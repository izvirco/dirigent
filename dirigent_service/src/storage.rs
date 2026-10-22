//! Sqlite persistence: the host's source of truth for threads and their pi entries.
//! `entries.seq` is append order; replaying a thread is `SELECT json ORDER BY seq`.
//! Images are kept out of the entry JSON in a content-addressed `blobs` table (see `blobs`).
//! `usage` is derived from the assistant messages (one row each) so spend over time is
//! one indexed query rather than a scan of every entry's JSON. `asks` holds the model's
//! open questions to the user (`ask_user`) until they are answered.
//!
//! Schema changes are additive and self-applying: new tables through `CREATE TABLE IF NOT
//! EXISTS`, new columns through `ensure_column`, so an existing db upgrades on open.

use anyhow::{Context, Result};
use dirigent_protocol::{Ask, Entry, Project, ThreadSummary, UsageBucket};
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
";

/// Columns added after the table first shipped: `(table, column, declaration)`.
const COLUMNS: &[(&str, &str, &str)] = &[
    // The dirigent tools the thread's last prompt enabled, as JSON (`["async_bash", ...]`).
    ("threads", "tools", "TEXT"),
    // The thread's isolated workspace directory (see `vcs`), while attached.
    ("threads", "workspace", "TEXT"),
];

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
        for (table, column, decl) in COLUMNS {
            ensure_column(&conn, table, column, decl)?;
        }
        // `usage` is derived data: an empty table on a db with history gets filled in once.
        let empty: bool = conn.query_row("SELECT NOT EXISTS (SELECT 1 FROM usage)", [], |r| r.get(0))?;
        if empty {
            conn.execute(&format!("INSERT INTO usage {USAGE_FROM_ENTRIES}"), [])?;
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

    /// Threads ordered by most recently updated. `running_since` is always `None` here; the caller fills it in.
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
        let mut stmt = self.conn.prepare(
            "SELECT t.id, p.id, t.cwd, t.name, t.model, t.thinking_level, t.created_at, t.updated_at,
                    (SELECT COUNT(*) FROM entries e WHERE e.thread_id = t.id), t.archived_at IS NOT NULL, t.last_run_ms, t.workspace
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
            })
        })?;
        let mut threads = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        for t in &mut threads {
            t.asks = asks.remove(&t.id).unwrap_or_default();
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

    /// What the next run will use, ahead of pi's `model_change` / `thinking_level_change` entries.
    pub fn set_model(&self, id: &str, model: Option<&str>, thinking_level: Option<&str>) -> Result<()> {
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

    pub fn delete_thread(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM entries WHERE thread_id = ?1", [id])?;
        self.conn.execute("DELETE FROM asks WHERE thread_id = ?1", [id])?;
        let n = self.conn.execute("DELETE FROM threads WHERE id = ?1", [id])?;
        anyhow::ensure!(n == 1, "no such thread");
        Ok(())
    }

    pub fn create_thread(&self, id: &str, project_id: &str, cwd: &str, now: i64) -> Result<ThreadSummary> {
        self.conn.execute(
            "INSERT INTO threads (id, project_id, cwd, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![id, project_id, cwd, now],
        )?;
        Ok(ThreadSummary {
            id: id.into(),
            project_id: Some(project_id.into()),
            cwd: cwd.into(),
            name: None,
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
    /// `updated_at`, tracks the current model / thinking level, and derives a thread name
    /// from a `session_info` entry or, failing that, the first user message.
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
        self.conn.execute("UPDATE threads SET updated_at = ?2 WHERE id = ?1", params![thread_id, now])?;
        if entry.kind == "message" {
            self.conn
                .execute(&format!("INSERT INTO usage {USAGE_FROM_ENTRIES} AND thread_id = ?1 AND seq = ?2"), params![thread_id, seq])?;
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
            _ => {}
        }

        if let Some(name) = derived_name(&entry.json) {
            let sql = match entry.kind.as_str() {
                "session_info" => "UPDATE threads SET name = ?2 WHERE id = ?1",
                _ => "UPDATE threads SET name = ?2 WHERE id = ?1 AND name IS NULL",
            };
            self.conn.execute(sql, params![thread_id, name])?;
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

/// `ALTER TABLE ... ADD COLUMN` unless the column is already there.
fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt.query_map([], |r| r.get::<_, String>(1))?.any(|c| c.is_ok_and(|c| c == column));
    if !exists {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"), [])?;
    }
    Ok(())
}

/// Explicit session name, or the first line of a user message as a fallback title.
fn derived_name(entry: &Value) -> Option<String> {
    match entry.get("type")?.as_str()? {
        "session_info" => entry.get("name")?.as_str().map(str::to_owned),
        "message" => {
            let msg = entry.get("message")?;
            if msg.get("role")?.as_str()? != "user" {
                return None;
            }
            let text = match msg.get("content")? {
                Value::String(s) => s.clone(),
                Value::Array(blocks) => blocks
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" "),
                _ => return None,
            };
            let line = text.lines().next()?.trim();
            if line.is_empty() {
                return None;
            }
            Some(line.chars().take(80).collect())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dirigent_protocol::Question;
    use serde_json::json;

    #[test]
    fn asks_and_tools_live_with_the_thread() {
        let s = Storage::in_memory().unwrap();
        s.create_thread("t1", "p", "/x", 0).unwrap();
        assert!(s.thread_tools("t1").unwrap().is_empty());
        s.set_tools("t1", &["ask_user".into()]).unwrap();
        assert_eq!(s.thread_tools("t1").unwrap(), ["ask_user"]);
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
        s.create_thread("t1", "p1", "/x", 10).unwrap();
        s.set_header("t1", &json!({"type":"session","id":"t1"})).unwrap();
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

        // A workspace moves the thread's cwd; the project path is still known afterwards.
        s.set_cwd("t1", "/ws/t1", Some("/ws/t1")).unwrap();
        assert_eq!(s.thread_workspace("t1").unwrap().as_deref(), Some("/ws/t1"));
        assert_eq!(s.list_threads().unwrap()[0].cwd, "/ws/t1");
        assert_eq!(s.last_entry_id("t1").unwrap().as_deref(), Some("c"));

        // Removing the project orphans the thread but keeps it.
        s.remove_project("p1", 13).unwrap();
        assert!(s.list_projects().unwrap().is_empty());
        assert_eq!(s.list_threads().unwrap()[0].project_id, None);
        assert_eq!(s.thread_project_path("t1").unwrap().as_deref(), Some("/x"));

        s.set_archived("t1", true, 14).unwrap();
        assert!(s.list_threads().unwrap()[0].archived);
        s.set_archived("t1", false, 15).unwrap();
        assert!(!s.list_threads().unwrap()[0].archived);
        s.delete_thread("t1").unwrap();
        assert!(s.list_threads().unwrap().is_empty());
        assert!(s.get_thread("t1").unwrap().is_none());
        assert!(s.delete_thread("t1").is_err());
    }

    #[test]
    fn usage_is_bucketed_per_model() {
        let s = Storage::in_memory().unwrap();
        s.create_thread("t1", "p", "/x", 0).unwrap();
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
