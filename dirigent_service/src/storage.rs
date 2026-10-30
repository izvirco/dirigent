//! Sqlite persistence: the host's source of truth for threads and their pi entries.
//! `entries.seq` is append order; replaying a thread is `SELECT json ORDER BY seq`.
//! Images are kept out of the entry JSON in a content-addressed `blobs` table (see `blobs`).
//! `usage` is derived from the assistant messages (one row each) so spend over time is
//! one indexed query rather than a scan of every entry's JSON; each thread's totals are
//! kept on its row the same way (`spend_*`, `ADD_SPEND`). `asks` holds the model's
//! open questions to the user (`ask_user`) until they are answered. `entries_fts` is a
//! full-text index of every message's text (derived like `usage`, backfilled once), for
//! `SearchThreads`.
//!
//! Schema changes are additive and self-applying: new tables through `CREATE TABLE IF NOT
//! EXISTS`, new columns through `ensure_column`, so an existing db upgrades on open. Nothing
//! is dropped: older dbs still carry the removed workflow feature's `workflow_runs` /
//! `workflow_steps` tables and `threads.run_id`, unused and harmless.

use anyhow::{Context, Result};
use dirigent_protocol::{Ask, Entry, Notify, Project, Report, SearchHit, Spend, ThreadSummary, UsageBucket, prompt_title};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::collections::HashMap;
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
    name TEXT,
    header TEXT,
    model TEXT,
    thinking_level TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    last_run_ms INTEGER,
    archived_at INTEGER
);
CREATE TABLE IF NOT EXISTS entries (
    thread_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    id TEXT NOT NULL,
    parent_id TEXT,
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
";

/// Columns added after the table first shipped: `(table, column, declaration)`.
const COLUMNS: &[(&str, &str, &str)] = &[
    // The dirigent tools the thread's last prompt enabled, as JSON (`["async_bash", ...]`).
    ("threads", "tools", "TEXT"),
    // The thread's isolated workspace directory (see `vcs`), while attached.
    ("threads", "workspace", "TEXT"),
    // The thread that spawned this one as a sub-agent (`dirigent` tool), if any.
    ("threads", "parent_id", "TEXT"),
    // `updated_at` as of when the thread was snoozed; snoozed while `updated_at` hasn't moved past it.
    ("threads", "snoozed_at", "INTEGER"),
    // When the thread was last given a task (a `Prompt` that started a turn).
    ("threads", "prompted_at", "INTEGER"),
    // A sub-agent's `Report` / `Notify` (snake_case names): what its parent hears when it
    // ends, and when. NULL = the defaults.
    ("threads", "report", "TEXT"),
    ("threads", "notify", "TEXT"),
    // On top of its status group in the clients' lists.
    ("threads", "pinned", "INTEGER NOT NULL DEFAULT 0"),
    // `updated_at` as of the user's last look (unread tracking, shared by every client).
    ("threads", "seen_at", "INTEGER"),
    // What the thread's agent has consumed (`ThreadSummary::spend`), kept up by
    // `ADD_SPEND`. Filled in from the entries when the columns are added.
    ("threads", "spend_cost", "REAL NOT NULL DEFAULT 0"),
    ("threads", "spend_input", "INTEGER NOT NULL DEFAULT 0"),
    ("threads", "spend_output", "INTEGER NOT NULL DEFAULT 0"),
    ("threads", "spend_cached", "INTEGER NOT NULL DEFAULT 0"),
    // Named by hand or by a retitle (`set_name`): a first prompt's title landing after
    // that doesn't replace the name.
    ("threads", "renamed", "INTEGER NOT NULL DEFAULT 0"),
];

/// The searchable text of a message entry: user and assistant text blocks. Used both to
/// backfill `entries_fts` once and to index each new entry, so the two can't disagree.
const FTS_FROM_ENTRIES: &str = "
    SELECT thread_id,
           COALESCE(json_extract(json, '$.message.content'),
                    (SELECT group_concat(json_extract(b.value, '$.text'), ' ')
                     FROM json_each(json_extract(json, '$.message.content')) b
                     WHERE json_extract(b.value, '$.type') = 'text'))
    FROM entries
    WHERE kind = 'message' AND json_extract(json, '$.message.role') IN ('user', 'assistant')";

/// A `usage` row extracted from an assistant message entry. Used both to backfill the table
/// from existing entries (once, when it is empty) and to add each new entry's row, so the
/// two can't disagree. Rows survive their thread's deletion: the money was spent.
const USAGE_FROM_ENTRIES: &str = "
    SELECT json_extract(json, '$.message.timestamp'),
           COALESCE(json_extract(json, '$.message.provider'), ''),
           COALESCE(json_extract(json, '$.message.model'), ''),
           COALESCE(json_extract(json, '$.message.usage.input'), 0),
           COALESCE(json_extract(json, '$.message.usage.output'), 0),
           COALESCE(json_extract(json, '$.message.usage.cacheRead'), 0),
           COALESCE(json_extract(json, '$.message.usage.cacheWrite'), 0),
           COALESCE(json_extract(json, '$.message.usage.cost.total'), 0)
    FROM entries
    WHERE kind = 'message'
      AND json_extract(json, '$.message.role') = 'assistant'
      AND json_extract(json, '$.message.usage') IS NOT NULL
      AND json_extract(json, '$.message.timestamp') IS NOT NULL";

/// Adds the usage of assistant message entries to their threads' `spend_*` columns. Used
/// both to recount them (when one of the columns is new) and to add each new entry's, so
/// the two can't disagree. Narrowed with `AND thread_id = ?1 AND seq = ?2` before the
/// `GROUP BY` placeholder `{filter}`.
const ADD_SPEND: &str = "
    UPDATE threads SET spend_cost = spend_cost + s.cost, spend_input = spend_input + s.input, spend_output = spend_output + s.output,
                       spend_cached = spend_cached + s.cached
    FROM (SELECT thread_id,
                 SUM(COALESCE(json_extract(json, '$.message.usage.cost.total'), 0)) AS cost,
                 SUM(COALESCE(json_extract(json, '$.message.usage.input'), 0)
                     + COALESCE(json_extract(json, '$.message.usage.cacheRead'), 0)
                     + COALESCE(json_extract(json, '$.message.usage.cacheWrite'), 0)) AS input,
                 SUM(COALESCE(json_extract(json, '$.message.usage.output'), 0)) AS output,
                 SUM(COALESCE(json_extract(json, '$.message.usage.cacheRead'), 0)) AS cached
          FROM entries
          WHERE kind = 'message'
            AND json_extract(json, '$.message.role') = 'assistant'
            AND json_extract(json, '$.message.usage') IS NOT NULL {filter}
          GROUP BY thread_id) s
    WHERE threads.id = s.thread_id";

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
        // New columns and their backfill land together or not at all: a backfill cut short
        // after the columns were added would never run again. (An error drops `conn`,
        // which rolls back.)
        conn.execute_batch("BEGIN")?;
        let mut added = vec![];
        for (table, column, decl) in COLUMNS {
            if ensure_column(&conn, table, column, decl)? {
                added.push(*column);
            }
        }
        // `spend_*` is derived data too: a new one means totals for the history from before
        // it, recounted from scratch so the older ones aren't counted twice.
        if added.iter().any(|c| c.starts_with("spend_")) {
            conn.execute("UPDATE threads SET spend_cost = 0, spend_input = 0, spend_output = 0, spend_cached = 0", [])?;
            conn.execute(&ADD_SPEND.replace("{filter}", ""), [])?;
        }
        conn.execute_batch("COMMIT")?;
        // `usage` is derived data: an empty table on a db with history gets filled in once.
        let empty: bool = conn.query_row("SELECT NOT EXISTS (SELECT 1 FROM usage)", [], |r| r.get(0))?;
        if empty {
            conn.execute(&format!("INSERT INTO usage {USAGE_FROM_ENTRIES}"), [])?;
        }
        let empty: bool = conn.query_row("SELECT NOT EXISTS (SELECT 1 FROM entries_fts)", [], |r| r.get(0))?;
        if empty {
            conn.execute(&format!("INSERT INTO entries_fts (thread_id, text) {FTS_FROM_ENTRIES}"), [])?;
        }
        Ok(Self { conn })
    }

    /// Live (not soft-deleted) projects.
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare("SELECT id, path, origin, created_at FROM projects WHERE deleted_at IS NULL ORDER BY path")?;
        let rows = stmt.query_map([], |r| Ok(Project { id: r.get(0)?, path: r.get(1)?, origin: r.get(2)?, created_at: r.get(3)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_project(&self, id: &str, path: &str, origin: Option<&str>, now: i64) -> Result<Project> {
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

    /// The project directory a thread was created in, even if the project was removed since
    /// (its workspace still belongs to that repo).
    pub fn thread_project_path(&self, thread_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT p.path FROM threads t JOIN projects p ON p.id = t.project_id WHERE t.id = ?1", [thread_id], |r| r.get(0))
            .optional()?)
    }

    /// Point the thread at `cwd`; `workspace` records the isolated directory it owns, if any.
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

    /// Id of the thread's last entry (pi's leaf), for appending an entry of our own.
    pub fn last_entry_id(&self, thread_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM entries WHERE thread_id = ?1 ORDER BY seq DESC LIMIT 1", [thread_id], |r| r.get(0))
            .optional()?)
    }

    /// Threads ordered by most recently updated. `running_since` and `jobs` (runtime state) are always unset here; the caller fills them in.
    /// `project_id` is cleared for threads whose project was removed.
    pub fn list_threads(&self) -> Result<Vec<ThreadSummary>> {
        let mut asks: HashMap<String, Vec<Ask>> = HashMap::new();
        let mut stmt = self.conn.prepare("SELECT thread_id, json FROM asks ORDER BY created_at")?;
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
            "SELECT t.id, p.id, t.cwd, t.name, t.model, t.thinking_level, t.created_at, t.updated_at,
                    (SELECT COUNT(*) FROM entries e WHERE e.thread_id = t.id), t.archived_at IS NOT NULL, t.last_run_ms, t.workspace,
                    t.parent_id, t.snoozed_at IS NOT NULL AND t.updated_at <= t.snoozed_at, t.prompted_at, t.pinned, t.seen_at,
                    t.spend_cost, t.spend_input, t.spend_output, t.spend_cached
             FROM threads t LEFT JOIN projects p ON p.id = t.project_id AND p.deleted_at IS NULL
             ORDER BY t.updated_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ThreadSummary {
                id: r.get(0)?,
                project_id: r.get(1)?,
                cwd: r.get(2)?,
                name: r.get(3)?,
                model: r.get(4)?,
                thinking_level: r.get(5)?,
                created_at: r.get(6)?,
                updated_at: r.get(7)?,
                entry_count: r.get::<_, i64>(8)? as u64,
                running_since: None,
                last_run_ms: r.get(10)?,
                archived: r.get(9)?,
                asks: vec![],
                workspace: r.get(11)?,
                workspace_missing: false,
                parent_id: r.get(12)?,
                snoozed: r.get(13)?,
                pinned: r.get(15)?,
                seen_at: r.get(16)?,
                pinned_entries: vec![],
                prompted_at: r.get(14)?,
                jobs: 0,
                spend: Spend {
                    cost: r.get(17)?,
                    input: r.get::<_, i64>(18)? as u64,
                    output: r.get::<_, i64>(19)? as u64,
                    cached: r.get::<_, i64>(20)? as u64,
                },
                setup: None,
            })
        })?;
        let mut threads = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        for t in &mut threads {
            t.asks = asks.remove(&t.id).unwrap_or_default();
            t.pinned_entries = pins.remove(&t.id).unwrap_or_default();
        }
        Ok(threads)
    }

    /// Remember the dirigent tools the client enabled, for the thread's next load.
    pub fn set_tools(&self, id: &str, tools: &[String]) -> Result<()> {
        self.conn.execute("UPDATE threads SET tools = ?2 WHERE id = ?1", params![id, serde_json::to_string(tools)?])?;
        Ok(())
    }

    /// The tools of the thread's last prompt; none for a thread that never had one.
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

    /// Remove the ask and return it, if it was still open.
    pub fn take_ask(&self, thread_id: &str, ask_id: &str) -> Result<Option<Ask>> {
        let json: Option<String> = self
            .conn
            .query_row("SELECT json FROM asks WHERE thread_id = ?1 AND id = ?2", params![thread_id, ask_id], |r| r.get(0))
            .optional()?;
        self.conn.execute("DELETE FROM asks WHERE thread_id = ?1 AND id = ?2", params![thread_id, ask_id])?;
        Ok(json.map(|j| serde_json::from_str(&j)).transpose()?)
    }

    /// A `Prompt` with `text` started a turn at `now`; `model` / `thinking_level` are what the
    /// run will use, ahead of pi's `model_change` / `thinking_level_change` entries. A thread
    /// with no name yet is named after the prompt (`prompt_title`) until its title lands.
    pub fn set_prompted(&self, id: &str, text: &str, model: Option<&str>, thinking_level: Option<&str>, now: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE threads SET prompted_at = ?2, name = COALESCE(name, ?3) WHERE id = ?1",
            params![id, now, prompt_title(text)],
        )?;
        if let Some(m) = model {
            self.conn.execute("UPDATE threads SET model = ?2 WHERE id = ?1", params![id, m])?;
        }
        if let Some(l) = thinking_level {
            self.conn.execute("UPDATE threads SET thinking_level = ?2 WHERE id = ?1", params![id, l])?;
        }
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

    /// Restore the thread if it is archived; false when it wasn't (nothing to push). Archive
    /// holds threads at rest: a message or a run brings one back, for good.
    pub fn unarchive(&self, id: &str) -> Result<bool> {
        let n = self.conn.execute("UPDATE threads SET archived_at = NULL WHERE id = ?1 AND archived_at IS NOT NULL", [id])?;
        Ok(n == 1)
    }

    /// Snooze as of the thread's current `updated_at` (its next run wakes it), or wake it.
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

    /// A name given by hand or by a retitle. It sticks: a first prompt's title, which comes
    /// as an entry (`append_entry`) and may still be on its way, no longer applies.
    pub fn set_name(&self, id: &str, name: &str) -> Result<()> {
        let n = self.conn.execute("UPDATE threads SET name = ?2, renamed = 1 WHERE id = ?1", params![id, name])?;
        anyhow::ensure!(n == 1, "no such thread");
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

    /// Move the thread's `seen_at` forward to `updated_at`; false when it was already there
    /// (or further), so a client re-marking what it already saw causes no push.
    pub fn set_seen(&self, id: &str, updated_at: i64) -> Result<bool> {
        let n = self
            .conn
            .execute("UPDATE threads SET seen_at = ?2 WHERE id = ?1 AND (seen_at IS NULL OR seen_at < ?2)", params![id, updated_at])?;
        Ok(n == 1)
    }

    /// The thread that spawned this one, if any.
    pub fn thread_parent(&self, id: &str) -> Result<Option<String>> {
        let p: Option<Option<String>> =
            self.conn.query_row("SELECT parent_id FROM threads WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        Ok(p.flatten())
    }

    /// How the sub-agent's parent is told it ended (see `report`).
    pub fn set_report(&self, id: &str, report: Report, notify: Notify) -> Result<()> {
        let name = |v: Value| v.as_str().map(str::to_owned);
        self.conn.execute(
            "UPDATE threads SET report = ?2, notify = ?3 WHERE id = ?1",
            params![id, name(serde_json::to_value(report)?), name(serde_json::to_value(notify)?)],
        )?;
        Ok(())
    }

    /// What the sub-agent's parent hears when it ends, and when; the defaults for a thread
    /// spawned before these existed.
    pub fn report(&self, id: &str) -> Result<(Report, Notify)> {
        let (report, notify): (Option<String>, Option<String>) =
            self.conn.query_row("SELECT report, notify FROM threads WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        fn parse<T: serde::de::DeserializeOwned + Default>(s: Option<String>) -> T {
            s.and_then(|s| serde_json::from_value(Value::String(s)).ok()).unwrap_or_default()
        }
        Ok((parse(report), parse(notify)))
    }

    /// The thread and its sub-agents, at any depth.
    pub fn family(&self, id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE f(id) AS (SELECT ?1 UNION SELECT t.id FROM threads t JOIN f ON t.parent_id = f.id) SELECT id FROM f",
        )?;
        Ok(stmt.query_map([id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?)
    }

    /// How many ancestors the thread has (0 for a root).
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

    /// The text of the thread's last final assistant message (the reply that ended a turn),
    /// if it came after the latest user message: an aborted run has no reply.
    pub fn last_reply(&self, thread_id: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT json FROM entries WHERE thread_id = ?1 AND kind = 'message'
               AND json_extract(json, '$.message.role') = 'assistant'
               AND json_extract(json, '$.message.stopReason') IN ('stop', 'length')
               AND seq > (SELECT COALESCE(MAX(seq), -1) FROM entries
                          WHERE thread_id = ?1 AND kind = 'message' AND json_extract(json, '$.message.role') = 'user')
             ORDER BY seq DESC LIMIT 1",
        )?;
        let json: Option<String> = stmt.query_row([thread_id], |r| r.get(0)).optional()?;
        let Some(json) = json else { return Ok(None) };
        Ok(Some(message_text(&serde_json::from_str(&json)?)).filter(|t| !t.trim().is_empty()))
    }

    /// What a retitle reads (`title::digest`): the text of the thread's user messages,
    /// oldest first, and of its latest final reply (whether or not a run came after it).
    pub fn conversation(&self, thread_id: &str) -> Result<(Vec<String>, Option<String>)> {
        let texts = |sql: &str| -> Result<Vec<String>> {
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map([thread_id], |r| r.get::<_, String>(0))?;
            let mut texts = vec![];
            for json in rows {
                let text = message_text(&serde_json::from_str(&json?)?);
                if !text.trim().is_empty() {
                    texts.push(text);
                }
            }
            Ok(texts)
        };
        let prompts = texts(
            "SELECT json FROM entries WHERE thread_id = ?1 AND kind = 'message'
               AND json_extract(json, '$.message.role') = 'user' ORDER BY seq",
        )?;
        let reply = texts(
            "SELECT json FROM entries WHERE thread_id = ?1 AND kind = 'message'
               AND json_extract(json, '$.message.role') = 'assistant'
               AND json_extract(json, '$.message.stopReason') IN ('stop', 'length')
             ORDER BY seq DESC LIMIT 1",
        )?;
        Ok((prompts, reply.into_iter().next()))
    }

    /// Full-text search: the best-matching passage per thread, best thread first. Every
    /// word of `query` must match (as a prefix), in any message of the thread.
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
            model: None,
            thinking_level: None,
            created_at: now,
            updated_at: now,
            entry_count: 0,
            running_since: None,
            last_run_ms: None,
            archived: false,
            asks: vec![],
            workspace: None,
            workspace_missing: false,
            parent_id: parent.map(str::to_owned),
            snoozed: false,
            pinned: false,
            seen_at: None,
            pinned_entries: vec![],
            prompted_at: None,
            jobs: 0,
            spend: Spend::default(),
            setup: None,
        })
    }

    pub fn thread_cwd(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT cwd FROM threads WHERE id = ?1", [id], |r| r.get(0)).optional()?)
    }

    /// Header (null if the runtime hasn't produced one yet) plus all entries in order.
    pub fn get_thread(&self, id: &str) -> Result<Option<(Value, Vec<Entry>)>> {
        let header: Option<Option<String>> =
            self.conn.query_row("SELECT header FROM threads WHERE id = ?1", [id], |r| r.get(0)).optional()?;
        let Some(header) = header else { return Ok(None) };
        let header = header.map(|h| serde_json::from_str(&h)).transpose()?.unwrap_or(Value::Null);

        let mut stmt = self.conn.prepare("SELECT seq, id, parent_id, kind, json FROM entries WHERE thread_id = ?1 ORDER BY seq")?;
        let entries = stmt
            .query_map([id], |r| {
                Ok(Entry {
                    seq: r.get::<_, i64>(0)? as u64,
                    id: r.get(1)?,
                    parent_id: r.get(2)?,
                    kind: r.get(3)?,
                    json: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or(Value::Null),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some((header, entries)))
    }

    pub fn set_header(&self, id: &str, header: &Value) -> Result<()> {
        self.conn.execute("UPDATE threads SET header = ?2 WHERE id = ?1", params![id, header.to_string()])?;
        Ok(())
    }

    /// Append a pi entry; returns it with its assigned `seq` and its images moved to
    /// `blobs` (inline ones, and the local files a reply links to; see `blobs.rs`). Bumps
    /// `updated_at` for what the transcript shows (so a cache warm or a title doesn't
    /// mark the thread unread or move it up the inbox), tracks the current model /
    /// thinking level, and takes the thread's name from a `session_info` entry (the
    /// generated title).
    pub fn append_entry(&self, thread_id: &str, mut json: Value, now: i64) -> Result<Entry> {
        let mut failed = None;
        let mut store = |id: &str, mime: &str, bytes: &[u8]| {
            if let Err(e) = self.conn.execute("INSERT OR IGNORE INTO blobs (id, mime, bytes) VALUES (?1, ?2, ?3)", params![id, mime, bytes])
            {
                failed = Some(e);
            }
        };
        if let Some(cwd) = self.thread_cwd(thread_id)? {
            crate::blobs::link_images(&mut json, std::path::Path::new(&cwd), &mut store);
        }
        crate::blobs::strip(&mut json, &mut store);
        if let Some(e) = failed {
            return Err(e.into());
        }
        let seq: i64 =
            self.conn.query_row("SELECT COALESCE(MAX(seq) + 1, 0) FROM entries WHERE thread_id = ?1", [thread_id], |r| r.get(0))?;
        let entry = Entry::from_pi(seq as u64, json);
        self.conn.execute(
            "INSERT INTO entries (thread_id, seq, id, parent_id, kind, json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![thread_id, seq, entry.id, entry.parent_id, entry.kind, entry.json.to_string()],
        )?;
        if matches!(entry.kind.as_str(), "message" | "custom_message" | "compaction") {
            self.conn.execute("UPDATE threads SET updated_at = ?2 WHERE id = ?1", params![thread_id, now])?;
        }
        if entry.kind == "message" {
            self.conn
                .execute(&format!("INSERT INTO usage {USAGE_FROM_ENTRIES} AND thread_id = ?1 AND seq = ?2"), params![thread_id, seq])?;
            self.conn.execute(&ADD_SPEND.replace("{filter}", "AND thread_id = ?1 AND seq = ?2"), params![thread_id, seq])?;
            self.conn.execute(
                &format!("INSERT INTO entries_fts (thread_id, text) {FTS_FROM_ENTRIES} AND thread_id = ?1 AND seq = ?2"),
                params![thread_id, seq],
            )?;
        }

        let j = &entry.json;
        match entry.kind.as_str() {
            "model_change" => {
                if let (Some(p), Some(m)) = (j.get("provider").and_then(Value::as_str), j.get("modelId").and_then(Value::as_str)) {
                    self.conn.execute("UPDATE threads SET model = ?2 WHERE id = ?1", params![thread_id, format!("{p}/{m}")])?;
                }
            }
            "thinking_level_change" => {
                if let Some(l) = j.get("thinkingLevel").and_then(Value::as_str) {
                    self.conn.execute("UPDATE threads SET thinking_level = ?2 WHERE id = ?1", params![thread_id, l])?;
                }
            }
            // An explicit name: how the runtime's generated title arrives. A name set since
            // (`set_name`) wins.
            "session_info" => {
                if let Some(name) = j.get("name").and_then(Value::as_str) {
                    self.conn.execute("UPDATE threads SET name = ?2 WHERE id = ?1 AND renamed = 0", params![thread_id, name])?;
                }
            }
            _ => {}
        }
        Ok(entry)
    }

    /// Usage in `since..until`, summed per `bucket`-wide slot from `since` and per
    /// provider + model, oldest slot first.
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

    pub fn blob(&self, id: &str) -> Result<Option<(String, Vec<u8>)>> {
        Ok(self.conn.query_row("SELECT mime, bytes FROM blobs WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    /// Entries as pi expects them: image `blob` refs replaced by their inline `data`.
    pub fn inflate(&self, entries: &mut [Value]) {
        for e in entries {
            crate::blobs::inflate(e, &mut |id| self.blob(id).ok().flatten().map(|(_, bytes)| bytes));
        }
    }
}

/// The text of a pi message entry: its string content, or its text blocks (images and tool
/// calls left out) a line apart.
fn message_text(entry: &Value) -> String {
    match entry.pointer("/message/content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `ALTER TABLE ... ADD COLUMN` unless the column is already there; true if it was added.
fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt.query_map([], |r| r.get::<_, String>(1))?.any(|c| c.is_ok_and(|c| c == column));
    if !exists {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"), [])?;
    }
    Ok(!exists)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dirigent_protocol::Question;
    use serde_json::json;

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

    /// A db from before `COLUMNS` / `asks` existed gets them on open.
    #[test]
    fn old_db_is_upgraded_on_open() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, project_id TEXT, cwd TEXT NOT NULL, name TEXT, header TEXT, model TEXT,
             thinking_level TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, last_run_ms INTEGER, archived_at INTEGER);
             INSERT INTO threads (id, cwd, created_at, updated_at) VALUES ('t1', '/x', 1, 1);",
        )
        .unwrap();
        let s = Storage::init(conn).unwrap();
        assert_eq!(s.report("t1").unwrap(), (Report::Reply, Notify::Asap));
        s.set_tools("t1", &["async_bash".into()]).unwrap();
        assert_eq!(s.thread_tools("t1").unwrap(), ["async_bash"]);
        assert!(s.list_threads().unwrap()[0].asks.is_empty());
        // Opening again is a no-op.
        let s = Storage::init(s.conn).unwrap();
        assert_eq!(s.thread_tools("t1").unwrap(), ["async_bash"]);
    }

    #[test]
    fn roundtrip_thread_and_entries() {
        let s = Storage::in_memory().unwrap();
        s.add_project("p1", "/x", Some("github.com/a/b"), 9).unwrap();
        s.create_thread("t1", "p1", "/x", None, None, 10).unwrap();
        s.set_header("t1", &json!({"type":"session","id":"t1"})).unwrap();
        // Named after its first prompt until a title arrives.
        s.set_prompted("t1", "  hello  \nworld", None, None, 10).unwrap();
        assert_eq!(s.list_threads().unwrap()[0].name.as_deref(), Some("hello"));
        s.set_prompted("t1", "later", None, None, 10).unwrap();
        s.append_entry("t1", json!({"type":"model_change","id":"a","parentId":null,"provider":"anthropic","modelId":"m"}), 11).unwrap();
        let e = s
            .append_entry("t1", json!({"type":"message","id":"b","parentId":"a","message":{"role":"user","content":"hello\nworld"}}), 12)
            .unwrap();
        assert_eq!(e.seq, 1);
        assert_eq!(e.parent_id.as_deref(), Some("a"));
        // Images leave the entry for the blobs table and come back on inflate.
        let img = s
            .append_entry("t1", json!({"type":"message","id":"c","message":{"role":"user","content":[{"type":"image","data":"AQID","mimeType":"image/png"}]}}), 12)
            .unwrap();
        let blob_id = img.json["message"]["content"][0]["blob"].as_str().unwrap().to_owned();
        assert_eq!(s.blob(&blob_id).unwrap(), Some(("image/png".to_owned(), vec![1, 2, 3])));
        let mut inflated = vec![img.json.clone()];
        s.inflate(&mut inflated);
        assert_eq!(inflated[0]["message"]["content"][0]["data"], "AQID");

        let (header, entries) = s.get_thread("t1").unwrap().unwrap();
        assert_eq!(header["id"], "t1");
        assert_eq!(entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);

        let threads = s.list_threads().unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].name.as_deref(), Some("hello"));
        assert_eq!(threads[0].model.as_deref(), Some("anthropic/m"));
        assert_eq!(threads[0].project_id.as_deref(), Some("p1"));
        assert_eq!(threads[0].entry_count, 3);
        assert_eq!(threads[0].updated_at, 12);
        assert!(s.get_thread("nope").unwrap().is_none());
        // The generated title replaces it.
        s.append_entry("t1", json!({"type":"session_info","id":"n","parentId":"c","name":"Greeting"}), 20).unwrap();
        assert_eq!(s.list_threads().unwrap()[0].name.as_deref(), Some("Greeting"));
        // Neither it nor a cache warm is news: `updated_at` stays put.
        s.append_entry("t1", json!({"type":"usage","id":"w","parentId":"n","kind":"cache_warm"}), 21).unwrap();
        assert_eq!(s.list_threads().unwrap()[0].updated_at, 12);

        // A workspace moves the thread's cwd; the project path is still known afterwards.
        s.set_cwd("t1", "/ws/t1", Some("/ws/t1")).unwrap();
        assert_eq!(s.thread_workspace("t1").unwrap().as_deref(), Some("/ws/t1"));
        assert_eq!(s.list_threads().unwrap()[0].cwd, "/ws/t1");
        assert_eq!(s.last_entry_id("t1").unwrap().as_deref(), Some("w"));

        // Removing the project orphans the thread but keeps it.
        s.remove_project("p1", 13).unwrap();
        assert!(s.list_projects().unwrap().is_empty());
        assert_eq!(s.list_threads().unwrap()[0].project_id, None);
        assert_eq!(s.thread_project_path("t1").unwrap().as_deref(), Some("/x"));

        s.set_archived("t1", true, 14).unwrap();
        assert!(s.list_threads().unwrap()[0].archived);
        s.set_archived("t1", false, 15).unwrap();
        assert!(!s.list_threads().unwrap()[0].archived);
        // A message to an archived thread restores it.
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
        let by_id = |s: &Storage, id: &str| s.list_threads().unwrap().into_iter().find(|t| t.id == id).unwrap();

        // Snoozed until the thread's next entry moves `updated_at`.
        s.set_snoozed("root", true).unwrap();
        assert!(by_id(&s, "root").snoozed);
        s.append_entry("root", json!({"type":"message","id":"u","message":{"role":"user","content":"find the sqlite bug"}}), 50).unwrap();
        assert!(!by_id(&s, "root").snoozed);
        s.set_snoozed("root", true).unwrap();
        s.set_snoozed("root", false).unwrap();
        assert!(!by_id(&s, "root").snoozed);

        // Pins and seen marks live with the thread; `seen_at` only moves forward.
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

        // Search covers user and assistant text; the last final reply is what a parent reads.
        let assistant = |id: &str, stop: &str, text: &str| json!({"type":"message","id":id,"message":{"role":"assistant","stopReason":stop,"content":[{"type":"text","text":text}]}});
        s.append_entry("root", assistant("a1", "toolUse", "looking at storage.rs"), 51).unwrap();
        s.append_entry("root", assistant("a2", "stop", "The bug is a missing index on usage.at"), 52).unwrap();
        s.append_entry(
            "kid",
            json!({"type":"message","id":"k","message":{"role":"user","content":[{"type":"text","text":"write docs"}]}}),
            53,
        )
        .unwrap();
        assert_eq!(s.last_reply("root").unwrap().as_deref(), Some("The bug is a missing index on usage.at"));
        assert_eq!(s.last_reply("kid").unwrap(), None);
        // Named by its spawner: its first prompt doesn't rename it.
        s.set_prompted("kid", "write docs", None, None, 60).unwrap();
        assert_eq!(by_id(&s, "kid").name.as_deref(), Some("Review"));
        assert_eq!(by_id(&s, "kid").prompted_at, Some(60));
        // A new prompt without a reply yet (or aborted): no reply to report.
        s.append_entry("root", json!({"type":"message","id":"u2","message":{"role":"user","content":"and again"}}), 54).unwrap();
        assert_eq!(s.last_reply("root").unwrap(), None);
        // A retitle reads every user message and the latest final reply, a run after it or not.
        let (prompts, reply) = s.conversation("root").unwrap();
        assert_eq!(prompts, ["find the sqlite bug", "and again"]);
        assert_eq!(reply.as_deref(), Some("The bug is a missing index on usage.at"));
        s.set_name("root", "Missing usage index").unwrap();
        assert_eq!(by_id(&s, "root").name.as_deref(), Some("Missing usage index"));
        // A first title landing after it doesn't replace it.
        s.append_entry("root", json!({"type":"session_info","id":"n","name":"Sqlite bug"}), 55).unwrap();
        assert_eq!(by_id(&s, "root").name.as_deref(), Some("Missing usage index"));
        assert!(s.set_name("nope", "x").is_err());
        let hits = s.search("sqlite", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].thread_id, "root");
        assert!(hits[0].snippet.contains("sqlite"));
        assert_eq!(s.search("docs", 10).unwrap()[0].thread_id, "kid");
        assert!(s.search("\"weird) syntax", 10).unwrap().is_empty());
        assert_eq!(s.search("", 10).unwrap().len(), 0);
        // The index is rebuilt from the entries when it is empty (a db from before it existed).
        s.conn.execute("DELETE FROM entries_fts", []).unwrap();
        let s = Storage::init(s.conn).unwrap();
        assert_eq!(s.search("index", 10).unwrap()[0].thread_id, "root");
        s.delete_thread("root").unwrap();
        assert!(s.search("index", 10).unwrap().is_empty());
    }

    #[test]
    fn usage_is_bucketed_per_model() {
        let s = Storage::in_memory().unwrap();
        s.create_thread("t1", "p", "/x", None, None, 0).unwrap();
        let assistant = |at: i64, model: &str, input: u64, cost: f64| {
            json!({"type":"message","id":"x","message":{"role":"assistant","provider":"anthropic","model":model,"timestamp":at,
                "usage":{"input":input,"output":10,"cacheRead":100,"cacheWrite":5,"totalTokens":0,"cost":{"total":cost}}}})
        };
        s.append_entry("t1", json!({"type":"message","id":"u","message":{"role":"user","content":"hi","timestamp":1}}), 1).unwrap();
        s.append_entry("t1", assistant(1000, "a", 1, 0.5), 1).unwrap();
        s.append_entry("t1", assistant(1500, "a", 2, 0.25), 1).unwrap();
        s.append_entry("t1", assistant(2500, "b", 4, 1.0), 1).unwrap();
        s.append_entry("t1", assistant(9000, "a", 8, 1.0), 1).unwrap(); // past `until`
        let b = s.usage(0, 5000, 1000).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!((b[0].start, b[0].model.as_str(), b[0].input, b[0].cost, b[0].messages), (1000, "a", 3, 0.75, 2));
        assert_eq!((b[1].start, b[1].model.as_str(), b[1].input, b[1].cache_read), (2000, "b", 4, 100));
        // The thread's totals: every prompt token (fresh + cache), output, dollars.
        let spend = |s: &Storage| s.list_threads().unwrap()[0].spend;
        let want = Spend { input: 15 + 4 * 105, output: 40, cached: 400, cost: 2.75 };
        assert_eq!(spend(&s), want);
        // Totals start from the entries when the columns are new (a db from before them),
        // and a single new one recounts them all rather than adding on top.
        for c in ["spend_cost", "spend_input", "spend_output", "spend_cached"] {
            s.conn.execute(&format!("ALTER TABLE threads DROP COLUMN {c}"), []).unwrap();
        }
        let s = Storage::init(s.conn).unwrap();
        assert_eq!(spend(&s), want);
        s.conn.execute("ALTER TABLE threads DROP COLUMN spend_cached", []).unwrap();
        let s = Storage::init(s.conn).unwrap();
        assert_eq!(spend(&s), want);
        // An empty `usage` table (a db from before it existed) is rebuilt from the entries on open.
        s.conn.execute("DELETE FROM usage", []).unwrap();
        let s = Storage::init(s.conn).unwrap();
        let total = |s: &Storage| s.usage(0, 10_000, 10_000).unwrap().iter().map(|b| b.messages).sum::<u64>();
        assert_eq!(total(&s), 4);
        // Spend outlives the thread.
        s.delete_thread("t1").unwrap();
        assert_eq!(total(&s), 4);
    }
}
