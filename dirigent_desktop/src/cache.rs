//! Local sqlite cache so the UI renders instantly before any host answers.
//! Same shape as the service db plus a `host` column. Hosts are the source of truth;
//! everything here is overwritten whenever fresh data arrives. The only desktop-owned
//! data is `links` (manual project links), `seen` (unread tracking), `pins` (pinned
//! messages) and `kv` (last used cwd/model/...).

use anyhow::Result;
use dirigent_protocol::{Entry, ModelInfo, Project, ThreadSummary};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    host TEXT NOT NULL, id TEXT NOT NULL, path TEXT NOT NULL, origin TEXT, created_at INTEGER NOT NULL,
    PRIMARY KEY (host, id)
);
CREATE TABLE IF NOT EXISTS links (host TEXT NOT NULL, project_id TEXT NOT NULL, group_key TEXT NOT NULL, PRIMARY KEY (host, project_id));
CREATE TABLE IF NOT EXISTS threads (
    host TEXT NOT NULL, id TEXT NOT NULL, project_id TEXT, cwd TEXT NOT NULL, name TEXT,
    model TEXT, thinking_level TEXT,
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, entry_count INTEGER NOT NULL, last_run_ms INTEGER,
    archived INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (host, id)
);
CREATE TABLE IF NOT EXISTS entries (
    host TEXT NOT NULL, thread_id TEXT NOT NULL, seq INTEGER NOT NULL, json TEXT NOT NULL,
    PRIMARY KEY (host, thread_id, seq)
);
CREATE TABLE IF NOT EXISTS models (host TEXT PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS blobs (id TEXT PRIMARY KEY, mime TEXT NOT NULL, bytes BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS seen (host TEXT NOT NULL, thread_id TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY (host, thread_id));
CREATE TABLE IF NOT EXISTS pins (host TEXT NOT NULL, thread_id TEXT NOT NULL, item INTEGER NOT NULL, PRIMARY KEY (host, thread_id, item));
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

pub struct Cache {
    conn: Connection,
}

impl Cache {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // No migrations yet: when the schema changes, delete the db file.
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Forget everything cached for a host that was removed from `hosts.toml`.
    pub fn remove_host(&self, host: &str) -> Result<()> {
        for table in ["projects", "links", "threads", "entries", "models", "seen", "pins"] {
            self.conn.execute(&format!("DELETE FROM {table} WHERE host = ?1"), [host])?;
        }
        Ok(())
    }

    pub fn projects(&self, host: &str) -> Result<Vec<Project>> {
        let mut stmt = self.conn.prepare("SELECT id, path, origin, created_at FROM projects WHERE host = ?1 ORDER BY path")?;
        let rows = stmt.query_map([host], |r| Ok(Project { id: r.get(0)?, path: r.get(1)?, origin: r.get(2)?, created_at: r.get(3)? }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_projects(&mut self, host: &str, projects: &[Project]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM projects WHERE host = ?1", [host])?;
        for p in projects {
            tx.execute(
                "INSERT INTO projects (host, id, path, origin, created_at) VALUES (?1,?2,?3,?4,?5)",
                params![host, p.id, p.path, p.origin, p.created_at],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Manual links: (host, project id) → group key.
    pub fn links(&self) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare("SELECT host, project_id, group_key FROM links")?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_link(&self, host: &str, project_id: &str, group_key: Option<&str>) -> Result<()> {
        match group_key {
            Some(k) => self
                .conn
                .execute("INSERT OR REPLACE INTO links (host, project_id, group_key) VALUES (?1, ?2, ?3)", params![host, project_id, k])?,
            None => self.conn.execute("DELETE FROM links WHERE host = ?1 AND project_id = ?2", params![host, project_id])?,
        };
        Ok(())
    }

    pub fn threads(&self, host: &str) -> Result<Vec<ThreadSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, cwd, name, model, thinking_level, created_at, updated_at, entry_count, archived, last_run_ms
             FROM threads WHERE host = ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([host], |r| {
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

    /// Replace the host's thread list with what it reported; entries of threads that
    /// no longer exist (deleted on the host) go with them.
    pub fn set_threads(&mut self, host: &str, threads: &[ThreadSummary]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM threads WHERE host = ?1", [host])?;
        for t in threads {
            tx.execute(
                "INSERT INTO threads (host, id, project_id, cwd, name, model, thinking_level, created_at, updated_at, entry_count, archived, last_run_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![host, t.id, t.project_id, t.cwd, t.name, t.model, t.thinking_level, t.created_at, t.updated_at, t.entry_count as i64, t.archived, t.last_run_ms],
            )?;
        }
        for table in ["entries", "pins"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE host = ?1 AND thread_id NOT IN (SELECT id FROM threads WHERE host = ?1)"),
                [host],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn entries(&self, host: &str, thread_id: &str) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare("SELECT json FROM entries WHERE host = ?1 AND thread_id = ?2 ORDER BY seq")?;
        let rows = stmt.query_map(params![host, thread_id], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).filter_map(|s| serde_json::from_str(&s).ok()).collect())
    }

    pub fn set_entries(&mut self, host: &str, thread_id: &str, entries: &[Entry]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM entries WHERE host = ?1 AND thread_id = ?2", params![host, thread_id])?;
        for e in entries {
            tx.execute(
                "INSERT INTO entries (host, thread_id, seq, json) VALUES (?1,?2,?3,?4)",
                params![host, thread_id, e.seq as i64, serde_json::to_string(e)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn add_entry(&self, host: &str, thread_id: &str, entry: &Entry) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO entries (host, thread_id, seq, json) VALUES (?1,?2,?3,?4)",
            params![host, thread_id, entry.seq as i64, serde_json::to_string(entry)?],
        )?;
        Ok(())
    }

    /// Image bytes by content hash (host-independent: the same picture is one row).
    pub fn blob(&self, id: &str) -> Option<(String, Vec<u8>)> {
        self.conn.query_row("SELECT mime, bytes FROM blobs WHERE id = ?1", [id], |r| Ok((r.get(0)?, r.get(1)?))).optional().ok().flatten()
    }

    pub fn put_blob(&self, id: &str, mime: &str, bytes: &[u8]) -> Result<()> {
        self.conn.execute("INSERT OR IGNORE INTO blobs (id, mime, bytes) VALUES (?1, ?2, ?3)", params![id, mime, bytes])?;
        Ok(())
    }

    pub fn models(&self, host: &str) -> Vec<ModelInfo> {
        self.conn
            .query_row("SELECT json FROM models WHERE host = ?1", [host], |r| r.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn set_models(&self, host: &str, models: &[ModelInfo]) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO models (host, json) VALUES (?1, ?2)", params![host, serde_json::to_string(models)?])?;
        Ok(())
    }

    /// The `updated_at` of every thread as of when the user last looked at it.
    pub fn seen(&self) -> Result<Vec<(String, String, i64)>> {
        let mut stmt = self.conn.prepare("SELECT host, thread_id, updated_at FROM seen")?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_seen(&self, host: &str, thread_id: &str, updated_at: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO seen (host, thread_id, updated_at) VALUES (?1, ?2, ?3)",
            params![host, thread_id, updated_at],
        )?;
        Ok(())
    }

    /// Timeline indices of a thread's pinned messages. Entries only ever append and fold
    /// deterministically, so an index names the same message every time.
    pub fn pins(&self, host: &str, thread_id: &str) -> Vec<usize> {
        let Ok(mut stmt) = self.conn.prepare("SELECT item FROM pins WHERE host = ?1 AND thread_id = ?2") else { return vec![] };
        stmt.query_map(params![host, thread_id], |r| r.get::<_, i64>(0))
            .map(|rows| rows.filter_map(|r| r.ok()).map(|i| i as usize).collect())
            .unwrap_or_default()
    }

    pub fn set_pin(&self, host: &str, thread_id: &str, item: usize, pinned: bool) -> Result<()> {
        let sql = if pinned {
            "INSERT OR IGNORE INTO pins (host, thread_id, item) VALUES (?1, ?2, ?3)"
        } else {
            "DELETE FROM pins WHERE host = ?1 AND thread_id = ?2 AND item = ?3"
        };
        self.conn.execute(sql, params![host, thread_id, item as i64])?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO kv (key, value) VALUES (?1, ?2)", params![key, value])?;
        Ok(())
    }
}
