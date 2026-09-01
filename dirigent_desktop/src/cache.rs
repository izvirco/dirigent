//! Caches Pi sessions, drafts, models, and reasoning levels in SQLite.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
};

use async_channel::Receiver;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::platform;

pub(crate) struct CachedSession {
    pub(crate) entries: Option<Vec<Value>>,
    pub(crate) leaf_id: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) thinking_level: Option<String>,
    pub(crate) composer_draft: String,
    pub(crate) composer_images_json: Vec<u8>,
}

type CacheTask = Box<dyn FnOnce(&CacheDatabase) + Send + 'static>;

pub(crate) struct SessionCache {
    tasks: Option<mpsc::Sender<CacheTask>>,
    worker: Option<thread::JoinHandle<()>>,
    errors: Receiver<String>,
    error_tx: async_channel::Sender<String>,
}

struct CacheDatabase {
    connection: Connection,
}

impl SessionCache {
    pub(crate) fn open() -> Result<Self, String> {
        let path = cache_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        let connection = Connection::open(&path)
            .map_err(|error| format!("could not open {}: {error}", path.display()))?;
        let database = CacheDatabase::initialize(connection)
            .map_err(|error| format!("could not initialize {}: {error}", path.display()))?;
        Self::start(database)
    }

    fn start(database: CacheDatabase) -> Result<Self, String> {
        let (task_tx, task_rx) = mpsc::channel::<CacheTask>();
        let (error_tx, error_rx) = async_channel::unbounded();
        let worker = thread::Builder::new()
            .name("dirigent-cache".into())
            .spawn(move || {
                while let Ok(task) = task_rx.recv() {
                    task(&database);
                }
            })
            .map_err(|error| format!("could not start the cache worker: {error}"))?;
        Ok(Self {
            tasks: Some(task_tx),
            worker: Some(worker),
            errors: error_rx,
            error_tx,
        })
    }

    pub(crate) fn errors(&self) -> Receiver<String> {
        self.errors.clone()
    }

    /// Runs a read on the SQLite owner thread and waits for its result.
    fn request<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&CacheDatabase) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        self.tasks
            .as_ref()
            .expect("cache worker sender is available until drop")
            .send(Box::new(move |database| {
                let _ = result_tx.send(operation(database));
            }))
            .map_err(|_| "cache worker stopped unexpectedly".to_string())?;
        result_rx
            .recv()
            .map_err(|_| "cache worker stopped unexpectedly".to_string())?
    }

    /// Queues a best-effort write; failures are delivered asynchronously to the UI.
    fn execute(
        &self,
        operation: impl FnOnce(&CacheDatabase) -> Result<(), String> + Send + 'static,
    ) -> Result<(), String> {
        let error_tx = self.error_tx.clone();
        self.tasks
            .as_ref()
            .expect("cache worker sender is available until drop")
            .send(Box::new(move |database| {
                if let Err(error) = operation(database) {
                    let _ = error_tx.send_blocking(error);
                }
            }))
            .map_err(|_| "cache worker stopped unexpectedly".to_string())
    }

    pub(crate) fn load_session(
        &self,
        session_file: &Path,
    ) -> Result<Option<CachedSession>, String> {
        let session_file = session_file.to_path_buf();
        self.request(move |database| database.load_session(&session_file))
    }

    pub(crate) fn save_entries(
        &self,
        session_file: &Path,
        entries: Arc<Vec<Value>>,
        leaf_id: Option<&str>,
    ) -> Result<(), String> {
        let session_file = session_file.to_path_buf();
        let leaf_id = leaf_id.map(str::to_string);
        self.execute(move |database| {
            database.save_entries(&session_file, &entries, leaf_id.as_deref())
        })
    }

    /// Persists only the newly received entries on the UI thread. The cache worker merges and
    /// serializes the complete snapshot.
    pub(crate) fn append_entries(
        &self,
        session_file: &Path,
        entries: Vec<Value>,
        leaf_id: Option<&str>,
    ) -> Result<(), String> {
        let session_file = session_file.to_path_buf();
        let leaf_id = leaf_id.map(str::to_string);
        self.execute(move |database| {
            database.append_entries(&session_file, entries, leaf_id.as_deref())
        })
    }

    pub(crate) fn save_session_state(
        &self,
        session_file: &Path,
        model: Option<&str>,
        thinking_level: Option<&str>,
    ) -> Result<(), String> {
        let session_file = session_file.to_path_buf();
        let model = model.map(str::to_string);
        let thinking_level = thinking_level.map(str::to_string);
        self.execute(move |database| {
            database.save_session_state(&session_file, model.as_deref(), thinking_level.as_deref())
        })
    }

    pub(crate) fn save_composer_text(
        &self,
        session_file: &Path,
        composer_draft: &str,
    ) -> Result<(), String> {
        let session_file = session_file.to_path_buf();
        let composer_draft = composer_draft.to_string();
        self.execute(move |database| database.save_composer_text(&session_file, &composer_draft))
    }

    pub(crate) fn save_composer_draft(
        &self,
        session_file: &Path,
        composer_draft: &str,
        composer_images_json: &[u8],
    ) -> Result<(), String> {
        let session_file = session_file.to_path_buf();
        let composer_draft = composer_draft.to_string();
        let composer_images_json = composer_images_json.to_vec();
        self.execute(move |database| {
            database.save_composer_draft(&session_file, &composer_draft, &composer_images_json)
        })
    }

    pub(crate) fn load_models(&self, project_path: &Path) -> Result<Option<Vec<u8>>, String> {
        let project_path = project_path.to_path_buf();
        self.request(move |database| database.load_models(&project_path))
    }

    pub(crate) fn save_models(
        &self,
        project_path: &Path,
        models_json: &[u8],
    ) -> Result<(), String> {
        let project_path = project_path.to_path_buf();
        let models_json = models_json.to_vec();
        self.execute(move |database| database.save_models(&project_path, &models_json))
    }

    pub(crate) fn load_thinking_levels(
        &self,
        project_path: &Path,
    ) -> Result<Vec<(String, Vec<String>)>, String> {
        let project_path = project_path.to_path_buf();
        self.request(move |database| database.load_thinking_levels(&project_path))
    }

    pub(crate) fn save_thinking_levels(
        &self,
        project_path: &Path,
        model: &str,
        levels: &[String],
    ) -> Result<(), String> {
        let project_path = project_path.to_path_buf();
        let model = model.to_string();
        let levels = levels.to_vec();
        self.execute(move |database| database.save_thinking_levels(&project_path, &model, &levels))
    }
}

impl Drop for SessionCache {
    fn drop(&mut self) {
        // Disconnect only after all queued writes have been sent, then let the worker
        // drain them before the application exits.
        self.tasks.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl CacheDatabase {
    fn initialize(connection: Connection) -> rusqlite::Result<Self> {
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS session_cache (
                 session_file BLOB PRIMARY KEY,
                 entries_json BLOB,
                 leaf_id TEXT,
                 model TEXT,
                 thinking_level TEXT,
                 composer_draft TEXT NOT NULL DEFAULT '',
                 composer_images_json BLOB NOT NULL DEFAULT X'5B5D',
                 updated_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE IF NOT EXISTS model_cache (
                 project_path BLOB PRIMARY KEY,
                 models_json BLOB NOT NULL,
                 updated_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE IF NOT EXISTS thinking_level_cache (
                 project_path BLOB NOT NULL,
                 model TEXT NOT NULL,
                 levels_json BLOB NOT NULL,
                 updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
                 PRIMARY KEY (project_path, model)
             );",
        )?;
        let has_composer_draft = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('session_cache') WHERE name = 'composer_draft'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_composer_draft {
            connection.execute(
                "ALTER TABLE session_cache
                 ADD COLUMN composer_draft TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        let has_composer_images = connection.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM pragma_table_info('session_cache')
                WHERE name = 'composer_images_json'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_composer_images {
            connection.execute(
                "ALTER TABLE session_cache
                 ADD COLUMN composer_images_json BLOB NOT NULL DEFAULT X'5B5D'",
                [],
            )?;
        }
        Ok(Self { connection })
    }

    fn load_session(&self, session_file: &Path) -> Result<Option<CachedSession>, String> {
        let row = self
            .connection
            .query_row(
                "SELECT entries_json, leaf_id, model, thinking_level, composer_draft,
                        composer_images_json
                 FROM session_cache WHERE session_file = ?1",
                params![path_key(session_file)],
                |row| {
                    Ok((
                        row.get::<_, Option<Vec<u8>>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| format!("could not read the session cache: {error}"))?;
        let Some((
            entries_json,
            leaf_id,
            model,
            thinking_level,
            composer_draft,
            composer_images_json,
        )) = row
        else {
            return Ok(None);
        };
        let entries = entries_json
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|error| format!("could not decode cached session entries: {error}"))
            })
            .transpose()?;
        Ok(Some(CachedSession {
            entries,
            leaf_id,
            model,
            thinking_level,
            composer_draft,
            composer_images_json,
        }))
    }

    fn save_entries(
        &self,
        session_file: &Path,
        entries: &[Value],
        leaf_id: Option<&str>,
    ) -> Result<(), String> {
        let entries_json = serde_json::to_vec(entries)
            .map_err(|error| format!("could not encode session entries for caching: {error}"))?;
        self.connection
            .execute(
                "INSERT INTO session_cache (session_file, entries_json, leaf_id)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_file) DO UPDATE SET
                     entries_json = excluded.entries_json,
                     leaf_id = excluded.leaf_id,
                     updated_at = unixepoch()",
                params![path_key(session_file), entries_json, leaf_id],
            )
            .map_err(|error| format!("could not update the session cache: {error}"))?;
        Ok(())
    }

    fn append_entries(
        &self,
        session_file: &Path,
        incoming: Vec<Value>,
        leaf_id: Option<&str>,
    ) -> Result<(), String> {
        let entries_json = self
            .connection
            .query_row(
                "SELECT entries_json FROM session_cache WHERE session_file = ?1",
                params![path_key(session_file)],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()
            .map_err(|error| format!("could not read cached session entries: {error}"))?
            .flatten();
        let mut entries = entries_json
            .map(|bytes| {
                serde_json::from_slice::<Vec<Value>>(&bytes)
                    .map_err(|error| format!("could not decode cached session entries: {error}"))
            })
            .transpose()?
            .unwrap_or_default();
        let mut known_ids = entries
            .iter()
            .filter_map(|entry| entry.get("id")?.as_str().map(str::to_string))
            .collect::<HashSet<_>>();
        entries.extend(incoming.into_iter().filter(|entry| {
            entry
                .get("id")
                .and_then(Value::as_str)
                .is_none_or(|id| known_ids.insert(id.to_string()))
        }));
        self.save_entries(session_file, &entries, leaf_id)
    }

    fn save_session_state(
        &self,
        session_file: &Path,
        model: Option<&str>,
        thinking_level: Option<&str>,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO session_cache (session_file, model, thinking_level)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_file) DO UPDATE SET
                     model = excluded.model,
                     thinking_level = excluded.thinking_level,
                     updated_at = unixepoch()",
                params![path_key(session_file), model, thinking_level],
            )
            .map_err(|error| format!("could not update cached Pi state: {error}"))?;
        Ok(())
    }

    fn save_composer_text(&self, session_file: &Path, composer_draft: &str) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO session_cache (session_file, composer_draft)
                 VALUES (?1, ?2)
                 ON CONFLICT(session_file) DO UPDATE SET
                     composer_draft = excluded.composer_draft,
                     updated_at = unixepoch()",
                params![path_key(session_file), composer_draft],
            )
            .map_err(|error| format!("could not update the cached composer draft: {error}"))?;
        Ok(())
    }

    fn save_composer_draft(
        &self,
        session_file: &Path,
        composer_draft: &str,
        composer_images_json: &[u8],
    ) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO session_cache
                    (session_file, composer_draft, composer_images_json)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_file) DO UPDATE SET
                     composer_draft = excluded.composer_draft,
                     composer_images_json = excluded.composer_images_json,
                     updated_at = unixepoch()",
                params![path_key(session_file), composer_draft, composer_images_json],
            )
            .map_err(|error| format!("could not update the cached composer draft: {error}"))?;
        Ok(())
    }

    fn load_models(&self, project_path: &Path) -> Result<Option<Vec<u8>>, String> {
        self.connection
            .query_row(
                "SELECT models_json FROM model_cache WHERE project_path = ?1",
                params![path_key(project_path)],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("could not read cached models: {error}"))
    }

    fn save_models(&self, project_path: &Path, models_json: &[u8]) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO model_cache (project_path, models_json) VALUES (?1, ?2)
                 ON CONFLICT(project_path) DO UPDATE SET
                     models_json = excluded.models_json,
                     updated_at = unixepoch()",
                params![path_key(project_path), models_json],
            )
            .map_err(|error| format!("could not update cached models: {error}"))?;
        Ok(())
    }

    fn load_thinking_levels(
        &self,
        project_path: &Path,
    ) -> Result<Vec<(String, Vec<String>)>, String> {
        let mut statement = self
            .connection
            .prepare("SELECT model, levels_json FROM thinking_level_cache WHERE project_path = ?1")
            .map_err(|error| format!("could not prepare cached thinking levels: {error}"))?;
        let rows = statement
            .query_map(params![path_key(project_path)], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|error| format!("could not read cached thinking levels: {error}"))?;
        let mut levels = Vec::new();
        for row in rows {
            let (model, levels_json) =
                row.map_err(|error| format!("could not read cached thinking levels: {error}"))?;
            let model_levels = serde_json::from_slice(&levels_json)
                .map_err(|error| format!("could not decode cached thinking levels: {error}"))?;
            levels.push((model, model_levels));
        }
        Ok(levels)
    }

    fn save_thinking_levels(
        &self,
        project_path: &Path,
        model: &str,
        levels: &[String],
    ) -> Result<(), String> {
        let levels_json = serde_json::to_vec(levels)
            .map_err(|error| format!("could not encode thinking levels for caching: {error}"))?;
        self.connection
            .execute(
                "INSERT INTO thinking_level_cache (project_path, model, levels_json)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(project_path, model) DO UPDATE SET
                     levels_json = excluded.levels_json,
                     updated_at = unixepoch()",
                params![path_key(project_path), model, levels_json],
            )
            .map_err(|error| format!("could not update cached thinking levels: {error}"))?;
        Ok(())
    }
}

fn path_key(path: &Path) -> Vec<u8> {
    // Keep the platform-native encoded path rather than a lossy display string. The cache is
    // local to this machine, so portability of these keys is neither needed nor desirable.
    path.as_os_str().as_encoded_bytes().to_vec()
}

fn cache_path() -> Result<PathBuf, String> {
    platform::cache_path()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn appends_only_new_session_entries() {
        let database = CacheDatabase::initialize(
            Connection::open_in_memory().expect("in-memory cache should open"),
        )
        .expect("cache should initialize");
        let session = Path::new("session.jsonl");
        database
            .save_entries(session, &[json!({"id": "one"})], Some("one"))
            .expect("initial entries should save");
        database
            .append_entries(
                session,
                vec![json!({"id": "one"}), json!({"id": "two"})],
                Some("two"),
            )
            .expect("incremental entries should append");

        let cached = database
            .load_session(session)
            .expect("cache should load")
            .expect("session should exist");
        let entries = cached.entries.expect("entries should exist");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].get("id").and_then(Value::as_str), Some("two"));
        assert_eq!(cached.leaf_id.as_deref(), Some("two"));
    }
}
