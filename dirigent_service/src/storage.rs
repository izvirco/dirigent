use anyhow::{Context, Result};
use dirigent_agent::import;
use dirigent_protocol::transcript::{self, Body, Entry, Stop, Usage, text_of};
use dirigent_protocol::{
    Ask, ForkedFrom, Notify, Project, ProxySample, Report, SearchHit, Spend, ThreadSummary, UsageBucket, prompt_title,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    origin TEXT,
    created_at INTEGER NOT NULL,
    deleted_at INTEGER
);
CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY,
    project_id TEXT,
    cwd TEXT NOT NULL,
    workspace TEXT,
    name TEXT,
    renamed INTEGER NOT NULL DEFAULT 0,
    model TEXT,
    effort TEXT,
    tools TEXT,
    parent_id TEXT,
    report TEXT,
    notify TEXT,
    forked_from TEXT,
    forked_at TEXT,
    locked INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    prompted_at INTEGER,
    last_run_ms INTEGER,
    archived_at INTEGER,
    pinned INTEGER NOT NULL DEFAULT 0,
    snoozed_at INTEGER,
    seen_at INTEGER,
    spend_cost REAL NOT NULL DEFAULT 0,
    spend_input INTEGER NOT NULL DEFAULT 0,
    spend_output INTEGER NOT NULL DEFAULT 0,
    spend_cached INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS entries (
    thread_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    id TEXT NOT NULL,
    parent_id TEXT,
    at INTEGER NOT NULL,
    kind TEXT NOT NULL,
    json TEXT NOT NULL,
    PRIMARY KEY (thread_id, seq)
);
CREATE TABLE IF NOT EXISTS blobs (
    id TEXT PRIMARY KEY,
    mime TEXT NOT NULL,
    bytes BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS usage (
    at INTEGER NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input INTEGER NOT NULL,
    output INTEGER NOT NULL,
    cache_read INTEGER NOT NULL,
    cache_write INTEGER NOT NULL,
    cost REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS usage_at ON usage (at);
CREATE TABLE IF NOT EXISTS asks (
    thread_id TEXT NOT NULL,
    id TEXT NOT NULL,
    json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (thread_id, id)
);
CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5 (thread_id UNINDEXED, text);
CREATE TABLE IF NOT EXISTS message_pins (
    thread_id TEXT NOT NULL,
    entry_id TEXT NOT NULL,
    PRIMARY KEY (thread_id, entry_id)
);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS api_keys (
    name TEXT PRIMARY KEY,
    hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS proxy_samples (
    at INTEGER NOT NULL,
    account TEXT NOT NULL,
    name TEXT NOT NULL,
    used REAL NOT NULL,
    PRIMARY KEY (at, account, name)
) WITHOUT ROWID;
";

pub struct Storage {
    conn: Connection,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare("SELECT id, path, origin, created_at FROM projects WHERE deleted_at IS NULL ORDER BY path")?;
        let rows = stmt.query_map([], |r| Ok(Project { id: r.get(0)?, path: r.get(1)?, origin: r.get(2)?, created_at: r.get(3)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // MUST KILL: rename: add_project returns the existing project for a known path; call it add_or_get_project
    pub fn add_project(&self, id: &str, path: &str, origin: Option<&str>, now: i64) -> Result<Project> {
        // `C:\Work\x` and `c:\work\x` are one directory on Windows.
        let same = if cfg!(windows) { "path = ?1 COLLATE NOCASE" } else { "path = ?1" };
        let existing = self
            .conn
            .query_row(
                &format!(
                    "SELECT id, path, origin, created_at FROM projects WHERE deleted_at IS NULL AND {same} ORDER BY created_at LIMIT 1"
                ),
                [path],
                |r| Ok(Project { id: r.get(0)?, path: r.get(1)?, origin: r.get(2)?, created_at: r.get(3)? }),
            )
            .optional()?;
        if let Some(project) = existing {
            return Ok(project);
        }
        self.conn.execute("INSERT INTO projects (id, path, origin, created_at) VALUES (?1, ?2, ?3, ?4)", params![id, path, origin, now])?;
        Ok(Project { id: id.into(), path: path.into(), origin: origin.map(str::to_owned), created_at: now })
    }

    pub fn remove_project(&self, id: &str, now: i64) -> Result<()> {
        self.conn.execute("UPDATE projects SET deleted_at = ?2 WHERE id = ?1", params![id, now])?;
        Ok(())
    }

    pub fn project_path(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT path FROM projects WHERE id = ?1 AND deleted_at IS NULL", [id], |r| r.get(0)).optional()?)
    }

    // MUST KILL: rename: thread_project_path → thread_project_path_incl_deleted
    pub fn thread_project_path(&self, thread_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT p.path FROM threads t JOIN projects p ON p.id = t.project_id WHERE t.id = ?1", [thread_id], |r| r.get(0))
            .optional()?)
    }

    pub fn set_cwd(&self, id: &str, cwd: &str, workspace: Option<&str>) -> Result<()> {
        let n = self.conn.execute("UPDATE threads SET cwd = ?2, workspace = ?3 WHERE id = ?1", params![id, cwd, workspace])?;
        anyhow::ensure!(n == 1, "no such thread");
        Ok(())
    }

    pub fn thread_workspace(&self, id: &str) -> Result<Option<String>> {
        let ws: Option<Option<String>> =
            self.conn.query_row("SELECT workspace FROM threads WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(ws.flatten())
    }

    pub fn last_entry_id(&self, thread_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM entries WHERE thread_id = ?1 ORDER BY seq DESC LIMIT 1", [thread_id], |r| r.get(0))
            .optional()?)
    }

    pub fn list_threads(&self) -> Result<Vec<ThreadSummary>> {
        let mut asks: HashMap<String, Vec<Ask>> = HashMap::new();
        let mut stmt = self.conn.prepare("SELECT thread_id, json FROM asks ORDER BY created_at, rowid")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (thread_id, json) = row?;
            if let Ok(ask) = serde_json::from_str(&json) {
                asks.entry(thread_id).or_default().push(ask);
            }
        }
        let mut pins: HashMap<String, Vec<String>> = HashMap::new();
        let mut stmt = self.conn.prepare("SELECT thread_id, entry_id FROM message_pins ORDER BY rowid")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (thread_id, entry_id) = row?;
            pins.entry(thread_id).or_default().push(entry_id);
        }
        let mut stmt = self.conn.prepare(
            "SELECT t.id, p.id, t.cwd, t.name, t.model, t.effort, t.created_at, t.updated_at,
                    (SELECT COUNT(*) FROM entries e WHERE e.thread_id = t.id), t.archived_at IS NOT NULL, t.last_run_ms, t.workspace,
                    t.parent_id, t.snoozed_at IS NOT NULL AND t.updated_at <= t.snoozed_at, t.prompted_at, t.pinned, t.seen_at,
                    t.spend_cost, t.spend_input, t.spend_output, t.spend_cached, t.forked_from, t.forked_at, t.locked
             FROM threads t LEFT JOIN projects p ON p.id = t.project_id AND p.deleted_at IS NULL
             ORDER BY t.updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            let id: String = r.get(0)?;
            Ok(ThreadSummary {
                asks: asks.remove(&id).unwrap_or_default(),
                pinned_entries: pins.remove(&id).unwrap_or_default(),
                id,
                project_id: r.get(1)?,
                cwd: r.get(2)?,
                name: r.get(3)?,
                model: r.get(4)?,
                effort: r.get(5)?,
                created_at: r.get(6)?,
                updated_at: r.get(7)?,
                entry_count: r.get::<_, i64>(8)? as u64,
                archived: r.get(9)?,
                last_run_ms: r.get(10)?,
                workspace: r.get(11)?,
                parent_id: r.get(12)?,
                snoozed: r.get(13)?,
                prompted_at: r.get(14)?,
                pinned: r.get(15)?,
                seen_at: r.get(16)?,
                spend: Spend {
                    cost: r.get(17)?,
                    input: r.get::<_, i64>(18)? as u64,
                    output: r.get::<_, i64>(19)? as u64,
                    cached: r.get::<_, i64>(20)? as u64,
                },
                forked_from: r
                    .get::<_, Option<String>>(21)?
                    .zip(r.get::<_, Option<String>>(22)?)
                    .map(|(thread_id, entry_id)| ForkedFrom { thread_id, entry_id }),
                locked: r.get(23)?,
                ..Default::default()
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_tools(&self, id: &str, tools: &[String]) -> Result<()> {
        self.conn.execute("UPDATE threads SET tools = ?2 WHERE id = ?1", params![id, serde_json::to_string(tools)?])?;
        Ok(())
    }

    pub fn thread_tools(&self, id: &str) -> Result<Vec<String>> {
        let json: Option<Option<String>> = self.conn.query_row("SELECT tools FROM threads WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(json.flatten().and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default())
    }

    pub fn add_ask(&self, thread_id: &str, ask: &Ask, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO asks (thread_id, id, json, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![thread_id, ask.id, serde_json::to_string(ask)?, now],
        )?;
        Ok(())
    }

    pub fn take_ask(&self, thread_id: &str, ask_id: &str) -> Result<Option<Ask>> {
        let json: Option<String> = self
            .conn
            .query_row("SELECT json FROM asks WHERE thread_id = ?1 AND id = ?2", params![thread_id, ask_id], |r| r.get(0))
            .optional()?;
        self.conn.execute("DELETE FROM asks WHERE thread_id = ?1 AND id = ?2", params![thread_id, ask_id])?;
        Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    // MUST KILL: extract: set_prompted also names an unnamed thread after the prompt; split that out
    pub fn set_prompted(&self, id: &str, text: &str, model: Option<&str>, effort: Option<&str>, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE threads SET prompted_at = ?2, name = COALESCE(name, ?3), model = COALESCE(?4, model), effort = COALESCE(?5, effort)
             WHERE id = ?1",
            params![id, now, prompt_title(text), model, effort],
        )?;
        Ok(())
    }

    pub fn set_model(&self, id: &str, model: Option<&str>, effort: Option<&str>) -> Result<()> {
        self.conn.execute("UPDATE threads SET model = ?2, effort = ?3 WHERE id = ?1", params![id, model, effort])?;
        Ok(())
    }

    pub fn set_last_run(&self, id: &str, ms: i64) -> Result<()> {
        self.conn.execute("UPDATE threads SET last_run_ms = ?2 WHERE id = ?1", params![id, ms])?;
        Ok(())
    }

    pub fn set_archived(&self, id: &str, archived: bool, now: i64) -> Result<()> {
        let n = self.conn.execute("UPDATE threads SET archived_at = ?2 WHERE id = ?1", params![id, archived.then_some(now)])?;
        anyhow::ensure!(n == 1, "no such thread");
        Ok(())
    }

    pub fn unarchive(&self, id: &str) -> Result<bool> {
        let n = self.conn.execute("UPDATE threads SET archived_at = NULL WHERE id = ?1 AND archived_at IS NOT NULL", [id])?;
        Ok(n == 1)
    }

    pub fn set_snoozed(&self, id: &str, snoozed: bool) -> Result<()> {
        let sql = if snoozed {
            "UPDATE threads SET snoozed_at = updated_at WHERE id = ?1"
        } else {
            "UPDATE threads SET snoozed_at = NULL WHERE id = ?1"
        };
        let n = self.conn.execute(sql, [id])?;
        anyhow::ensure!(n == 1, "no such thread");
        Ok(())
    }

    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        let n = self.conn.execute("UPDATE threads SET pinned = ?2 WHERE id = ?1", params![id, pinned])?;
        anyhow::ensure!(n == 1, "no such thread");
        Ok(())
    }

    pub fn set_name(&self, id: &str, name: &str) -> Result<()> {
        let n = self.conn.execute("UPDATE threads SET name = ?2, renamed = 1 WHERE id = ?1", params![id, name])?;
        anyhow::ensure!(n == 1, "no such thread");
        Ok(())
    }

    // MUST KILL: rename: set_title → set_title_unless_renamed
    pub fn set_title(&self, id: &str, title: &str) -> Result<()> {
        self.conn.execute("UPDATE threads SET name = ?2 WHERE id = ?1 AND renamed = 0", params![id, title])?;
        Ok(())
    }

    pub fn set_message_pin(&self, thread_id: &str, entry_id: &str, pinned: bool) -> Result<()> {
        anyhow::ensure!(self.thread_cwd(thread_id)?.is_some(), "no such thread");
        let sql = if pinned {
            "INSERT OR IGNORE INTO message_pins (thread_id, entry_id) VALUES (?1, ?2)"
        } else {
            "DELETE FROM message_pins WHERE thread_id = ?1 AND entry_id = ?2"
        };
        self.conn.execute(sql, params![thread_id, entry_id])?;
        Ok(())
    }

    pub fn set_seen(&self, id: &str, updated_at: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("UPDATE threads SET seen_at = ?2 WHERE id = ?1 AND (seen_at IS NULL OR seen_at < ?2)", params![id, updated_at])?;
        Ok(n == 1)
    }

    pub fn thread_parent(&self, id: &str) -> Result<Option<String>> {
        let p: Option<Option<String>> =
            self.conn.query_row("SELECT parent_id FROM threads WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(p.flatten())
    }

    pub fn thread_locked(&self, id: &str) -> Result<bool> {
        Ok(self.conn.query_row("SELECT locked FROM threads WHERE id = ?1", [id], |r| r.get(0)).optional()?.unwrap_or(false))
    }

    pub fn set_report(&self, id: &str, report: Report, notify: Notify) -> Result<()> {
        let name = |v: Value| v.as_str().map(str::to_owned);
        self.conn.execute(
            "UPDATE threads SET report = ?2, notify = ?3 WHERE id = ?1",
            params![id, name(serde_json::to_value(report)?), name(serde_json::to_value(notify)?)],
        )?;
        Ok(())
    }

    pub fn report(&self, id: &str) -> Result<(Report, Notify)> {
        let (report, notify): (Option<String>, Option<String>) =
            self.conn.query_row("SELECT report, notify FROM threads WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        fn parse<T: serde::de::DeserializeOwned + Default>(s: Option<String>) -> T {
            s.and_then(|s| serde_json::from_value(Value::String(s)).ok()).unwrap_or_default()
        }
        Ok((parse(report), parse(notify)))
    }

    pub fn family(&self, id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE f(id) AS (SELECT ?1 UNION SELECT t.id FROM threads t JOIN f ON t.parent_id = f.id) SELECT id FROM f",
        )?;
        Ok(stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn depth(&self, id: &str) -> Result<usize> {
        let mut depth = 0;
        let mut cur = self.thread_parent(id)?;
        while let Some(p) = cur {
            depth += 1;
            if depth > 64 {
                break;
            }
            cur = self.thread_parent(&p)?;
        }
        Ok(depth)
    }

    pub fn last_reply(&self, thread_id: &str) -> Result<Option<String>> {
        let Some(entries) = self.get_thread(thread_id)? else { return Ok(None) };
        let reply = current_branch(&entries).into_iter().rev().take_while(|e| !matches!(e.body, Body::User { .. })).find_map(final_reply);
        Ok(reply.filter(|text| !text.trim().is_empty()))
    }

    pub fn conversation(&self, thread_id: &str) -> Result<(Vec<String>, Option<String>)> {
        let entries = self.get_thread(thread_id)?.ok_or_else(|| anyhow::anyhow!("no such thread"))?;
        let branch = current_branch(&entries);
        let said = |text: &String| !text.trim().is_empty();
        let prompts = branch.iter().filter_map(|e| match &e.body {
            Body::User { content } => Some(text_of(content)).filter(said),
            _ => None,
        });
        let reply = branch.iter().rev().copied().find_map(final_reply).filter(said);
        Ok((prompts.collect(), reply))
    }

    pub fn search(&self, query: &str, limit: u64) -> Result<Vec<SearchHit>> {
        // Quote each word so punctuation in the query can't break FTS5's syntax.
        let terms: Vec<String> = query.split_whitespace().map(|w| format!("\"{}\"*", w.replace('"', "\"\""))).collect();
        if terms.is_empty() {
            return Ok(vec![]);
        }
        // `snippet` can't be aggregated, so the best rows come out message by message and
        // the first one per thread wins.
        let mut stmt = self.conn.prepare(
            "SELECT thread_id, snippet(entries_fts, 1, '', '', '\u{2026}', 16)
             FROM entries_fts WHERE entries_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![terms.join(" "), (limit * 20) as i64], |r| {
            Ok(SearchHit { thread_id: r.get(0)?, snippet: r.get::<_, String>(1)?.split_whitespace().collect::<Vec<_>>().join(" ") })
        })?;
        let mut hits: Vec<SearchHit> = vec![];
        for hit in rows {
            let hit = hit?;
            if !hits.iter().any(|h| h.thread_id == hit.thread_id) {
                hits.push(hit);
            }
            if hits.len() as u64 >= limit {
                break;
            }
        }
        Ok(hits)
    }

    pub fn delete_thread(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM entries WHERE thread_id = ?1", [id])?;
        self.conn.execute("DELETE FROM entries_fts WHERE thread_id = ?1", [id])?;
        self.conn.execute("DELETE FROM asks WHERE thread_id = ?1", [id])?;
        self.conn.execute("DELETE FROM message_pins WHERE thread_id = ?1", [id])?;
        let n = self.conn.execute("DELETE FROM threads WHERE id = ?1", [id])?;
        anyhow::ensure!(n == 1, "no such thread");
        Ok(())
    }

    pub fn create_thread(
        &self,
        id: &str,
        project_id: &str,
        cwd: &str,
        parent: Option<&str>,
        name: Option<&str>,
        now: i64,
    ) -> Result<ThreadSummary> {
        self.conn.execute(
            "INSERT INTO threads (id, project_id, cwd, parent_id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, project_id, cwd, parent, name, now],
        )?;
        Ok(ThreadSummary {
            id: id.into(),
            project_id: Some(project_id.into()),
            cwd: cwd.into(),
            name: name.map(str::to_owned),
            created_at: now,
            updated_at: now,
            parent_id: parent.map(str::to_owned),
            ..Default::default()
        })
    }

    pub fn fork_thread(&self, src: &str, entry_id: &str, id: &str, now: i64) -> Result<ThreadSummary> {
        let entries = self.get_thread(src)?.ok_or_else(|| anyhow::anyhow!("no such thread"))?;
        let branch = transcript::branch(&entries, entry_id);
        anyhow::ensure!(!branch.is_empty(), "no such entry in the thread");
        let unread = branch.iter().any(|e| e.body == Body::Unknown);
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO threads (id, project_id, cwd, name, model, effort, tools, locked, created_at, updated_at, forked_from, forked_at)
             SELECT ?1, project_id, cwd, name, model, effort, tools, locked AND ?2, ?3, ?3, id, ?4 FROM threads WHERE id = ?5",
            params![id, unread, now, entry_id, src],
        )?;
        for (seq, e) in branch.iter().enumerate() {
            tx.execute(
                "INSERT INTO entries (thread_id, seq, id, parent_id, at, kind, json)
                 SELECT ?1, ?2, id, parent_id, at, kind, json FROM entries WHERE thread_id = ?3 AND seq = ?4",
                params![id, seq as i64, src, e.seq as i64],
            )?;
            index_text(&tx, id, &e.body)?;
        }
        tx.commit()?;
        self.list_threads()?.into_iter().find(|t| t.id == id).ok_or_else(|| anyhow::anyhow!("fork vanished"))
    }

    pub fn is_user_message(&self, thread_id: &str, entry_id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM entries WHERE thread_id = ?1 AND id = ?2 AND kind = 'user')",
            params![thread_id, entry_id],
            |r| r.get(0),
        )?)
    }

    pub fn thread_model(&self, id: &str) -> Result<(Option<String>, Option<String>)> {
        let row = self.conn.query_row("SELECT model, effort FROM threads WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)));
        Ok(row.optional()?.unwrap_or_default())
    }

    pub fn thread_cwd(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT cwd FROM threads WHERE id = ?1", [id], |r| r.get(0)).optional()?)
    }

    pub fn get_thread(&self, id: &str) -> Result<Option<Vec<Entry>>> {
        if self.thread_cwd(id)?.is_none() {
            return Ok(None);
        }
        let mut stmt = self.conn.prepare("SELECT seq, id, parent_id, at, json FROM entries WHERE thread_id = ?1 ORDER BY seq")?;
        let entries = stmt.query_map([id], |r| {
            Ok(Entry {
                seq: r.get::<_, i64>(0)? as u64,
                id: r.get(1)?,
                parent_id: r.get(2)?,
                at: r.get(3)?,
                body: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or(Body::Unknown),
            })
        })?;
        Ok(Some(entries.collect::<rusqlite::Result<_>>()?))
    }

    pub fn append_entry(&self, thread_id: &str, mut entry: Entry, now: i64) -> Result<Entry> {
        let tx = self.conn.unchecked_transaction()?;
        let exists: bool = tx.query_row("SELECT EXISTS (SELECT 1 FROM threads WHERE id = ?1)", [thread_id], |r| r.get(0))?;
        anyhow::ensure!(exists, "no such thread");
        let seq: i64 = tx.query_row("SELECT COALESCE(MAX(seq) + 1, 0) FROM entries WHERE thread_id = ?1", [thread_id], |r| r.get(0))?;
        entry.seq = seq as u64;
        insert_entry(&tx, thread_id, &entry)?;
        index_text(&tx, thread_id, &entry.body)?;
        if let Some((provider, model, usage)) = used(&entry.body) {
            add_usage(&tx, entry.at, &provider, model, usage)?;
            add_spend(&tx, thread_id, usage)?;
        }
        // MUST KILL: extract: a moves_updated_at(&Body) helper instead of the inline Store exception to the updated_at bump
        if !matches!(entry.body, Body::Store { .. }) {
            tx.execute("UPDATE threads SET updated_at = ?2 WHERE id = ?1", params![thread_id, now])?;
        }
        tx.commit()?;
        Ok(entry)
    }

    pub fn add_key(&self, name: &str, hash: &str, at: i64) -> Result<()> {
        anyhow::ensure!(!name.trim().is_empty(), "a key needs a name");
        let added =
            self.conn.execute("INSERT OR IGNORE INTO api_keys (name, hash, created_at) VALUES (?1, ?2, ?3)", params![name, hash, at])?;
        anyhow::ensure!(added == 1, "a key named {name:?} exists; revoke it first");
        Ok(())
    }

    pub fn key_name(&self, hash: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT name FROM api_keys WHERE hash = ?1", [hash], |r| r.get(0)).optional()?)
    }

    pub fn keys(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare("SELECT name, created_at FROM api_keys ORDER BY created_at")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn revoke_key(&self, name: &str) -> Result<bool> {
        Ok(self.conn.execute("DELETE FROM api_keys WHERE name = ?1", [name])? == 1)
    }

    pub fn service_id(&self) -> Result<String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        self.conn.execute("INSERT OR IGNORE INTO meta (key, value) VALUES ('service_id', ?1)", [&id])?;
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key = 'service_id'", [], |r| r.get(0))?)
    }

    pub fn add_proxy_samples(&self, at: i64, samples: &[(&str, &str, f64)], keep_from: i64) -> Result<()> {
        let mut add = self.conn.prepare_cached("INSERT OR REPLACE INTO proxy_samples (at, account, name, used) VALUES (?1, ?2, ?3, ?4)")?;
        for (account, name, used) in samples {
            add.execute(params![at, account, name, used])?;
        }
        self.conn.execute("DELETE FROM proxy_samples WHERE at < ?1", [keep_from])?;
        Ok(())
    }

    pub fn proxy_samples(&self, since: i64, until: i64, bucket: i64) -> Result<Vec<ProxySample>> {
        anyhow::ensure!(bucket > 0, "bucket must be positive");
        let mut stmt = self.conn.prepare(
            "SELECT ?1 + ((at - ?1) / ?3) * ?3 AS start, account, name, MAX(used)
             FROM proxy_samples WHERE at >= ?1 AND at < ?2
             GROUP BY start, account, name ORDER BY start",
        )?;
        let rows = stmt.query_map(params![since, until, bucket], |r| {
            Ok(ProxySample { start: r.get(0)?, account: r.get(1)?, limit: r.get(2)?, used: r.get(3)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn usage(&self, since: i64, until: i64, bucket: i64) -> Result<Vec<UsageBucket>> {
        anyhow::ensure!(bucket > 0, "bucket must be positive");
        let mut stmt = self.conn.prepare(
            "SELECT ?1 + ((at - ?1) / ?3) * ?3 AS start, provider, model,
                    SUM(input), SUM(output), SUM(cache_read), SUM(cache_write), SUM(cost), COUNT(*)
             FROM usage WHERE at >= ?1 AND at < ?2
             GROUP BY start, provider, model ORDER BY start",
        )?;
        let rows = stmt.query_map(params![since, until, bucket], |r| {
            Ok(UsageBucket {
                start: r.get(0)?,
                provider: r.get(1)?,
                model: r.get(2)?,
                input: r.get::<_, i64>(3)? as u64,
                output: r.get::<_, i64>(4)? as u64,
                cache_read: r.get::<_, i64>(5)? as u64,
                cache_write: r.get::<_, i64>(6)? as u64,
                cost: r.get(7)?,
                messages: r.get::<_, i64>(8)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn put_blob(&self, mime: &str, bytes: &[u8]) -> Result<String> {
        let id = format!("{:x}", Sha256::digest(bytes));
        self.conn.execute("INSERT OR IGNORE INTO blobs (id, mime, bytes) VALUES (?1, ?2, ?3)", params![id, mime, bytes])?;
        Ok(id)
    }

    pub fn blob(&self, id: &str) -> Result<Option<(String, Vec<u8>)>> {
        Ok(self.conn.query_row("SELECT mime, bytes FROM blobs WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }
}

fn current_branch(entries: &[Entry]) -> Vec<&Entry> {
    entries.last().map(|last| transcript::branch(entries, &last.id)).unwrap_or_default()
}

fn final_reply(entry: &Entry) -> Option<String> {
    match &entry.body {
        Body::Assistant(a) if matches!(a.stop, Stop::End | Stop::MaxTokens) => Some(text_of(&a.content)),
        _ => None,
    }
}

fn insert_entry(conn: &Connection, thread_id: &str, entry: &Entry) -> Result<()> {
    let json = serde_json::to_value(&entry.body)?;
    let kind = json.get("type").and_then(Value::as_str).unwrap_or_default();
    conn.prepare_cached("INSERT INTO entries (thread_id, seq, id, parent_id, at, kind, json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)")?
        .execute(params![thread_id, entry.seq as i64, entry.id, entry.parent_id, entry.at, kind, json.to_string()])?;
    Ok(())
}

fn index_text(conn: &Connection, thread_id: &str, body: &Body) -> Result<()> {
    let text = match body {
        Body::User { content } => text_of(content),
        Body::Assistant(a) => text_of(&a.content),
        _ => return Ok(()),
    };
    if !text.trim().is_empty() {
        conn.prepare_cached("INSERT INTO entries_fts (thread_id, text) VALUES (?1, ?2)")?.execute(params![thread_id, text])?;
    }
    Ok(())
}

fn used(body: &Body) -> Option<(String, &str, &Usage)> {
    match body {
        // MUST KILL: type: agent-closed turns are told by empty provider and zero usage; give them a marker of their own
        Body::Assistant(a) if a.provider.is_empty() && a.usage == Usage::default() => None,
        Body::Assistant(a) => Some((a.provider.clone(), &a.model, &a.usage)),
        Body::Compaction { model, usage, .. } => Some((import::provider(model, ""), model, usage)),
        _ => None,
    }
}

fn add_usage(conn: &Connection, at: i64, provider: &str, model: &str, usage: &Usage) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO usage (at, provider, model, input, output, cache_read, cache_write, cost) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?
    .execute(params![
        at,
        provider,
        model,
        usage.input as i64,
        usage.output as i64,
        usage.cache_read as i64,
        usage.cache_write as i64,
        usage.cost.total()
    ])?;
    Ok(())
}

fn add_spend(conn: &Connection, thread_id: &str, usage: &Usage) -> Result<()> {
    conn.prepare_cached(
        "UPDATE threads SET spend_cost = spend_cost + ?2, spend_input = spend_input + ?3, spend_output = spend_output + ?4,
                            spend_cached = spend_cached + ?5
         WHERE id = ?1",
    )?
    .execute(params![thread_id, usage.cost.total(), usage.prompt() as i64, usage.output as i64, usage.cache_read as i64])?;
    Ok(())
}

#[derive(Debug, Default, PartialEq)]
pub struct Migration {
    pub threads: usize,
    pub entries: usize,
    pub kept: usize,
    pub dropped: BTreeMap<String, usize>,
    pub locked: Vec<(String, Vec<String>)>,
}

pub fn from_pi(db: &Path) -> Result<bool> {
    if !db.exists() {
        return Ok(false);
    }
    let conn = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(columns(&conn, "main", "threads")?.iter().any(|column| column == "thinking_level"))
}

/// Make the file alone all of `db`, with nobody else on it: for a copy of it, and before
/// it is replaced. Leaving sqlite's WAL mode does both: its log (of a service that was
/// killed) is folded into the file, and sqlite refuses while another connection is open.
/// A Dirigent that runs on the db would go on writing into a file that is no longer there.
pub fn settle(db: &Path) -> Result<()> {
    let conn = Connection::open(db)?;
    conn.busy_timeout(std::time::Duration::ZERO)?;
    let left = match conn.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get::<_, String>(0)) {
        Ok(mode) => mode == "delete",
        Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::DatabaseBusy => false,
        Err(e) => return Err(e.into()),
    };
    anyhow::ensure!(left, "{} is in use: is another Dirigent running on it?", db.display());
    conn.close().map_err(|(_, e)| e)?;
    Ok(())
}

pub fn migrate(db: &Path, progress: impl FnMut(usize, usize)) -> Result<Option<Migration>> {
    if !from_pi(db)? {
        return Ok(None);
    }
    let beside = |suffix: &str| {
        let mut name = db.as_os_str().to_owned();
        name.push(suffix);
        std::path::PathBuf::from(name)
    };
    let building = beside(".migrating");
    let _ = std::fs::remove_file(&building);
    let built = build(db, &building, progress).and_then(|migration| {
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(beside(suffix));
        }
        std::fs::rename(&building, db)?;
        Ok(migration)
    });
    if built.is_err() {
        let _ = std::fs::remove_file(&building);
    }
    Ok(Some(built.with_context(|| format!("migrate {}", db.display()))?))
}

fn build(old: &Path, new: &Path, mut progress: impl FnMut(usize, usize)) -> Result<Migration> {
    let conn = Connection::open(new)?;
    conn.pragma_update(None, "journal_mode", "OFF")?;
    conn.execute_batch(SCHEMA)?;
    conn.execute("ATTACH DATABASE ?1 AS old", [read_only_uri(old)])?;
    conn.execute_batch("BEGIN")?;
    for table in ["projects", "threads", "blobs", "usage", "asks", "message_pins", "meta", "api_keys", "proxy_samples"] {
        let theirs = columns(&conn, "old", table)?;
        let both: Vec<String> = columns(&conn, "main", table)?.into_iter().filter(|c| theirs.contains(c)).collect();
        if !both.is_empty() {
            let (both, order) = (both.join(", "), if table == "proxy_samples" { "at" } else { "rowid" });
            conn.execute(&format!("INSERT INTO main.{table} ({both}) SELECT {both} FROM old.{table} ORDER BY {order}"), [])?;
        }
    }
    let models: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT DISTINCT provider, model FROM main.usage")?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
    };
    for (provider, model) in models {
        conn.execute(
            "UPDATE main.usage SET provider = ?1 WHERE provider = ?2 AND model = ?3",
            params![import::provider(&model, &provider), provider, model],
        )?;
    }
    conn.execute("UPDATE main.threads SET spend_cost = 0, spend_input = 0, spend_output = 0, spend_cached = 0", [])?;
    let threads: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM main.threads")?;
        stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let mut migration = Migration::default();
    let mut row = conn
        .prepare("SELECT o.model, o.thinking_level, t.forked_at FROM main.threads t JOIN old.threads o ON o.id = t.id WHERE t.id = ?1")?;
    let mut read = conn.prepare("SELECT json FROM old.entries WHERE thread_id = ?1 ORDER BY seq")?;
    let total = threads.len();
    for id in threads {
        progress(migration.threads, total);
        let (model, effort, forked_at): (Option<String>, Option<String>, Option<String>) =
            row.query_row([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        let pi = read.query_map([&id], |r| r.get::<_, String>(0))?;
        let pi: Vec<Value> = pi.map(|json| Ok(serde_json::from_str(&json?).unwrap_or(Value::Null))).collect::<rusqlite::Result<_>>()?;
        let converted = import::entries(&pi);
        let forked = forked_at.as_deref().and_then(|at| pi.iter().position(|e| pi_id(e) == Some(at)));
        let theirs: HashSet<&str> = forked.map(|at| pi[..=at].iter().filter_map(pi_id).collect()).unwrap_or_default();
        let copied = converted.entries.iter().take_while(|e| theirs.contains(e.id.as_str())).count();
        if let Some(last) = copied.checked_sub(1).map(|last| &converted.entries[last]) {
            conn.execute("UPDATE main.threads SET forked_at = ?2 WHERE id = ?1", params![id, last.id])?;
        }
        let mut made_by = String::new();
        for (i, entry) in converted.entries.iter().enumerate() {
            insert_entry(&conn, &id, entry)?;
            index_text(&conn, &id, &entry.body)?;
            if let Body::Assistant(a) = &entry.body {
                made_by.clone_from(&a.provider);
            }
            if i < copied {
                continue;
            }
            match &entry.body {
                Body::Assistant(a) => add_spend(&conn, &id, &a.usage)?,
                Body::Compaction { model, usage, .. } if *usage != Usage::default() => {
                    add_usage(&conn, entry.at, &import::provider(model, &made_by), model, usage)?;
                    add_spend(&conn, &id, usage)?;
                }
                _ => {}
            }
        }
        let model = model.or(converted.model).map(|m| import::model_id(&m));
        conn.execute(
            "UPDATE main.threads SET model = ?2, effort = ?3, locked = ?4 WHERE id = ?1",
            params![id, model, effort.or(converted.effort), !converted.unreadable.is_empty()],
        )?;
        migration.threads += 1;
        migration.entries += pi.len();
        migration.kept += converted.entries.len();
        for (what, n) in converted.dropped {
            *migration.dropped.entry(what).or_default() += n;
        }
        if !converted.unreadable.is_empty() {
            let name: Option<String> = conn.query_row("SELECT name FROM main.threads WHERE id = ?1", [&id], |r| r.get(0))?;
            migration.locked.push((name.unwrap_or(id), converted.unreadable));
        }
    }
    progress(total, total);
    drop((row, read));
    conn.execute_batch("COMMIT")?;
    conn.close().map_err(|(_, e)| e)?;
    Ok(migration)
}

fn pi_id(entry: &Value) -> Option<&str> {
    entry.get("id").and_then(Value::as_str)
}

fn columns(conn: &Connection, schema: &str, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA {schema}.table_info({table})"))?;
    Ok(stmt.query_map([], |r| r.get(1))?.collect::<rusqlite::Result<_>>()?)
}

/// `path` as a URI that sqlite opens read-only (sqlite.org/uri.html has the recipe).
fn read_only_uri(path: &Path) -> String {
    let mut path = path.to_string_lossy().replace('%', "%25").replace('?', "%3f").replace('#', "%23");
    if cfg!(windows) {
        path = path.replace('\\', "/");
        // A drive letter is no host name.
        if !path.starts_with('/') {
            path.insert(0, '/');
        }
    }
    format!("file:{path}?mode=ro")
}

#[cfg(test)]
pub(crate) const PI_SCHEMA: &str = "
    CREATE TABLE projects (id TEXT PRIMARY KEY, path TEXT NOT NULL, origin TEXT, created_at INTEGER NOT NULL, deleted_at INTEGER);
    CREATE TABLE threads (id TEXT PRIMARY KEY, project_id TEXT, cwd TEXT NOT NULL, name TEXT, header TEXT, model TEXT,
        thinking_level TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, last_run_ms INTEGER, archived_at INTEGER,
        run_id TEXT, tools TEXT, workspace TEXT, parent_id TEXT, pinned INTEGER NOT NULL DEFAULT 0,
        spend_cost REAL NOT NULL DEFAULT 0, spend_input INTEGER NOT NULL DEFAULT 0, spend_output INTEGER NOT NULL DEFAULT 0,
        spend_cached INTEGER NOT NULL DEFAULT 0, forked_from TEXT, forked_at TEXT);
    CREATE TABLE entries (thread_id TEXT NOT NULL, seq INTEGER NOT NULL, id TEXT NOT NULL, parent_id TEXT, kind TEXT NOT NULL,
        json TEXT NOT NULL, PRIMARY KEY (thread_id, seq));
    CREATE TABLE blobs (id TEXT PRIMARY KEY, mime TEXT NOT NULL, bytes BLOB NOT NULL);
    CREATE TABLE usage (at INTEGER NOT NULL, provider TEXT NOT NULL, model TEXT NOT NULL, input INTEGER NOT NULL,
        output INTEGER NOT NULL, cache_read INTEGER NOT NULL, cache_write INTEGER NOT NULL, cost REAL NOT NULL);
    CREATE TABLE asks (thread_id TEXT NOT NULL, id TEXT NOT NULL, json TEXT NOT NULL, created_at INTEGER NOT NULL,
        PRIMARY KEY (thread_id, id));
    CREATE VIRTUAL TABLE entries_fts USING fts5 (thread_id UNINDEXED, text);
    CREATE TABLE message_pins (thread_id TEXT NOT NULL, entry_id TEXT NOT NULL, PRIMARY KEY (thread_id, entry_id));
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE workflow_runs (id TEXT PRIMARY KEY);
";

#[cfg(test)]
mod tests {
    use super::*;
    use dirigent_protocol::Question;
    use dirigent_protocol::transcript::{Api, Assistant, Block, Cost, Notice, ToolResult};
    use serde_json::json;

    fn entry(id: &str, parent: Option<&str>, at: i64, body: Body) -> Entry {
        Entry { seq: 0, id: id.into(), parent_id: parent.map(str::to_owned), at, body }
    }

    fn user(id: &str, parent: Option<&str>, text: &str) -> Entry {
        entry(id, parent, 1, Body::User { content: vec![Block::text(text)] })
    }

    fn reply(id: &str, parent: &str, at: i64, stop: Stop, text: &str, input: u64, cost: f64) -> Entry {
        let usage = Usage { input, output: 10, cache_read: 100, cache_write: 5, cost: Cost { input: cost, ..Cost::default() } };
        let content = vec![Block::Thinking { text: "hm".into(), signature: None, redacted: false }, Block::text(text)];
        let a = Assistant {
            content,
            model: "m".into(),
            provider: "anthropic".into(),
            api: Api::Anthropic,
            effort: None,
            usage,
            stop,
            error: None,
            duration_ms: 0,
        };
        entry(id, Some(parent), at, Body::Assistant(a))
    }

    fn by_id(s: &Storage, id: &str) -> ThreadSummary {
        s.list_threads().unwrap().into_iter().find(|t| t.id == id).unwrap()
    }

    #[test]
    fn asks_and_tools_live_with_the_thread() {
        let s = Storage::in_memory().unwrap();
        s.create_thread("t1", "p", "/x", None, None, 0).unwrap();
        assert!(s.thread_tools("t1").unwrap().is_empty());
        s.set_tools("t1", &["ask_user".into()]).unwrap();
        assert_eq!(s.thread_tools("t1").unwrap(), ["ask_user"]);
        assert_eq!(s.report("t1").unwrap(), (Report::Reply, Notify::Asap));
        s.set_report("t1", Report::Done, Notify::WhenIdle).unwrap();
        assert_eq!(s.report("t1").unwrap(), (Report::Done, Notify::WhenIdle));
        let ask =
            Ask { id: "a1".into(), questions: vec![Question { text: "db?".into(), options: vec![], allow_custom: false, multi: false }] };
        s.add_ask("t1", &ask, 5).unwrap();
        assert_eq!(s.list_threads().unwrap()[0].asks, vec![ask.clone()]);
        assert_eq!(s.take_ask("t1", "a1").unwrap(), Some(ask));
        assert_eq!(s.take_ask("t1", "a1").unwrap(), None);
        assert!(!s.list_threads().unwrap()[0].asking());
    }

    #[test]
    fn adding_a_path_twice_keeps_one_project() {
        let s = Storage::in_memory().unwrap();
        let first = s.add_project("p1", "/x", None, 1).unwrap();
        assert_eq!(s.add_project("p2", "/x", None, 2).unwrap(), first);
        assert_eq!(s.list_projects().unwrap(), [first]);
        s.remove_project("p1", 3).unwrap();
        assert_eq!(s.add_project("p3", "/x", None, 4).unwrap().id, "p3");
    }

    #[test]
    fn roundtrip_thread_and_entries() {
        let s = Storage::in_memory().unwrap();
        s.add_project("p1", "/x", Some("github.com/a/b"), 9).unwrap();
        s.create_thread("t1", "p1", "/x", None, None, 10).unwrap();
        s.set_prompted("t1", "  hello  \nworld", Some("claude-opus-5-5"), Some("high"), 10).unwrap();
        assert_eq!(s.list_threads().unwrap()[0].name.as_deref(), Some("hello"));
        s.set_prompted("t1", "later", None, Some("low"), 10).unwrap();
        let blob = s.put_blob("image/png", &[1, 2, 3]).unwrap();
        assert_eq!(blob, "039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81");
        assert_eq!(s.put_blob("image/png", &[1, 2, 3]).unwrap(), blob);
        assert_eq!(s.blob(&blob).unwrap(), Some(("image/png".to_owned(), vec![1, 2, 3])));

        let image = Block::Image { mime: "image/png".into(), blob };
        let call =
            Block::ToolCall { id: "c".into(), name: "edit".into(), input: json!({"path":"a.rs","edits":[{"oldText":"x","newText":"y"}]}) };
        let calls = Assistant {
            content: vec![call],
            model: "claude-opus-5-5".into(),
            provider: "anthropic".into(),
            api: Api::Anthropic,
            effort: Some("high".into()),
            usage: Usage::default(),
            stop: Stop::ToolUse,
            error: None,
            duration_ms: 5,
        };
        let result =
            ToolResult { call_id: "c".into(), tool: "edit".into(), content: vec![Block::text("ok")], is_error: false, duration_ms: 3 };
        let appended = [
            entry("a", None, 11, Body::User { content: vec![Block::text("hello\nworld"), image] }),
            entry("b", Some("a"), 12, Body::Assistant(calls)),
            entry("c", Some("b"), 12, Body::ToolResult(result)),
            entry("d", Some("c"), 12, Body::Notice(Notice::Info { text: "moved".into() })),
        ];
        for (seq, e) in appended.iter().enumerate() {
            assert_eq!(s.append_entry("t1", e.clone(), 12).unwrap(), Entry { seq: seq as u64, ..e.clone() });
        }
        let entries = s.get_thread("t1").unwrap().unwrap();
        assert_eq!(entries.iter().map(|e| Entry { seq: 0, ..e.clone() }).collect::<Vec<_>>(), appended);
        let Body::Assistant(a) = &entries[1].body else { panic!("not a reply") };
        assert!(serde_json::to_string(&a.content).unwrap().contains(r#"{"path":"a.rs","edits":[{"oldText":"x","newText":"y"}]}"#));
        let kinds: Vec<String> = s
            .conn
            .prepare("SELECT kind FROM entries ORDER BY seq")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(kinds, ["user", "assistant", "tool_result", "notice"]);

        let threads = s.list_threads().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!((threads[0].model.as_deref(), threads[0].effort.as_deref()), (Some("claude-opus-5-5"), Some("low")));
        assert_eq!(threads[0].project_id.as_deref(), Some("p1"));
        assert_eq!((threads[0].entry_count, threads[0].updated_at, threads[0].locked), (4, 12, false));
        assert!(s.get_thread("nope").unwrap().is_none());
        s.set_title("t1", "Greeting").unwrap();
        assert_eq!(s.list_threads().unwrap()[0].name.as_deref(), Some("Greeting"));
        s.append_entry("t1", entry("w", Some("d"), 21, Body::Store { values: [("k".to_owned(), json!(1))].into() }), 21).unwrap();
        assert_eq!(s.list_threads().unwrap()[0].updated_at, 12);

        s.set_cwd("t1", "/ws/t1", Some("/ws/t1")).unwrap();
        assert_eq!(s.thread_workspace("t1").unwrap().as_deref(), Some("/ws/t1"));
        assert_eq!(s.list_threads().unwrap()[0].cwd, "/ws/t1");
        assert_eq!(s.last_entry_id("t1").unwrap().as_deref(), Some("w"));

        s.remove_project("p1", 13).unwrap();
        assert!(s.list_projects().unwrap().is_empty());
        assert_eq!(s.list_threads().unwrap()[0].project_id, None);
        assert_eq!(s.thread_project_path("t1").unwrap().as_deref(), Some("/x"));

        s.set_archived("t1", true, 14).unwrap();
        assert!(s.list_threads().unwrap()[0].archived);
        s.set_archived("t1", false, 15).unwrap();
        assert!(!s.list_threads().unwrap()[0].archived);
        assert!(!s.unarchive("t1").unwrap());
        s.set_archived("t1", true, 16).unwrap();
        assert!(s.unarchive("t1").unwrap());
        assert!(!s.list_threads().unwrap()[0].archived);
        s.delete_thread("t1").unwrap();
        assert!(s.list_threads().unwrap().is_empty());
        assert!(s.get_thread("t1").unwrap().is_none());
        assert!(s.delete_thread("t1").is_err());
    }

    #[test]
    fn fork_copies_a_branch() {
        let s = Storage::in_memory().unwrap();
        s.create_thread("src", "p", "/ws/src", None, Some("Bug hunt"), 1).unwrap();
        s.set_tools("src", &["ask_user".into()]).unwrap();
        s.set_prompted("src", "find the bug", Some("x"), Some("high"), 2).unwrap();
        s.append_entry("src", user("u1", None, "find the bug"), 3).unwrap();
        s.append_entry("src", reply("r1", "u1", 4, Stop::End, "found it", 5, 0.5), 4).unwrap();
        s.append_entry("src", user("u2", None, "find the other bug"), 5).unwrap();
        s.append_entry("src", reply("r2", "u2", 6, Stop::End, "found it", 5, 0.5), 6).unwrap();
        assert!(s.is_user_message("src", "u1").unwrap() && !s.is_user_message("src", "r1").unwrap());

        let fork = s.fork_thread("src", "r1", "f", 10).unwrap();
        assert_eq!(fork.forked_from, Some(ForkedFrom { thread_id: "src".into(), entry_id: "r1".into() }));
        assert_eq!((fork.cwd.as_str(), fork.name.as_deref(), fork.model.as_deref()), ("/ws/src", Some("Bug hunt"), Some("x")));
        assert_eq!((fork.parent_id, fork.workspace, fork.updated_at, fork.effort.as_deref()), (None, None, 10, Some("high")));
        assert_eq!(fork.spend, Spend::default());
        assert_eq!(s.thread_tools("f").unwrap(), ["ask_user"]);
        let source = s.get_thread("src").unwrap().unwrap();
        let entries = s.get_thread("f").unwrap().unwrap();
        assert_eq!(entries.iter().map(|e| (e.seq, e.id.as_str())).collect::<Vec<_>>(), [(0, "u1"), (1, "r1")]);
        assert_eq!(entries[1].body, source[1].body);
        assert_eq!(s.search("find bug", 10).unwrap().len(), 2);
        assert_eq!(s.usage(0, 100, 100).unwrap()[0].messages, 2);
        assert!(s.fork_thread("src", "nope", "g", 10).is_err());

        s.append_entry("src", entry("x", Some("r2"), 7, Body::Unknown), 7).unwrap();
        s.conn.execute("UPDATE threads SET locked = 1 WHERE id = 'src'", []).unwrap();
        assert!(s.thread_locked("src").unwrap() && by_id(&s, "src").locked && !s.thread_locked("f").unwrap());
        assert!(s.fork_thread("src", "x", "locked", 11).unwrap().locked);
        assert!(!s.fork_thread("src", "r2", "free", 11).unwrap().locked);
    }

    #[test]
    fn lineage_snooze_and_search() {
        let s = Storage::in_memory().unwrap();
        s.create_thread("root", "p", "/x", None, None, 1).unwrap();
        s.create_thread("kid", "p", "/x", Some("root"), Some("Review"), 2).unwrap();
        s.create_thread("grandkid", "p", "/x", Some("kid"), None, 3).unwrap();
        assert_eq!(s.depth("root").unwrap(), 0);
        assert_eq!(s.depth("grandkid").unwrap(), 2);
        assert_eq!(s.thread_parent("kid").unwrap().as_deref(), Some("root"));
        let mut family = s.family("root").unwrap();
        family.sort();
        assert_eq!(family, ["grandkid", "kid", "root"]);
        assert_eq!(s.family("grandkid").unwrap(), ["grandkid"]);

        s.set_snoozed("root", true).unwrap();
        assert!(by_id(&s, "root").snoozed);
        s.append_entry("root", user("u", None, "find the sqlite bug"), 50).unwrap();
        assert!(!by_id(&s, "root").snoozed);
        s.set_snoozed("root", true).unwrap();
        s.set_snoozed("root", false).unwrap();
        assert!(!by_id(&s, "root").snoozed);

        s.set_pinned("root", true).unwrap();
        s.set_message_pin("root", "u", true).unwrap();
        s.set_message_pin("root", "u", true).unwrap();
        assert!(s.set_message_pin("nope", "u", true).is_err());
        assert!(s.set_seen("root", 50).unwrap());
        assert!(!s.set_seen("root", 50).unwrap());
        assert!(!s.set_seen("root", 40).unwrap());
        let t = by_id(&s, "root");
        assert!(t.pinned && t.seen_at == Some(50) && t.pinned_entries == ["u"]);
        s.set_message_pin("root", "u", false).unwrap();
        assert!(by_id(&s, "root").pinned_entries.is_empty());

        s.append_entry("root", reply("a1", "u", 51, Stop::ToolUse, "looking at storage.rs", 0, 0.), 51).unwrap();
        s.append_entry("root", reply("a2", "a1", 52, Stop::End, "The bug is a missing index on usage.at", 0, 0.), 52).unwrap();
        s.append_entry("kid", user("k", None, "write docs"), 53).unwrap();
        assert_eq!(s.last_reply("root").unwrap().as_deref(), Some("The bug is a missing index on usage.at"));
        assert_eq!(s.last_reply("kid").unwrap(), None);
        assert_eq!(s.last_reply("nope").unwrap(), None);
        s.set_prompted("kid", "write docs", None, None, 60).unwrap();
        assert_eq!(by_id(&s, "kid").name.as_deref(), Some("Review"));
        assert_eq!(by_id(&s, "kid").prompted_at, Some(60));
        s.append_entry("root", user("u2", Some("a2"), "and again"), 54).unwrap();
        s.append_entry("root", reply("a3", "u2", 55, Stop::Aborted, "The bug is", 0, 0.), 55).unwrap();
        assert_eq!(s.last_reply("root").unwrap(), None);
        let (prompts, reply) = s.conversation("root").unwrap();
        assert_eq!(prompts, ["find the sqlite bug", "and again"]);
        assert_eq!(reply.as_deref(), Some("The bug is a missing index on usage.at"));
        s.append_entry("root", user("u3", Some("a2"), "once more"), 56).unwrap();
        assert_eq!(s.conversation("root").unwrap().0, ["find the sqlite bug", "once more"]);
        s.set_name("root", "Missing usage index").unwrap();
        assert_eq!(by_id(&s, "root").name.as_deref(), Some("Missing usage index"));
        s.set_title("root", "Sqlite bug").unwrap();
        assert_eq!(by_id(&s, "root").name.as_deref(), Some("Missing usage index"));
        assert!(s.set_name("nope", "x").is_err());
        let hits = s.search("sqlite", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].thread_id, "root");
        assert!(hits[0].snippet.contains("sqlite"));
        assert_eq!(s.search("docs", 10).unwrap()[0].thread_id, "kid");
        assert!(s.search("hm", 10).unwrap().is_empty());
        assert!(s.search("\"weird) syntax", 10).unwrap().is_empty());
        assert_eq!(s.search("", 10).unwrap().len(), 0);
        s.delete_thread("root").unwrap();
        assert!(s.search("index", 10).unwrap().is_empty());
        s.append_entry("root", user("late", None, "the index again"), 60).unwrap_err();
        assert!(s.search("index", 10).unwrap().is_empty());
    }

    #[test]
    fn service_id_is_made_once() {
        let s = Storage::in_memory().unwrap();
        let id = s.service_id().unwrap();
        assert_eq!(id.len(), 32);
        assert_eq!(s.service_id().unwrap(), id);
    }

    #[test]
    fn proxy_samples_keep_the_slots_peak() {
        let s = Storage::in_memory().unwrap();
        s.add_proxy_samples(100, &[("a", "5h", 0.2), ("a", "7d", 0.1)], 0).unwrap();
        s.add_proxy_samples(100, &[("a", "5h", 0.3)], 0).unwrap();
        s.add_proxy_samples(200, &[("a", "5h", 0.9)], 0).unwrap();
        s.add_proxy_samples(1100, &[("a", "5h", 0.)], 0).unwrap();
        let got = s.proxy_samples(0, 2000, 1000).unwrap();
        let row = |start: i64, limit: &str, used: f64| ProxySample { start, account: "a".into(), limit: limit.into(), used };
        assert_eq!(got, [row(0, "5h", 0.9), row(0, "7d", 0.1), row(1000, "5h", 0.)]);
        s.add_proxy_samples(1200, &[("a", "5h", 0.5)], 1000).unwrap();
        assert_eq!(s.proxy_samples(0, 2000, 1000).unwrap(), [row(1000, "5h", 0.5)]);
    }

    #[test]
    fn usage_is_counted_as_entries_are_appended() {
        let s = Storage::in_memory().unwrap();
        s.create_thread("t1", "p", "/x", None, None, 0).unwrap();
        s.append_entry("t1", user("u", None, "hi"), 1).unwrap();
        s.append_entry("t1", reply("a", "u", 1000, Stop::ToolUse, "", 1, 0.5), 1).unwrap();
        s.append_entry("t1", reply("b", "a", 1500, Stop::ToolUse, "", 2, 0.25), 1).unwrap();
        let usage = Usage { input: 4, output: 10, cache_read: 100, cache_write: 5, cost: Cost { output: 1.0, ..Cost::default() } };
        let compaction =
            Body::Compaction { summary: "so far".into(), first_kept: "b".into(), tokens_before: 9, model: "claude-x".into(), usage };
        s.append_entry("t1", entry("c", Some("b"), 2500, compaction), 1).unwrap();
        s.append_entry("t1", reply("d", "c", 9000, Stop::End, "done", 8, 1.0), 1).unwrap();
        let mut closed = reply("e", "d", 2600, Stop::Aborted, "", 0, 0.);
        if let Body::Assistant(a) = &mut closed.body {
            (a.provider, a.usage) = (String::new(), Usage::default());
        }
        s.append_entry("t1", closed, 1).unwrap();
        let b = s.usage(0, 5000, 1000).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].start, b[0].provider.as_str(), b[0].model.as_str()), (1000, "anthropic", "m"));
        assert_eq!((b[0].input, b[0].cost, b[0].messages), (3, 0.75, 2));
        assert_eq!(
            (b[1].start, b[1].provider.as_str(), b[1].model.as_str(), b[1].input, b[1].cache_read),
            (2000, "anthropic", "claude-x", 4, 100)
        );
        let want = Spend { input: 15 + 4 * 105, output: 40, cached: 400, cost: 2.75 };
        assert_eq!(s.list_threads().unwrap()[0].spend, want);
        s.delete_thread("t1").unwrap();
        assert_eq!(s.usage(0, 10_000, 10_000).unwrap().iter().map(|b| b.messages).sum::<u64>(), 4);
    }

    #[test]
    fn a_service_db_is_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path().join("50% #1?");
        std::fs::create_dir(&dir).unwrap();
        let live = dir.join("service.db");
        assert_eq!(migrate(&live, |_, _| {}).unwrap(), None);
        assert!(!live.exists() && !from_pi(&live).unwrap());

        let conn = Connection::open(&live).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(PI_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO projects VALUES ('p', '/x', NULL, 1, NULL);
             INSERT INTO threads (id, project_id, cwd, name, header, model, thinking_level, created_at, updated_at, tools, pinned, spend_cost)
                VALUES ('t', 'p', '/x', 'Bug hunt', '{}', 'cliproxyapi/claude-opus-5-5', 'high', 1, 9, '[\"ask_user\"]', 1, 77),
                       ('fork', 'p', '/x', 'Bug hunt', NULL, NULL, NULL, 5, 5, NULL, 0, 77),
                       ('late', 'p', '/x', 'Bug hunt', NULL, NULL, NULL, 5, 5, NULL, 0, 77),
                       ('odd', 'p', '/x', NULL, NULL, 'anthropic/claude-opus-5-5', 'low', 6, 6, NULL, 0, 0);
             UPDATE threads SET forked_from = 't', forked_at = 'a' WHERE id = 'fork';
             UPDATE threads SET forked_from = 't', forked_at = 'l' WHERE id = 'late';
             INSERT INTO blobs VALUES ('ab', 'image/png', x'010203');
             INSERT INTO usage VALUES (3000, 'cliproxyapi', 'claude-opus-5-5', 1, 2, 3, 4, 0.5), (10, 'cliproxyapi', 'qwen3', 9, 9, 9, 9, 9.0);
             INSERT INTO asks VALUES ('t', 'q', '{\"id\":\"q\",\"questions\":[{\"text\":\"db?\"}]}', 8);
             INSERT INTO message_pins VALUES ('t', 'a');
             INSERT INTO meta VALUES ('service_id', 'the-id');",
        )
        .unwrap();
        let pi_reply = |timestamp: i64| {
            json!({"role":"assistant","api":"cliproxyapi-codex-responses","provider":"cliproxyapi","model":"claude-opus-5-5","stopReason":"stop",
                "timestamp":timestamp,"content":[{"type":"text","text":"found it"}],
                "usage":{"input":1,"output":2,"cacheRead":3,"cacheWrite":4,"cost":{"input":0.25,"output":0.25,"total":0.5}}})
        };
        let message =
            |id: &str, parent: Option<&str>, message: Value| json!({"type":"message","id":id,"parentId":parent,"message":message});
        let image = json!({"type":"image","mimeType":"image/png","blob":"ab"});
        let model = json!({"type":"model_change","id":"m","parentId":null,"provider":"cliproxyapi","modelId":"claude-opus-5-5"});
        let named = json!({"type":"session_info","id":"n","parentId":"a","name":"Bug hunt"});
        let compaction = json!({"type":"compaction","id":"c","parentId":"n","timestamp":"1970-01-01T00:00:04.000Z","summary":"so far",
            "firstKeptEntryId":"u","tokensBefore":9,"usage":{"input":10,"output":20,"cost":{"input":1.0,"output":1.0,"total":2.0}}});
        let level = json!({"type":"thinking_level_change","id":"l","parentId":"c","thinkingLevel":"high"});
        let said = || message("u", Some("m"), json!({"role":"user","timestamp":2000,"content":"find the bug"}));
        let pi = [
            ("t", model.clone()),
            ("t", message("u", Some("m"), json!({"role":"user","timestamp":2000,"content":[{"type":"text","text":"find the bug"},image]}))),
            ("t", message("a", Some("u"), pi_reply(3000))),
            ("t", named.clone()),
            ("t", compaction.clone()),
            ("t", level.clone()),
            ("fork", json!({"type":"model_change","id":"m","parentId":null,"provider":"cliproxyapi","modelId":"gpt-9"})),
            ("fork", said()),
            ("fork", message("a", Some("u"), pi_reply(3000))),
            ("fork", message("b", Some("a"), pi_reply(6000))),
            ("late", model),
            ("late", said()),
            ("late", message("a", Some("u"), pi_reply(3000))),
            ("late", named),
            ("late", compaction),
            ("late", level),
            ("late", message("b", Some("l"), pi_reply(6000))),
            ("odd", message("x", None, json!({"role":"bashExecution","command":"ls"}))),
        ];
        for (seq, (thread, json)) in pi.iter().enumerate() {
            conn.execute("INSERT INTO entries VALUES (?1, ?2, '', NULL, '', ?3)", params![thread, seq as i64, json.to_string()]).unwrap();
        }
        let killed = dir.join("killed");
        std::fs::create_dir(&killed).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let name = format!("service.db{suffix}");
            std::fs::copy(dir.join(&name), killed.join(&name)).unwrap();
        }
        drop(conn);
        let db = killed.join("service.db");
        assert!(from_pi(&db).unwrap());
        settle(&db).unwrap();
        assert!(!killed.join("service.db-wal").exists());

        let mut told = vec![];
        let migration = migrate(&db, |done, total| told.push((done, total))).unwrap().unwrap();
        assert_eq!(told, [(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)]);
        assert_eq!((migration.threads, migration.entries, migration.kept), (4, 18, 11));
        let dropped = [("model_change", 3), ("session_info", 2), ("thinking_level_change", 2)].map(|(what, n)| (what.to_owned(), n));
        assert_eq!(migration.dropped, dropped.into());
        assert_eq!(migration.locked, [("odd".to_owned(), vec!["entry x (message): a message of role \"bashExecution\"".to_owned()])]);
        assert!(!from_pi(&db).unwrap());
        assert_eq!(migrate(&db, |_, _| {}).unwrap(), None);
        let left: Vec<_> = std::fs::read_dir(&killed).unwrap().map(|entry| entry.unwrap().file_name()).collect();
        assert_eq!(left, ["service.db"]);

        let s = Storage::open(&db).unwrap();
        let t = by_id(&s, "t");
        assert_eq!((t.name.as_deref(), t.model.as_deref(), t.effort.as_deref()), (Some("Bug hunt"), Some("claude-opus-5-5"), Some("high")));
        assert_eq!((t.project_id.as_deref(), t.updated_at, t.pinned, t.locked), (Some("p"), 9, true, false));
        assert_eq!((t.asks.len(), t.pinned_entries.as_slice(), t.entry_count), (1, &["a".to_owned()][..], 3));
        assert_eq!(s.thread_tools("t").unwrap(), ["ask_user"]);
        let entries = s.get_thread("t").unwrap().unwrap();
        assert_eq!(
            entries.iter().map(|e| (e.seq, e.id.as_str(), e.parent_id.as_deref(), e.at)).collect::<Vec<_>>(),
            [(0, "u", None, 2000), (1, "a", Some("u"), 3000), (2, "c", Some("a"), 4000)]
        );
        let image = Block::Image { mime: "image/png".into(), blob: "ab".into() };
        assert_eq!(entries[0].body, Body::User { content: vec![Block::text("find the bug"), image] });
        assert_eq!(s.blob("ab").unwrap(), Some(("image/png".to_owned(), vec![1, 2, 3])));
        assert_eq!(s.service_id().unwrap(), "the-id");
        let spend = Spend { input: 8, output: 2, cached: 3, cost: 0.5 };
        assert_eq!((t.spend, by_id(&s, "fork").spend), (Spend { input: 18, output: 22, cached: 3, cost: 2.5 }, spend));
        let late = by_id(&s, "late");
        assert_eq!((late.spend, late.entry_count), (spend, 4));
        assert_eq!(late.forked_from, Some(ForkedFrom { thread_id: "t".into(), entry_id: "c".into() }));
        assert_eq!(by_id(&s, "fork").model.as_deref(), Some("gpt-9"));
        assert_eq!(by_id(&s, "fork").forked_from, Some(ForkedFrom { thread_id: "t".into(), entry_id: "a".into() }));
        let usage = s.usage(0, 10_000, 10_000).unwrap();
        let mut by: Vec<_> = usage.iter().map(|b| (b.provider.as_str(), b.model.as_str(), b.messages, b.cost)).collect();
        by.sort_by_key(|by| by.0);
        assert_eq!(by, [("anthropic", "claude-opus-5-5", 2, 2.5), ("cliproxyapi", "qwen3", 1, 9.0)]);
        assert_eq!(s.search("bug", 10).unwrap().len(), 3);
        assert!(by_id(&s, "odd").locked);
        assert_eq!(s.get_thread("odd").unwrap().unwrap()[0].body, Body::Unknown);
        s.append_entry("t", reply("r", "a", 4000, Stop::End, "and another", 1, 0.5), 10).unwrap();
        assert_eq!(by_id(&s, "t").spend.cost, 3.0);
    }

    #[test]
    #[ignore = "needs a snapshot of a real service.db: see testing.md"]
    fn a_real_service_db_is_migrated() {
        let old = std::env::var("DIRIGENT_MIGRATE").expect("DIRIGENT_MIGRATE: the path of a snapshot of a service.db");
        let old = Path::new(&old);
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("service.db");
        std::fs::copy(old, &new).unwrap();
        settle(&new).unwrap();
        let started = std::time::Instant::now();
        let migration = migrate(&new, |_, _| {}).unwrap().expect("a db to migrate");
        let size = |path: &Path| path.metadata().unwrap().len() >> 20;
        println!("migrated in {:.1?}: {} MB to {} MB", started.elapsed(), size(old), size(&new));
        println!("{} threads, {} entries: {} kept, dropped {:#?}", migration.threads, migration.entries, migration.kept, migration.dropped);
        println!("locked: {:#?}", migration.locked);

        let s = Storage::open(&new).unwrap();
        s.conn.execute("ATTACH DATABASE ?1 AS old", [read_only_uri(old)]).unwrap();
        let count = |sql: &str| s.conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap();
        let counts = |sql: &str| -> BTreeMap<String, i64> {
            let mut stmt = s.conn.prepare(sql).unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect()
        };
        let strings = |sql: &str| -> Vec<String> {
            let mut stmt = s.conn.prepare(sql).unwrap();
            stmt.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };

        assert_eq!(strings("SELECT id FROM main.threads ORDER BY id"), strings("SELECT id FROM old.threads ORDER BY id"));
        assert_eq!(migration.threads as i64, count("SELECT COUNT(*) FROM old.threads"));
        assert_eq!(migration.entries as i64, count("SELECT COUNT(*) FROM old.entries"), "entries of threads that are gone?");
        assert_eq!(migration.kept as i64, count("SELECT COUNT(*) FROM main.entries"));
        assert_eq!(migration.kept + migration.dropped.values().sum::<usize>(), migration.entries);
        let before = counts(
            "SELECT CASE kind WHEN 'message' THEN 'message:' || COALESCE(json_extract(json, '$.message.role'), '?')
                              WHEN 'custom' THEN 'custom:' || COALESCE(json_extract(json, '$.customType'), '?')
                              WHEN 'custom_message' THEN 'custom_message:' || COALESCE(json_extract(json, '$.customType'), '?')
                              ELSE kind END, COUNT(*)
             FROM old.entries GROUP BY 1",
        );
        let after = counts(
            "SELECT CASE kind WHEN 'notice' THEN 'notice:' || json_extract(json, '$.kind') ELSE kind END, COUNT(*) FROM main.entries GROUP BY 1",
        );
        println!("pi's entries: {before:#?}\nours: {after:#?}");
        let n = |counts: &BTreeMap<String, i64>, kind: &str| counts.get(kind).copied().unwrap_or(0);
        let dropped = |what: &str| migration.dropped.get(what).copied().unwrap_or(0) as i64;
        let unread = n(&after, "unknown");
        assert_eq!(unread, migration.locked.iter().map(|(_, why)| why.len() as i64).sum::<i64>());
        for (ours, pis) in [
            ("user", "message:user"),
            ("assistant", "message:assistant"),
            ("tool_result", "message:toolResult"),
            ("notice:job", "custom_message:dirigent:job"),
            ("notice:child", "custom_message:dirigent:child"),
            ("notice:answer", "custom_message:dirigent:answer"),
            ("compaction", "compaction"),
            ("compaction_failed", "custom:dirigent:compaction_failed"),
        ] {
            let (ours, pis) = (n(&after, ours), n(&before, pis));
            assert!(ours <= pis && pis - ours <= unread, "{ours} of {pis}");
        }
        assert_eq!(n(&after, "store") + dropped("custom:codemode-store"), n(&before, "custom:codemode-store"));
        for what in migration.dropped.keys().filter(|what| *what != "custom:codemode-store") {
            assert_eq!(dropped(what), n(&before, what), "{what}");
        }
        assert_eq!(
            strings("SELECT id FROM main.threads WHERE locked ORDER BY id"),
            strings("SELECT DISTINCT thread_id FROM main.entries WHERE kind = 'unknown' ORDER BY 1")
        );

        let mut compacted: HashMap<String, Vec<(String, Usage)>> = HashMap::new();
        for thread in s.list_threads().unwrap() {
            let entries = s.get_thread(&thread.id).unwrap().unwrap();
            let copied = thread.forked_from.and_then(|from| entries.iter().position(|e| e.id == from.entry_id)).map_or(0, |at| at + 1);
            for e in &entries[copied..] {
                if let Body::Compaction { model, usage, .. } = &e.body
                    && *usage != Usage::default()
                {
                    compacted.entry(thread.id.clone()).or_default().push((model.clone(), *usage));
                }
            }
        }
        type Sums = (i64, i64, i64, i64, i64, f64);
        let by_model = |schema: &str| -> BTreeMap<String, Sums> {
            let sql = format!(
                "SELECT model, COUNT(*), SUM(input), SUM(output), SUM(cache_read), SUM(cache_write), SUM(cost) FROM {schema}.usage GROUP BY 1"
            );
            let mut stmt = s.conn.prepare(&sql).unwrap();
            let sums = |r: &rusqlite::Row| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)));
            stmt.query_map([], sums).unwrap().map(Result::unwrap).collect()
        };
        let mut want = by_model("old");
        for (model, usage) in compacted.values().flatten() {
            let sums = want.entry(model.clone()).or_default();
            *sums = (
                sums.0 + 1,
                sums.1 + usage.input as i64,
                sums.2 + usage.output as i64,
                sums.3 + usage.cache_read as i64,
                sums.4 + usage.cache_write as i64,
                sums.5 + usage.cost.total(),
            );
        }
        let ours = by_model("main");
        assert_eq!(ours.keys().collect::<Vec<_>>(), want.keys().collect::<Vec<_>>());
        for (model, ours) in &ours {
            let want = want[model];
            assert_eq!((ours.0, ours.1, ours.2, ours.3, ours.4), (want.0, want.1, want.2, want.3, want.4), "{model}");
            assert!((ours.5 - want.5).abs() < 1e-6, "{model} cost ${}, wanted ${}", ours.5, want.5);
        }
        println!("usage: {ours:#?}\nunder: {:?}", counts("SELECT provider, COUNT(*) FROM main.usage GROUP BY 1"));
        let spends = |schema: &str| -> Vec<(String, i64, i64, i64, f64)> {
            let sql = format!("SELECT id, spend_input, spend_output, spend_cached, spend_cost FROM {schema}.threads ORDER BY id");
            let mut stmt = s.conn.prepare(&sql).unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap().map(Result::unwrap).collect()
        };
        for (ours, mut pis) in spends("main").into_iter().zip(spends("old")) {
            for (_, usage) in compacted.get(&pis.0).into_iter().flatten() {
                pis = (
                    pis.0,
                    pis.1 + usage.prompt() as i64,
                    pis.2 + usage.output as i64,
                    pis.3 + usage.cache_read as i64,
                    pis.4 + usage.cost.total(),
                );
            }
            assert_eq!((&ours.0, ours.1, ours.2, ours.3), (&pis.0, pis.1, pis.2, pis.3));
            assert!((ours.4 - pis.4).abs() < 1e-6, "thread {} spent ${}, wanted ${}", ours.0, ours.4, pis.4);
        }
        let compactions: Vec<&Usage> = compacted.values().flatten().map(|(_, usage)| usage).collect();
        println!(
            "spend: ${:.2}, ${:.2} of it by {} compactions the old db did not count",
            s.list_threads().unwrap().iter().map(|t| t.spend.cost).sum::<f64>(),
            compactions.iter().map(|usage| usage.cost.total()).sum::<f64>(),
            compactions.len()
        );

        for table in ["projects", "blobs", "asks", "message_pins", "meta", "api_keys", "proxy_samples"] {
            let rows = |schema: &str| count(&format!("SELECT COUNT(*) FROM {schema}.{table}"));
            assert_eq!(rows("main"), rows("old"), "{table}");
        }
        let same = count("SELECT COUNT(*) FROM main.blobs b JOIN old.blobs o ON o.id = b.id AND o.mime = b.mime AND o.bytes = b.bytes");
        assert_eq!(same, count("SELECT COUNT(*) FROM old.blobs"));
        let id = "SELECT value FROM {}.meta WHERE key = 'service_id'";
        assert_eq!(strings(&id.replace("{}", "main")), strings(&id.replace("{}", "old")));
        let rows = "SELECT id || ' ' || COALESCE(name, '-') || ' ' || COALESCE({effort}, '-') || ' ' || COALESCE({model}, '-') || ' ' || updated_at
                           || ' ' || COALESCE(parent_id, '-') || ' ' || COALESCE(project_id, '-') || ' ' || cwd FROM {db}.threads ORDER BY id";
        let ours = rows.replace("{effort}", "effort").replace("{model}", "model").replace("{db}", "main");
        let pis =
            rows.replace("{effort}", "thinking_level").replace("{model}", "substr(model, instr(model, '/') + 1)").replace("{db}", "old");
        assert_eq!(strings(&ours), strings(&pis));

        let dangling = [
            "SELECT COUNT(*) FROM main.message_pins p
              WHERE NOT EXISTS (SELECT 1 FROM main.entries e WHERE e.thread_id = p.thread_id AND e.id = p.entry_id)",
            "SELECT COUNT(*) FROM main.threads t
              WHERE forked_at IS NOT NULL AND NOT EXISTS (SELECT 1 FROM main.entries e WHERE e.thread_id = t.id AND e.id = t.forked_at)",
            "SELECT COUNT(*) FROM main.entries e
              WHERE parent_id IS NOT NULL
                AND NOT EXISTS (SELECT 1 FROM main.entries p WHERE p.thread_id = e.thread_id AND p.id = e.parent_id AND p.seq < e.seq)",
            "SELECT COUNT(*) FROM main.entries e, json_each(json_extract(e.json, '$.content')) b
              WHERE e.kind IN ('user', 'tool_result') AND json_extract(b.value, '$.type') = 'image'
                AND NOT EXISTS (SELECT 1 FROM main.blobs WHERE id = json_extract(b.value, '$.blob'))",
            "SELECT COUNT(*) FROM main.entries e, json_each(json_extract(e.json, '$.content')) b, json_each(json_extract(b.value, '$.blobs')) l
              WHERE e.kind = 'assistant' AND NOT EXISTS (SELECT 1 FROM main.blobs WHERE id = l.value)",
        ];
        for sql in dangling {
            assert_eq!(count(sql), 0, "{sql}");
        }

        let mut edited = 0;
        for thread in s.list_threads().unwrap() {
            let ours = s.get_thread(&thread.id).unwrap().unwrap();
            let pis: Vec<Value> = strings(&format!("SELECT json FROM old.entries WHERE thread_id = '{}' ORDER BY seq", thread.id))
                .iter()
                .map(|json| serde_json::from_str(json).unwrap())
                .collect();
            let by_id: HashMap<&str, &Value> = pis.iter().filter_map(|e| Some((e["id"].as_str()?, e))).collect();
            let mut pis_branch = vec![];
            let mut next = pis.last();
            while let Some(e) = next {
                pis_branch.push(e);
                next = e["parentId"].as_str().and_then(|parent| by_id.get(parent).copied());
            }
            pis_branch.reverse();
            edited += usize::from(pis_branch.len() < pis.len());
            let was: Vec<&str> = pis_branch
                .iter()
                .filter(|e| match e["type"].as_str() {
                    Some("message") => e["message"]["role"] != "system",
                    kind => matches!(kind, Some("custom_message" | "branch_summary" | "compaction")),
                })
                .filter_map(|e| e["id"].as_str())
                .collect();
            let branch = current_branch(&ours);
            let is: Vec<&str> = branch
                .iter()
                .filter(|e| !matches!(e.body, Body::Store { .. } | Body::CompactionFailed { .. }))
                .map(|e| e.id.as_str())
                .collect();
            if !thread.locked {
                assert_eq!(is, was, "thread {}", thread.id);
            }
            let calls: std::collections::HashSet<&str> = ours
                .iter()
                .flat_map(|e| match &e.body {
                    Body::Assistant(a) => a.content.as_slice(),
                    _ => &[],
                })
                .filter_map(|b| match b {
                    Block::ToolCall { id, .. } => Some(id.as_str()),
                    _ => None,
                })
                .collect();
            for e in &ours {
                match &e.body {
                    Body::ToolResult(r) => {
                        assert!(calls.contains(r.call_id.as_str()), "thread {}: result {} answers no call", thread.id, e.id)
                    }
                    Body::Compaction { first_kept, .. } => {
                        assert!(ours.iter().any(|o| &o.id == first_kept), "thread {}: compaction {} keeps from nowhere", thread.id, e.id)
                    }
                    _ => {}
                }
            }
        }
        println!("{edited} threads with an edited message");

        s.conn
            .execute_batch(
                "CREATE VIRTUAL TABLE temp.said USING fts5 (thread_id UNINDEXED, text);
                 INSERT INTO temp.said
                 SELECT e.thread_id, CASE json_type(e.json, '$.message.content')
                     WHEN 'array' THEN (SELECT group_concat(json_extract(b.value, '$.text'), char(10))
                                        FROM json_each(json_extract(e.json, '$.message.content')) b WHERE json_extract(b.value, '$.type') = 'text')
                     ELSE json_extract(e.json, '$.message.content') END
                 FROM old.entries e WHERE e.kind = 'message' AND json_extract(e.json, '$.message.role') IN ('user', 'assistant');",
            )
            .unwrap();
        let words: std::collections::BTreeSet<String> = strings("SELECT name FROM old.threads WHERE name IS NOT NULL")
            .iter()
            .flat_map(|name| name.split(|c: char| !c.is_alphanumeric()).map(str::to_lowercase).collect::<Vec<_>>())
            .filter(|word| word.chars().count() > 2)
            .collect();
        let matching = |table: &str, word: &str| -> Vec<String> {
            let sql = format!("SELECT DISTINCT thread_id FROM {table} WHERE {} MATCH ?1 ORDER BY 1", table.split('.').next_back().unwrap());
            let mut stmt = s.conn.prepare(&sql).unwrap();
            stmt.query_map([format!("\"{word}\"*")], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
        };
        let (mut more, mut fewer) = (0, 0);
        for word in &words {
            let (ours, said, pis) = (matching("main.entries_fts", word), matching("temp.said", word), matching("old.entries_fts", word));
            assert_eq!(ours, said, "{word}");
            let missed: Vec<&String> = ours.iter().filter(|thread| !pis.contains(thread)).collect();
            for thread in &missed {
                let escaped = ["n", "t", "r"].iter().any(|e| matching("old.entries_fts", &format!("{e}{word}")).contains(thread));
                assert!(escaped, "{word}: the old index doesn't find thread {thread}");
            }
            more += usize::from(!missed.is_empty());
            fewer += usize::from(pis.iter().any(|thread| !ours.contains(thread)));
            let hits = s.search(word, 100).unwrap();
            assert!(hits.iter().all(|hit| ours.contains(&hit.thread_id)) && hits.is_empty() == ours.is_empty(), "{word}");
        }
        println!(
            "search: {} words. Next to the old index, it finds threads the old one missed for {more} of them (a word after a line break), \
             and fewer threads for {fewer} (words that were not said: thinking, tool calls)",
            words.len()
        );
    }
}
