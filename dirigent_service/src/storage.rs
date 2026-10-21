//! Sqlite persistence: the host's source of truth for threads and their pi entries.
//! `entries.seq` is append order; replaying a thread is `SELECT json ORDER BY seq`.
//! Images are kept out of the entry JSON in a content-addressed `blobs` table (see `blobs`).

use anyhow::{Context, Result};
use dirigent_protocol::{Entry, Project, ThreadSummary};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
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

    // No migrations yet: when the schema changes, delete the db file.
    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
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

    /// Threads ordered by most recently updated. `running_since` is always `None` here; the caller fills it in.
    /// `project_id` is cleared for threads whose project was removed.
    pub fn list_threads(&self) -> Result<Vec<ThreadSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, p.id, t.cwd, t.name, t.model, t.thinking_level, t.created_at, t.updated_at,
                    (SELECT COUNT(*) FROM entries e WHERE e.thread_id = t.id), t.archived_at IS NOT NULL, t.last_run_ms
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
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
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
    /// `blobs`. Bumps `updated_at`, tracks the current model / thinking level, and derives a
    /// thread name from a `session_info` entry or, failing that, the first user message.
    pub fn append_entry(&self, thread_id: &str, mut json: Value, now: i64) -> Result<Entry> {
        let mut failed = None;
        crate::blobs::strip(&mut json, &mut |id, mime, bytes| {
            if let Err(e) = self.conn.execute("INSERT OR IGNORE INTO blobs (id, mime, bytes) VALUES (?1, ?2, ?3)", params![id, mime, bytes])
            {
                failed = Some(e);
            }
        });
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
    use serde_json::json;

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

        // Removing the project orphans the thread but keeps it.
        s.remove_project("p1", 13).unwrap();
        assert!(s.list_projects().unwrap().is_empty());
        assert_eq!(s.list_threads().unwrap()[0].project_id, None);

        s.set_archived("t1", true, 14).unwrap();
        assert!(s.list_threads().unwrap()[0].archived);
        s.set_archived("t1", false, 15).unwrap();
        assert!(!s.list_threads().unwrap()[0].archived);
        s.delete_thread("t1").unwrap();
        assert!(s.list_threads().unwrap().is_empty());
        assert!(s.get_thread("t1").unwrap().is_none());
        assert!(s.delete_thread("t1").is_err());
    }
}
