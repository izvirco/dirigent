use crate::input::Content;
use anyhow::Result;
use dirigent_protocol::transcript::Entry;
use dirigent_protocol::{ModelInfo, Project, ThreadSummary};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    host TEXT NOT NULL, id TEXT NOT NULL, path TEXT NOT NULL, origin TEXT, created_at INTEGER NOT NULL,
    PRIMARY KEY (host, id)
);
CREATE TABLE IF NOT EXISTS links (host TEXT NOT NULL, project_id TEXT NOT NULL, group_key TEXT NOT NULL, PRIMARY KEY (host, project_id));
CREATE TABLE IF NOT EXISTS project_names (group_key TEXT PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS threads (
    host TEXT NOT NULL, id TEXT NOT NULL, project_id TEXT, cwd TEXT NOT NULL, name TEXT,
    model TEXT, thinking_level TEXT,
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, entry_count INTEGER NOT NULL, last_run_ms INTEGER,
    archived INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (host, id)
);
CREATE TABLE IF NOT EXISTS transcript (
    host TEXT NOT NULL, thread_id TEXT NOT NULL, seq INTEGER NOT NULL, json TEXT NOT NULL,
    PRIMARY KEY (host, thread_id, seq)
);
CREATE TABLE IF NOT EXISTS models (host TEXT PRIMARY KEY, json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS blobs (id TEXT PRIMARY KEY, mime TEXT NOT NULL, bytes BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS seen (host TEXT NOT NULL, thread_id TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY (host, thread_id));
CREATE TABLE IF NOT EXISTS snoozed (host TEXT NOT NULL, thread_id TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY (host, thread_id));
CREATE TABLE IF NOT EXISTS pinned (host TEXT NOT NULL, thread_id TEXT NOT NULL, PRIMARY KEY (host, thread_id));
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

// MUST KILL: type: DraftImage as a struct with n, mime and bytes fields, not a tuple
pub type DraftImage = (u32, String, Vec<u8>);

const PER_THREAD: [&str; 3] = ["transcript", "drafts", "draft_images"];

const HOST_TABLES: [&str; 10] =
    ["projects", "links", "threads", "transcript", "models", "seen", "snoozed", "pinned", "drafts", "draft_images"];

const COLUMNS: &[(&str, &str, &str)] = &[
    ("threads", "workspace", "TEXT"),
    ("threads", "parent_id", "TEXT"),
    ("threads", "snoozed", "INTEGER NOT NULL DEFAULT 0"),
    ("threads", "prompted_at", "INTEGER"),
    ("threads", "pinned", "INTEGER NOT NULL DEFAULT 0"),
    ("threads", "seen_at", "INTEGER"),
    ("threads", "pinned_entries", "TEXT"),
    ("threads", "spend", "TEXT"),
    ("threads", "forked_from", "TEXT"),
];

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
        conn.execute_batch(SCHEMA)?;
        for (table, column, decl) in COLUMNS {
            ensure_column(&conn, table, column, decl)?;
        }
        // MUST KILL: extract: the DROP TABLE entries in init into a named step that drops the pi-era cache
        conn.execute_batch("DROP TABLE IF EXISTS entries")?;
        Ok(Self { conn })
    }

    pub fn remove_host(&self, host: &str) -> Result<()> {
        for table in HOST_TABLES {
            self.conn.execute(&format!("DELETE FROM {table} WHERE host = ?1"), [host])?;
        }
        Ok(())
    }

    // MUST KILL: rename: rename_host -> rename_host_replacing, it drops what new already had
    pub fn rename_host(&mut self, old: &str, new: &str) -> Result<()> {
        let tx = self.conn.transaction()?;
        for table in HOST_TABLES {
            tx.execute(&format!("DELETE FROM {table} WHERE host = ?1"), [new])?;
            tx.execute(&format!("UPDATE {table} SET host = ?2 WHERE host = ?1"), [old, new])?;
        }
        tx.commit()?;
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

    // MUST KILL: type: links() returns a Link struct, not a (host, project id, group key) tuple
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

    pub fn project_names(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT group_key, name FROM project_names")?;
        Ok(stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_project_name(&self, group_key: &str, name: Option<&str>) -> Result<()> {
        match name {
            Some(n) => {
                self.conn.execute("INSERT OR REPLACE INTO project_names (group_key, name) VALUES (?1, ?2)", params![group_key, n])?
            }
            None => self.conn.execute("DELETE FROM project_names WHERE group_key = ?1", [group_key])?,
        };
        Ok(())
    }

    pub fn threads(&self, host: &str) -> Result<Vec<ThreadSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, project_id, cwd, name, model, thinking_level, created_at, updated_at, entry_count, archived, last_run_ms, workspace,
                    parent_id, snoozed, prompted_at, pinned, seen_at, pinned_entries, spend, forked_from
             FROM threads WHERE host = ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([host], |r| {
            Ok(ThreadSummary {
                id: r.get(0)?,
                project_id: r.get(1)?,
                cwd: r.get(2)?,
                name: r.get(3)?,
                model: r.get(4)?,
                effort: r.get(5)?,
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
                pinned_entries: r.get::<_, Option<String>>(17)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
                prompted_at: r.get(14)?,
                jobs: 0,
                free_jobs: 0,
                spend: r.get::<_, Option<String>>(18)?.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default(),
                forked_from: r.get::<_, Option<String>>(19)?.and_then(|s| serde_json::from_str(&s).ok()),
                setup: None,
                compacting_since: None,
                screen: false,
                locked: false,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // MUST KILL: extract: the PER_THREAD prune in set_threads into a named prune_deleted_threads step
    pub fn set_threads(&mut self, host: &str, threads: &[ThreadSummary]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM threads WHERE host = ?1", [host])?;
        for t in threads {
            tx.execute(
                "INSERT INTO threads (host, id, project_id, cwd, name, model, thinking_level, created_at, updated_at, entry_count, archived, last_run_ms, workspace, parent_id, snoozed, prompted_at, pinned, seen_at, pinned_entries, spend, forked_from)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
                params![host, t.id, t.project_id, t.cwd, t.name, t.model, t.effort, t.created_at, t.updated_at, t.entry_count as i64, t.archived, t.last_run_ms, t.workspace, t.parent_id, t.snoozed, t.prompted_at, t.pinned, t.seen_at, serde_json::to_string(&t.pinned_entries)?, serde_json::to_string(&t.spend)?, t.forked_from.as_ref().map(serde_json::to_string).transpose()?],
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

    pub fn thread_models_as_ids(&self) -> Result<()> {
        self.conn.execute("UPDATE threads SET model = substr(model, instr(model, '/') + 1) WHERE instr(model, '/') > 0", [])?;
        Ok(())
    }

    pub fn entries(&self, host: &str, thread_id: &str) -> Result<Vec<Entry>> {
        let mut stmt = self.conn.prepare("SELECT json FROM transcript WHERE host = ?1 AND thread_id = ?2 ORDER BY seq")?;
        let rows = stmt.query_map(params![host, thread_id], |r| r.get::<_, String>(0))?;
        Ok(rows.filter_map(|r| r.ok()).filter_map(|s| serde_json::from_str(&s).ok()).collect())
    }

    // MUST KILL: rename: set_entries -> set_entries_up_to, entries past the snapshot's end stay
    pub fn set_entries(&mut self, host: &str, thread_id: &str, entries: &[Entry]) -> Result<()> {
        let end = entries.last().map_or(-1, |e| e.seq as i64);
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM transcript WHERE host = ?1 AND thread_id = ?2 AND seq <= ?3", params![host, thread_id, end])?;
        for e in entries {
            tx.execute(
                "INSERT INTO transcript (host, thread_id, seq, json) VALUES (?1,?2,?3,?4)",
                params![host, thread_id, e.seq as i64, serde_json::to_string(e)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove_entries(&self, host: &str, thread_id: &str, after: Option<u64>, to: u64) -> Result<()> {
        let after = after.map_or(-1, |s| s as i64);
        self.conn.execute(
            "DELETE FROM transcript WHERE host = ?1 AND thread_id = ?2 AND seq > ?3 AND seq <= ?4",
            params![host, thread_id, after, to as i64],
        )?;
        Ok(())
    }

    pub fn add_entry(&self, host: &str, thread_id: &str, entry: &Entry) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO transcript (host, thread_id, seq, json) VALUES (?1,?2,?3,?4)",
            params![host, thread_id, entry.seq as i64, serde_json::to_string(entry)?],
        )?;
        Ok(())
    }

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

    pub fn take_legacy_seen(&self, host: &str) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.thread_id, s.updated_at FROM seen s JOIN threads t ON t.host = s.host AND t.id = s.thread_id
             WHERE s.host = ?1 AND t.archived = 0",
        )?;
        let marks = stmt.query_map([host], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        self.conn.execute("DELETE FROM seen WHERE host = ?1", [host])?;
        Ok(marks)
    }

    pub fn take_legacy_snoozes(&self, host: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.thread_id FROM snoozed s JOIN threads t ON t.host = s.host AND t.id = s.thread_id
             WHERE s.host = ?1 AND t.updated_at <= s.updated_at",
        )?;
        let ids = stmt.query_map([host], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        self.conn.execute("DELETE FROM snoozed WHERE host = ?1", [host])?;
        Ok(ids)
    }

    pub fn take_legacy_pins(&self, host: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT p.thread_id FROM pinned p JOIN threads t ON t.host = p.host AND t.id = p.thread_id WHERE p.host = ?1")?;
        let ids = stmt.query_map([host], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        self.conn.execute("DELETE FROM pinned WHERE host = ?1", [host])?;
        Ok(ids)
    }

    // MUST KILL: type: the draft key as an enum (NewThread / Thread), not ("", "") strings
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

    pub fn remove(&self, key: &str) -> Result<()> {
        self.conn.execute("DELETE FROM kv WHERE key = ?1", [key])?;
        Ok(())
    }

    // MUST KILL: rename: kv_empty -> is_new_data_dir
    pub fn kv_empty(&self) -> bool {
        self.conn.query_row("SELECT NOT EXISTS (SELECT 1 FROM kv)", [], |r| r.get(0)).unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renaming_a_host_keeps_its_cache() {
        let mut c = Cache::in_memory().unwrap();
        c.set_threads("old", &[thread("t1")]).unwrap();
        c.set_link("old", "p1", Some("g")).unwrap();
        c.set_threads("new", &[thread("stale")]).unwrap();
        c.rename_host("old", "new").unwrap();
        assert!(c.threads("old").unwrap().is_empty());
        assert_eq!(c.threads("new").unwrap().iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["t1"]);
        assert_eq!(c.links().unwrap(), [("new".to_owned(), "p1".to_owned(), "g".to_owned())]);
    }

    fn thread(id: &str) -> ThreadSummary {
        ThreadSummary {
            id: id.into(),
            project_id: None,
            cwd: "/p".into(),
            name: None,
            model: None,
            effort: None,
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
            snoozed: false,
            pinned: false,
            seen_at: None,
            pinned_entries: vec![],
            prompted_at: None,
            jobs: 0,
            free_jobs: 0,
            spend: Default::default(),
            forked_from: None,
            setup: None,
            compacting_since: None,
            screen: false,
            locked: false,
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
        assert_eq!(cache.take_legacy_snoozes("h").unwrap(), vec!["t".to_owned()]);
        assert!(cache.take_legacy_snoozes("h").unwrap().is_empty());
    }

    #[test]
    fn legacy_pins_and_seen_marks_are_handed_over_once() {
        let mut cache = Cache::in_memory().unwrap();
        let mut old = thread("old");
        old.archived = true;
        cache.set_threads("h", &[thread("t"), old]).unwrap();
        cache.conn.execute("INSERT INTO pinned VALUES ('h', 't'), ('h', 'gone')", []).unwrap();
        cache.conn.execute("INSERT INTO seen VALUES ('h', 't', 5), ('h', 'old', 7), ('h', 'gone', 1)", []).unwrap();
        assert_eq!(cache.take_legacy_pins("h").unwrap(), vec!["t".to_owned()]);
        assert!(cache.take_legacy_pins("h").unwrap().is_empty());
        assert_eq!(cache.take_legacy_seen("h").unwrap(), vec![("t".to_owned(), 5)]);
        assert!(cache.take_legacy_seen("h").unwrap().is_empty());
    }

    #[test]
    fn thread_state_round_trips() {
        let mut cache = Cache::in_memory().unwrap();
        let mut t = thread("t");
        t.pinned = true;
        t.seen_at = Some(3);
        t.pinned_entries = vec!["e1".into(), "e2".into()];
        t.spend = dirigent_protocol::Spend { input: 1200, output: 30, cached: 900, cost: 0.25 };
        t.forked_from = Some(dirigent_protocol::ForkedFrom { thread_id: "s".into(), entry_id: "e1".into() });
        cache.set_threads("h", std::slice::from_ref(&t)).unwrap();
        assert_eq!(cache.threads("h").unwrap(), vec![t]);
    }

    #[test]
    fn entries_past_a_snapshot_stay_unless_stale() {
        let mut cache = Cache::in_memory().unwrap();
        let entry = |seq| Entry {
            seq,
            id: format!("e{seq}"),
            parent_id: None,
            at: 0,
            body: dirigent_protocol::transcript::Body::User { content: vec![] },
        };
        for seq in [0, 2, 3] {
            cache.add_entry("h", "t", &entry(seq)).unwrap();
        }
        let seqs = |cache: &Cache| cache.entries("h", "t").unwrap().iter().map(|e| e.seq).collect::<Vec<_>>();
        cache.set_entries("h", "t", &[entry(0), entry(1), entry(2)]).unwrap();
        assert_eq!(seqs(&cache), [0, 1, 2, 3]);
        cache.remove_entries("h", "t", Some(2), 3).unwrap();
        assert_eq!(seqs(&cache), [0, 1, 2]);
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

        cache.set_draft("h", "t", &content, &images[1..]).unwrap();
        assert_eq!(cache.draft("h", "t").unwrap().1.len(), 1);

        cache.set_threads("h", &[thread("other")]).unwrap();
        assert!(cache.draft("h", "t").is_none());
        assert_eq!(cache.draft("", "").unwrap().0.text, "new");

        cache.remove_draft("", "").unwrap();
        assert!(cache.draft("", "").is_none());
    }
}
