//! Local sqlite cache so the UI renders instantly before any host answers.
//! Same shape as the service db plus a `host` column. Hosts are the source of truth;
//! everything here is overwritten whenever fresh data arrives. The only desktop-owned
//! data is `links` (manual project links), `seen` (unread tracking), `snoozed` (legacy:
//! snoozes from before the hosts kept them, handed over once), `pinned` (threads kept on top of their
//! list), `pins` (pinned messages), `drafts` / `draft_images` (what the composer holds per
//! thread; the new-thread draft is the row with an empty host and thread id) and `kv`
//! (last used cwd/model/...).

use crate::input::Content;
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
CREATE TABLE IF NOT EXISTS snoozed (host TEXT NOT NULL, thread_id TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY (host, thread_id));
CREATE TABLE IF NOT EXISTS pinned (host TEXT NOT NULL, thread_id TEXT NOT NULL, PRIMARY KEY (host, thread_id));
CREATE TABLE IF NOT EXISTS pins (host TEXT NOT NULL, thread_id TEXT NOT NULL, item INTEGER NOT NULL, PRIMARY KEY (host, thread_id, item));
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS drafts (
    host TEXT NOT NULL, thread_id TEXT NOT NULL, text TEXT NOT NULL,
    sel_start INTEGER NOT NULL, sel_end INTEGER NOT NULL, reversed INTEGER NOT NULL,
    PRIMARY KEY (host, thread_id)
);
CREATE TABLE IF NOT EXISTS draft_images (
    host TEXT NOT NULL, thread_id TEXT NOT NULL, n INTEGER NOT NULL, mime TEXT NOT NULL, bytes BLOB NOT NULL,
    PRIMARY KEY (host, thread_id, n)
);
";

/// A draft's pasted image as stored: its `[image-N]` number, mime type and bytes.
pub type DraftImage = (u32, String, Vec<u8>);

/// Tables keyed by thread that go with it when it is deleted.
const PER_THREAD: [&str; 4] = ["entries", "pins", "drafts", "draft_images"];

/// Columns added after the table first shipped: `(table, column, declaration)`.
const COLUMNS: &[(&str, &str, &str)] = &[
    ("threads", "workspace", "TEXT"),
    ("threads", "parent_id", "TEXT"),
    ("threads", "run_id", "TEXT"),
    ("threads", "snoozed", "INTEGER NOT NULL DEFAULT 0"),
    ("threads", "prompted_at", "INTEGER"),
];

/// `ALTER TABLE ... ADD COLUMN` unless the column is already there.
fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt.query_map([], |r| r.get::<_, String>(1))?.any(|c| c.is_ok_and(|c| c == column));
    if !exists {
        conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"), [])?;
    }
    Ok(())
}

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
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // Schema changes are additive: new tables here, new columns in `COLUMNS`.
        conn.execute_batch(SCHEMA)?;
        for (table, column, decl) in COLUMNS {
            ensure_column(&conn, table, column, decl)?;
        }
        Ok(Self { conn })
    }

    /// Forget everything cached for a host that was removed from `hosts.toml`.
    pub fn remove_host(&self, host: &str) -> Result<()> {
        for table in ["projects", "links", "threads", "entries", "models", "seen", "snoozed", "pinned", "pins", "drafts", "draft_images"] {
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
            "SELECT id, project_id, cwd, name, model, thinking_level, created_at, updated_at, entry_count, archived, last_run_ms, workspace,
                    parent_id, run_id, snoozed, prompted_at
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
                asks: vec![],
                workspace: r.get(11)?,
                workspace_missing: false,
                parent_id: r.get(12)?,
                run_id: r.get(13)?,
                snoozed: r.get(14)?,
                prompted_at: r.get(15)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replace the host's thread list with what it reported; entries, pins and drafts of
    /// threads that no longer exist (deleted on the host) go with them.
    pub fn set_threads(&mut self, host: &str, threads: &[ThreadSummary]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM threads WHERE host = ?1", [host])?;
        for t in threads {
            tx.execute(
                "INSERT INTO threads (host, id, project_id, cwd, name, model, thinking_level, created_at, updated_at, entry_count, archived, last_run_ms, workspace, parent_id, run_id, snoozed, prompted_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                params![host, t.id, t.project_id, t.cwd, t.name, t.model, t.thinking_level, t.created_at, t.updated_at, t.entry_count as i64, t.archived, t.last_run_ms, t.workspace, t.parent_id, t.run_id, t.snoozed, t.prompted_at],
            )?;
        }
        for table in PER_THREAD {
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

    /// Snoozes from before snooze moved to the hosts: the threads of `host` still snoozed by
    /// the old rule (not updated since), handed over to the host once and then forgotten.
    pub fn take_legacy_snoozes(&self, host: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.thread_id FROM snoozed s JOIN threads t ON t.host = s.host AND t.id = s.thread_id
             WHERE s.host = ?1 AND t.updated_at <= s.updated_at",
        )?;
        let ids = stmt.query_map([host], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        self.conn.execute("DELETE FROM snoozed WHERE host = ?1", [host])?;
        Ok(ids)
    }

    /// Every pinned thread.
    pub fn pinned(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT host, thread_id FROM pinned")?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_pinned(&self, host: &str, thread_id: &str, pinned: bool) -> Result<()> {
        let sql = if pinned {
            "INSERT OR IGNORE INTO pinned (host, thread_id) VALUES (?1, ?2)"
        } else {
            "DELETE FROM pinned WHERE host = ?1 AND thread_id = ?2"
        };
        self.conn.execute(sql, params![host, thread_id])?;
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

    /// The composer's saved state for a thread: text with selection, and the pasted images.
    pub fn draft(&self, host: &str, thread_id: &str) -> Option<(Content, Vec<DraftImage>)> {
        let content = self
            .conn
            .query_row(
                "SELECT text, sel_start, sel_end, reversed FROM drafts WHERE host = ?1 AND thread_id = ?2",
                params![host, thread_id],
                |r| {
                    Ok(Content {
                        text: r.get(0)?,
                        selection: r.get::<_, i64>(1)? as usize..r.get::<_, i64>(2)? as usize,
                        reversed: r.get(3)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()?;
        let mut stmt = self.conn.prepare("SELECT n, mime, bytes FROM draft_images WHERE host = ?1 AND thread_id = ?2 ORDER BY n").ok()?;
        let images = stmt
            .query_map(params![host, thread_id], |r| Ok((r.get::<_, i64>(0)? as u32, r.get(1)?, r.get(2)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default();
        Some((content, images))
    }

    /// Save a draft. An image's bytes are written once per `n` (`INSERT OR IGNORE`), so
    /// saving on every keystroke costs a row update, not the pictures again; images no
    /// longer in `images` are dropped.
    pub fn set_draft(&mut self, host: &str, thread_id: &str, content: &Content, images: &[(u32, &str, &[u8])]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO drafts (host, thread_id, text, sel_start, sel_end, reversed) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![host, thread_id, content.text, content.selection.start as i64, content.selection.end as i64, content.reversed],
        )?;
        let keep: Vec<String> = images.iter().map(|(n, ..)| n.to_string()).collect();
        tx.execute(
            &format!("DELETE FROM draft_images WHERE host = ?1 AND thread_id = ?2 AND n NOT IN ({})", keep.join(",")),
            params![host, thread_id],
        )?;
        for (n, mime, bytes) in images {
            tx.execute(
                "INSERT OR IGNORE INTO draft_images (host, thread_id, n, mime, bytes) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![host, thread_id, *n as i64, mime, bytes],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Sent, emptied, or the thread is gone: nothing left to restore.
    pub fn remove_draft(&self, host: &str, thread_id: &str) -> Result<()> {
        for table in ["drafts", "draft_images"] {
            self.conn.execute(&format!("DELETE FROM {table} WHERE host = ?1 AND thread_id = ?2"), params![host, thread_id])?;
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str) -> ThreadSummary {
        ThreadSummary {
            id: id.into(),
            project_id: None,
            cwd: "/p".into(),
            name: None,
            model: None,
            thinking_level: None,
            created_at: 0,
            updated_at: 0,
            entry_count: 0,
            running_since: None,
            last_run_ms: None,
            archived: false,
            asks: vec![],
            workspace: None,
            workspace_missing: false,
            parent_id: None,
            run_id: None,
            snoozed: false,
            prompted_at: None,
        }
    }

    #[test]
    fn legacy_snoozes_are_handed_over_once() {
        let mut cache = Cache::in_memory().unwrap();
        let mut t = thread("t");
        t.updated_at = 5;
        let mut u = thread("u");
        u.updated_at = 9;
        cache.set_threads("h", &[t, u]).unwrap();
        cache.conn.execute("INSERT INTO snoozed VALUES ('h', 't', 5), ('h', 'u', 7), ('h', 'gone', 1)", []).unwrap();
        // `u` ran since it was snoozed, `gone` is no longer listed: only `t` is still snoozed.
        assert_eq!(cache.take_legacy_snoozes("h").unwrap(), vec!["t".to_owned()]);
        assert!(cache.take_legacy_snoozes("h").unwrap().is_empty());
    }

    #[test]
    fn drafts_round_trip_and_follow_their_thread() {
        let mut cache = Cache::in_memory().unwrap();
        let content = Content { text: "see [image-1] and [image-2]".into(), selection: 4..13, reversed: true };
        let images = [(1, "image/png", &b"one"[..]), (2, "image/png", &b"two"[..])];
        cache.set_draft("h", "t", &content, &images).unwrap();
        cache.set_draft("", "", &Content { text: "new".into(), ..Default::default() }, &[]).unwrap();

        let (got, got_images) = cache.draft("h", "t").unwrap();
        assert_eq!(got, content);
        assert_eq!(got_images, vec![(1, "image/png".to_owned(), b"one".to_vec()), (2, "image/png".to_owned(), b"two".to_vec())]);

        // Saving again keeps only the images still listed.
        cache.set_draft("h", "t", &content, &images[1..]).unwrap();
        assert_eq!(cache.draft("h", "t").unwrap().1.len(), 1);

        // The host's thread list no longer has `t`: its draft goes, the new-thread draft stays.
        cache.set_threads("h", &[thread("other")]).unwrap();
        assert!(cache.draft("h", "t").is_none());
        assert_eq!(cache.draft("", "").unwrap().0.text, "new");

        cache.remove_draft("", "").unwrap();
        assert!(cache.draft("", "").is_none());
    }
}
