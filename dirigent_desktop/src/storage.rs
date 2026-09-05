//! Persists durable application state and turn diffs in SQLite.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    diff::{DiffViewMode, TurnDiff},
    model::{Harness, Id, ManagedWorkspace, Project, WorkspaceBackend, WorkspaceState},
    platform,
};

pub(crate) const DEFAULT_SIDEBAR_WIDTH: f32 = 288.0;
pub(crate) const DEFAULT_DIFF_SIDEBAR_WIDTH: f32 = 560.0;
const SCHEMA_VERSION: i64 = 7;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS app_state (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        next_id TEXT NOT NULL,
        next_sidebar_order TEXT NOT NULL,
        last_used_harness TEXT,
        sidebar_width REAL NOT NULL,
        diff_sidebar_open INTEGER NOT NULL DEFAULT 0,
        diff_sidebar_width REAL NOT NULL DEFAULT 560,
        diff_view_mode_json BLOB NOT NULL DEFAULT X'22556E696669656422'
    );
    CREATE TABLE IF NOT EXISTS projects (
        id TEXT PRIMARY KEY,
        order_index INTEGER NOT NULL UNIQUE,
        name TEXT NOT NULL,
        path_json BLOB NOT NULL,
        workspace_root_json BLOB,
        keep_active_threads_in_project INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE IF NOT EXISTS harnesses (
        id TEXT PRIMARY KEY,
        order_index INTEGER NOT NULL UNIQUE,
        project_id TEXT NOT NULL,
        title TEXT NOT NULL,
        session_file_json BLOB,
        nix_enabled INTEGER NOT NULL,
        workspace_id TEXT,
        last_vcs_label TEXT,
        archived INTEGER NOT NULL,
        sidebar_order TEXT NOT NULL,
        turn_diffs_json BLOB NOT NULL DEFAULT X'5B5D',
        work_group_expansion_json BLOB NOT NULL DEFAULT X'7B7D',
        delegation_json BLOB NOT NULL DEFAULT X'7B7D'
    );
    CREATE TABLE IF NOT EXISTS turn_diffs (
        harness_id TEXT NOT NULL,
        turn_id TEXT NOT NULL,
        diff_json BLOB NOT NULL,
        PRIMARY KEY (harness_id, turn_id)
    );
    CREATE TABLE IF NOT EXISTS workspaces (
        id TEXT PRIMARY KEY,
        order_index INTEGER NOT NULL UNIQUE,
        project_id TEXT NOT NULL,
        backend_json BLOB NOT NULL,
        root_json BLOB NOT NULL,
        working_directory_json BLOB NOT NULL,
        source_repository_json BLOB NOT NULL,
        source_id TEXT NOT NULL,
        source_label TEXT NOT NULL,
        source_revision TEXT NOT NULL,
        jj_parent_revisions_json BLOB NOT NULL,
        git_branch TEXT,
        state_json BLOB NOT NULL
    );
    CREATE TABLE IF NOT EXISTS collapsed_projects (
        project_id TEXT PRIMARY KEY
    );
";

struct StoredState {
    next_id: Id,
    next_sidebar_order: u64,
    projects: Vec<StoredProject>,
    harnesses: Vec<StoredHarness>,
    workspaces: Vec<StoredWorkspace>,
    last_used_harness: Option<Id>,
    collapsed_projects: Vec<Id>,
    sidebar_width: f32,
    diff_sidebar_open: bool,
    diff_sidebar_width: f32,
    diff_view_mode: DiffViewMode,
}

struct StoredMetadata {
    next_id: Id,
    next_sidebar_order: u64,
    projects: Vec<StoredProject>,
    harnesses: Vec<StoredHarnessMetadata>,
    workspaces: Vec<StoredWorkspace>,
    last_used_harness: Option<Id>,
    collapsed_projects: Vec<Id>,
    sidebar_width: f32,
    diff_sidebar_open: bool,
    diff_sidebar_width: f32,
    diff_view_mode: DiffViewMode,
}

impl StoredState {
    fn empty() -> Self {
        Self {
            next_id: 1,
            next_sidebar_order: 1,
            projects: Vec::new(),
            harnesses: Vec::new(),
            workspaces: Vec::new(),
            last_used_harness: None,
            collapsed_projects: Vec::new(),
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            diff_sidebar_open: false,
            diff_sidebar_width: DEFAULT_DIFF_SIDEBAR_WIDTH,
            diff_view_mode: DiffViewMode::Unified,
        }
    }
}

struct StoredProject {
    id: Id,
    name: String,
    path: PathBuf,
    workspace_root: Option<PathBuf>,
    keep_active_threads_in_project: bool,
}

struct StoredHarness {
    delegation: crate::delegation::Delegation,
    id: Id,
    project_id: Id,
    title: String,
    session_file: Option<PathBuf>,
    nix_enabled: bool,
    workspace_id: Option<String>,
    last_vcs_label: Option<String>,
    archived: bool,
    sidebar_order: u64,
    turn_diffs: Vec<TurnDiff>,
    work_group_expansion: HashMap<String, bool>,
}

struct StoredHarnessMetadata {
    delegation: crate::delegation::Delegation,
    id: Id,
    project_id: Id,
    title: String,
    session_file: Option<PathBuf>,
    nix_enabled: bool,
    workspace_id: Option<String>,
    last_vcs_label: Option<String>,
    archived: bool,
    sidebar_order: u64,
    work_group_expansion: HashMap<String, bool>,
}

struct StoredWorkspace {
    id: String,
    project_id: Id,
    backend: WorkspaceBackend,
    root: PathBuf,
    working_directory: PathBuf,
    source_repository: PathBuf,
    source_id: String,
    source_label: String,
    source_revision: String,
    jj_parent_revisions: Vec<String>,
    git_branch: Option<String>,
    state: WorkspaceState,
}

pub(crate) struct LoadedState {
    pub(crate) projects: Vec<Project>,
    pub(crate) harnesses: Vec<Harness>,
    pub(crate) workspaces: Vec<ManagedWorkspace>,
    pub(crate) next_id: Id,
    pub(crate) next_sidebar_order: u64,
    pub(crate) last_used_harness: Option<Id>,
    pub(crate) collapsed_projects: HashSet<Id>,
    pub(crate) sidebar_width: f32,
    pub(crate) diff_sidebar_open: bool,
    pub(crate) diff_sidebar_width: f32,
    pub(crate) diff_view_mode: DiffViewMode,
}

enum StorageCommand {
    SaveMetadata(StoredMetadata),
    SaveDiffSidebar {
        open: bool,
        width: f32,
        view_mode: DiffViewMode,
    },
    SaveSidebarLayout {
        sidebar_width: f32,
        diff_sidebar_width: f32,
    },
    SaveLastUsedHarness(Option<Id>),
    SaveHarnessSessionFile {
        harness_id: Id,
        session_file: Option<PathBuf>,
    },
    SaveTurnDiff {
        harness_id: Id,
        turn: Arc<TurnDiff>,
    },
    RetainTurnDiffs {
        harness_id: Id,
        turn_ids: Vec<u64>,
    },
    CopyTurnDiffs {
        source_harness_id: Id,
        target_harness_id: Id,
    },
    Shutdown,
}

pub(crate) struct StateDatabase {
    sender: Sender<StorageCommand>,
    worker: Option<JoinHandle<()>>,
}

impl StateDatabase {
    pub(crate) fn open() -> Result<(Self, LoadedState), String> {
        let database_path = platform::state_database_path()?;
        Self::open_at(&database_path)
    }

    fn open_at(database_path: &Path) -> Result<(Self, LoadedState), String> {
        // Startup reads synchronously once, then transfers sole connection ownership to the
        // worker so later GPUI updates only enqueue commands.
        let connection = open_database(database_path)?;
        let loaded = read_stored_state(&connection).map(StoredState::into_loaded)?;
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("dirigent-storage".into())
            .spawn(move || run_storage_worker(connection, receiver))
            .map_err(|error| format!("could not start state storage worker: {error}"))?;
        Ok((
            Self {
                sender,
                worker: Some(worker),
            },
            loaded,
        ))
    }

    fn send(&self, command: StorageCommand) -> Result<(), String> {
        self.sender
            .send(command)
            .map_err(|_| "application state storage worker stopped unexpectedly".to_string())
    }

    pub(crate) fn save_diff_sidebar(
        &self,
        open: bool,
        width: f32,
        view_mode: DiffViewMode,
    ) -> Result<(), String> {
        self.send(StorageCommand::SaveDiffSidebar {
            open,
            width,
            view_mode,
        })
    }

    pub(crate) fn save_sidebar_layout(
        &self,
        sidebar_width: f32,
        diff_sidebar_width: f32,
    ) -> Result<(), String> {
        self.send(StorageCommand::SaveSidebarLayout {
            sidebar_width,
            diff_sidebar_width,
        })
    }

    pub(crate) fn save_last_used_harness(&self, harness_id: Option<Id>) -> Result<(), String> {
        self.send(StorageCommand::SaveLastUsedHarness(harness_id))
    }

    pub(crate) fn save_harness_session_file(
        &self,
        harness_id: Id,
        session_file: Option<&Path>,
    ) -> Result<(), String> {
        self.send(StorageCommand::SaveHarnessSessionFile {
            harness_id,
            session_file: session_file.map(Path::to_path_buf),
        })
    }

    pub(crate) fn save_turn_diff(&self, harness_id: Id, turn: Arc<TurnDiff>) -> Result<(), String> {
        self.send(StorageCommand::SaveTurnDiff { harness_id, turn })
    }

    pub(crate) fn retain_turn_diffs(
        &self,
        harness_id: Id,
        turn_ids: Vec<u64>,
    ) -> Result<(), String> {
        self.send(StorageCommand::RetainTurnDiffs {
            harness_id,
            turn_ids,
        })
    }

    pub(crate) fn copy_turn_diffs(
        &self,
        source_harness_id: Id,
        target_harness_id: Id,
    ) -> Result<(), String> {
        self.send(StorageCommand::CopyTurnDiffs {
            source_harness_id,
            target_harness_id,
        })
    }

    /// Queues a full metadata reconciliation; normalized turn diffs are persisted separately.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn save(
        &self,
        projects: &[Project],
        harnesses: &[Harness],
        workspaces: &[ManagedWorkspace],
        next_id: Id,
        next_sidebar_order: u64,
        last_used_harness: Option<Id>,
        collapsed_projects: &HashSet<Id>,
        sidebar_width: f32,
        diff_sidebar_open: bool,
        diff_sidebar_width: f32,
        diff_view_mode: DiffViewMode,
    ) -> Result<(), String> {
        let mut collapsed_projects = collapsed_projects.iter().copied().collect::<Vec<_>>();
        collapsed_projects.sort_unstable();
        self.send(StorageCommand::SaveMetadata(StoredMetadata {
            next_id,
            next_sidebar_order,
            projects: projects
                .iter()
                .map(|project| StoredProject {
                    id: project.id,
                    name: project.name.clone(),
                    path: project.path.clone(),
                    workspace_root: project.workspace_root.clone(),
                    keep_active_threads_in_project: project.keep_active_threads_in_project,
                })
                .collect(),
            harnesses: harnesses
                .iter()
                .map(|harness| StoredHarnessMetadata {
                    delegation: harness.delegation.clone(),
                    id: harness.id,
                    project_id: harness.project_id,
                    title: harness.title.clone(),
                    session_file: harness.session_file.clone(),
                    nix_enabled: harness.nix_enabled,
                    workspace_id: harness.workspace_id.clone(),
                    last_vcs_label: harness.last_vcs_label.clone(),
                    archived: harness.archived,
                    sidebar_order: harness.sidebar_order,
                    work_group_expansion: harness.work_group_expansion.clone(),
                })
                .collect(),
            workspaces: workspaces
                .iter()
                .map(|workspace| StoredWorkspace {
                    id: workspace.id.clone(),
                    project_id: workspace.project_id,
                    backend: workspace.backend,
                    root: workspace.root.clone(),
                    working_directory: workspace.working_directory.clone(),
                    source_repository: workspace.source_repository.clone(),
                    source_id: workspace.source_id.clone(),
                    source_label: workspace.source_label.clone(),
                    source_revision: workspace.source_revision.clone(),
                    jj_parent_revisions: workspace.jj_parent_revisions.clone(),
                    git_branch: workspace.git_branch.clone(),
                    state: workspace.state.clone(),
                })
                .collect(),
            last_used_harness,
            collapsed_projects,
            sidebar_width,
            diff_sidebar_open,
            diff_sidebar_width,
            diff_view_mode,
        }))
    }
}

impl Drop for StateDatabase {
    fn drop(&mut self) {
        let _ = self.sender.send(StorageCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Owns the SQLite connection and executes persistence commands in application order.
fn run_storage_worker(mut connection: Connection, receiver: Receiver<StorageCommand>) {
    while let Ok(command) = receiver.recv() {
        if matches!(command, StorageCommand::Shutdown) {
            break;
        }
        let started = Instant::now();
        let operation = storage_command_name(&command);
        let result = execute_storage_command(&mut connection, command);
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_millis(100) {
            tracing::warn!(
                operation,
                elapsed_ms = elapsed.as_millis(),
                "slow asynchronous application state persistence"
            );
        }
        if let Err(error) = result {
            tracing::error!(operation, error = %error, "could not persist application state");
        }
    }
}

fn storage_command_name(command: &StorageCommand) -> &'static str {
    match command {
        StorageCommand::SaveMetadata(_) => "metadata",
        StorageCommand::SaveDiffSidebar { .. } => "diff_sidebar",
        StorageCommand::SaveSidebarLayout { .. } => "sidebar_layout",
        StorageCommand::SaveLastUsedHarness(_) => "last_used_harness",
        StorageCommand::SaveHarnessSessionFile { .. } => "session_file",
        StorageCommand::SaveTurnDiff { .. } => "turn_diff",
        StorageCommand::RetainTurnDiffs { .. } => "retain_turn_diffs",
        StorageCommand::CopyTurnDiffs { .. } => "copy_turn_diffs",
        StorageCommand::Shutdown => "shutdown",
    }
}

fn execute_storage_command(
    connection: &mut Connection,
    command: StorageCommand,
) -> Result<(), String> {
    match command {
        StorageCommand::SaveMetadata(state) => sync_stored_metadata(connection, &state),
        StorageCommand::SaveDiffSidebar {
            open,
            width,
            view_mode,
        } => {
            connection
                .execute(
                    "UPDATE app_state SET
                         diff_sidebar_open = ?1,
                         diff_sidebar_width = ?2,
                         diff_view_mode_json = ?3
                     WHERE singleton = 1",
                    params![open, width, encode_json(&view_mode, "diff view mode")?],
                )
                .map_err(|error| format!("could not store diff sidebar state: {error}"))?;
            Ok(())
        }
        StorageCommand::SaveSidebarLayout {
            sidebar_width,
            diff_sidebar_width,
        } => {
            connection
                .execute(
                    "UPDATE app_state SET sidebar_width = ?1, diff_sidebar_width = ?2
                     WHERE singleton = 1",
                    params![sidebar_width, diff_sidebar_width],
                )
                .map_err(|error| format!("could not store sidebar layout: {error}"))?;
            Ok(())
        }
        StorageCommand::SaveLastUsedHarness(harness_id) => {
            connection
                .execute(
                    "UPDATE app_state SET last_used_harness = ?1 WHERE singleton = 1",
                    params![harness_id.map(|id| id.to_string())],
                )
                .map_err(|error| format!("could not store last used thread: {error}"))?;
            Ok(())
        }
        StorageCommand::SaveHarnessSessionFile {
            harness_id,
            session_file,
        } => {
            let session_file_json = session_file
                .as_deref()
                .map(|path| encode_json(path, "harness session path"))
                .transpose()?;
            connection
                .execute(
                    "UPDATE harnesses SET session_file_json = ?1 WHERE id = ?2",
                    params![session_file_json, harness_id.to_string()],
                )
                .map_err(|error| format!("could not store thread session path: {error}"))?;
            Ok(())
        }
        StorageCommand::SaveTurnDiff { harness_id, turn } => {
            save_turn_diff(connection, harness_id, turn.as_ref())
        }
        StorageCommand::RetainTurnDiffs {
            harness_id,
            turn_ids,
        } => retain_turn_diffs(connection, harness_id, &turn_ids),
        StorageCommand::CopyTurnDiffs {
            source_harness_id,
            target_harness_id,
        } => {
            connection
                .execute(
                    "INSERT OR REPLACE INTO turn_diffs (harness_id, turn_id, diff_json)
                     SELECT ?1, turn_id, diff_json FROM turn_diffs WHERE harness_id = ?2",
                    params![target_harness_id.to_string(), source_harness_id.to_string()],
                )
                .map_err(|error| {
                    format!(
                        "could not copy thread {source_harness_id} turns to {target_harness_id}: {error}"
                    )
                })?;
            Ok(())
        }
        StorageCommand::Shutdown => Ok(()),
    }
}

fn open_database(database_path: &Path) -> Result<Connection, String> {
    if let Some(parent) = database_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let mut connection = Connection::open(database_path)
        .map_err(|error| format!("could not open {}: {error}", database_path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("could not configure {}: {error}", database_path.display()))?;
    initialize_schema(&connection, database_path)?;

    let has_state = connection
        .query_row("SELECT EXISTS(SELECT 1 FROM app_state)", [], |row| {
            row.get::<_, bool>(0)
        })
        .map_err(|error| format!("could not inspect {}: {error}", database_path.display()))?;

    if !has_state {
        write_stored_state(&mut connection, &StoredState::empty())?;
    }

    Ok(connection)
}

fn initialize_schema(connection: &Connection, database_path: &Path) -> Result<(), String> {
    let version = connection
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map_err(|error| format!("could not inspect {}: {error}", database_path.display()))?;
    if version > SCHEMA_VERSION {
        return Err(format!(
            "{} uses state schema version {version}, but this version of Dirigent supports up to {SCHEMA_VERSION}",
            database_path.display()
        ));
    }
    connection
        .execute_batch(&format!(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             {SCHEMA}"
        ))
        .map_err(|error| format!("could not initialize {}: {error}", database_path.display()))?;
    let migrations = [
        (
            "harnesses",
            "delegation_json",
            "ALTER TABLE harnesses ADD COLUMN delegation_json BLOB NOT NULL DEFAULT X'7B7D';",
        ),
        (
            "app_state",
            "diff_sidebar_open",
            "ALTER TABLE app_state ADD COLUMN diff_sidebar_open INTEGER NOT NULL DEFAULT 0;",
        ),
        (
            "app_state",
            "diff_sidebar_width",
            "ALTER TABLE app_state ADD COLUMN diff_sidebar_width REAL NOT NULL DEFAULT 560;",
        ),
        (
            "app_state",
            "diff_view_mode_json",
            "ALTER TABLE app_state ADD COLUMN diff_view_mode_json BLOB NOT NULL DEFAULT X'22556E696669656422';",
        ),
        (
            "harnesses",
            "turn_diffs_json",
            "ALTER TABLE harnesses ADD COLUMN turn_diffs_json BLOB NOT NULL DEFAULT X'5B5D';",
        ),
        (
            "harnesses",
            "last_vcs_label",
            "ALTER TABLE harnesses ADD COLUMN last_vcs_label TEXT;",
        ),
        (
            "harnesses",
            "work_group_expansion_json",
            "ALTER TABLE harnesses ADD COLUMN work_group_expansion_json BLOB NOT NULL DEFAULT X'7B7D';",
        ),
        (
            "projects",
            "keep_active_threads_in_project",
            "ALTER TABLE projects ADD COLUMN keep_active_threads_in_project INTEGER NOT NULL DEFAULT 0;",
        ),
    ];
    for (table, column, migration) in migrations {
        if !table_has_column(connection, table, column).map_err(|error| {
            format!(
                "could not inspect {} for migration: {error}",
                database_path.display()
            )
        })? {
            connection.execute_batch(migration).map_err(|error| {
                format!("could not migrate {}: {error}", database_path.display())
            })?;
        }
    }
    if version < 6 {
        migrate_normalized_turn_diffs(connection, database_path)?;
    }
    connection
        .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
        .map_err(|error| format!("could not update {}: {error}", database_path.display()))
}

fn migrate_normalized_turn_diffs(
    connection: &Connection,
    database_path: &Path,
) -> Result<(), String> {
    let mut statement = connection
        .prepare("SELECT id, turn_diffs_json FROM harnesses")
        .map_err(|error| {
            format!(
                "could not prepare {} turn diff migration: {error}",
                database_path.display()
            )
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|error| {
            format!(
                "could not read {} turn diffs for migration: {error}",
                database_path.display()
            )
        })?;
    let mut migrated = Vec::new();
    for row in rows {
        let (harness_id, bytes) = row.map_err(|error| {
            format!(
                "could not read {} turn diff migration row: {error}",
                database_path.display()
            )
        })?;
        migrated.push((harness_id, decode_turn_diffs(&bytes)?));
    }
    drop(statement);
    let transaction = connection.unchecked_transaction().map_err(|error| {
        format!(
            "could not begin {} turn diff migration: {error}",
            database_path.display()
        )
    })?;
    for (harness_id, turns) in migrated {
        for turn in turns {
            transaction
                .execute(
                    "INSERT OR REPLACE INTO turn_diffs (harness_id, turn_id, diff_json)
                     VALUES (?1, ?2, ?3)",
                    params![harness_id, turn.id.to_string(), encode_turn_diff(&turn)?],
                )
                .map_err(|error| {
                    format!(
                        "could not migrate {} turn diff: {error}",
                        database_path.display()
                    )
                })?;
        }
    }
    transaction
        .execute("UPDATE harnesses SET turn_diffs_json = X'5B5D'", [])
        .map_err(|error| {
            format!(
                "could not clear {} legacy turn diffs after migration: {error}",
                database_path.display()
            )
        })?;
    transaction.commit().map_err(|error| {
        format!(
            "could not commit {} turn diff migration: {error}",
            database_path.display()
        )
    })
}

fn table_has_column(connection: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn write_stored_state(connection: &mut Connection, state: &StoredState) -> Result<(), String> {
    let transaction = connection
        .transaction()
        .map_err(|error| format!("could not begin state transaction: {error}"))?;
    replace_state(&transaction, state)?;
    transaction
        .commit()
        .map_err(|error| format!("could not commit state transaction: {error}"))
}

fn replace_state(transaction: &Transaction<'_>, state: &StoredState) -> Result<(), String> {
    transaction
        .execute_batch(
            "DELETE FROM collapsed_projects;
             DELETE FROM turn_diffs;
             DELETE FROM harnesses;
             DELETE FROM workspaces;
             DELETE FROM projects;
             DELETE FROM app_state;",
        )
        .map_err(|error| format!("could not clear stored state: {error}"))?;

    transaction
        .execute(
            "INSERT INTO app_state (
                singleton, next_id, next_sidebar_order, last_used_harness, sidebar_width,
                diff_sidebar_open, diff_sidebar_width, diff_view_mode_json
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                state.next_id.to_string(),
                state.next_sidebar_order.to_string(),
                state.last_used_harness.map(|id| id.to_string()),
                state.sidebar_width,
                state.diff_sidebar_open,
                state.diff_sidebar_width,
                encode_json(&state.diff_view_mode, "diff view mode")?,
            ],
        )
        .map_err(|error| format!("could not store application state: {error}"))?;

    for (index, project) in state.projects.iter().enumerate() {
        let path_json = encode_json(&project.path, "project path")?;
        let workspace_root_json = project
            .workspace_root
            .as_ref()
            .map(|path| encode_json(path, "project workspace root"))
            .transpose()?;
        transaction
            .execute(
                "INSERT INTO projects (
                    id, order_index, name, path_json, workspace_root_json,
                    keep_active_threads_in_project
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    project.id.to_string(),
                    order_index(index)?,
                    &project.name,
                    path_json,
                    workspace_root_json,
                    project.keep_active_threads_in_project,
                ],
            )
            .map_err(|error| format!("could not store project {}: {error}", project.id))?;
    }

    for (index, harness) in state.harnesses.iter().enumerate() {
        let session_file_json = harness
            .session_file
            .as_ref()
            .map(|path| encode_json(path, "harness session path"))
            .transpose()?;
        transaction
            .execute(
                "INSERT INTO harnesses (
                    id, order_index, project_id, title, session_file_json, nix_enabled,
                    workspace_id, last_vcs_label, archived, sidebar_order,
                    work_group_expansion_json, delegation_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    harness.id.to_string(),
                    order_index(index)?,
                    harness.project_id.to_string(),
                    &harness.title,
                    session_file_json,
                    harness.nix_enabled,
                    &harness.workspace_id,
                    &harness.last_vcs_label,
                    harness.archived,
                    harness.sidebar_order.to_string(),
                    encode_json(&harness.work_group_expansion, "work group expansion")?,
                    encode_json(&harness.delegation, "delegation")?,
                ],
            )
            .map_err(|error| format!("could not store harness {}: {error}", harness.id))?;
        for turn in &harness.turn_diffs {
            transaction
                .execute(
                    "INSERT INTO turn_diffs (harness_id, turn_id, diff_json)
                     VALUES (?1, ?2, ?3)",
                    params![
                        harness.id.to_string(),
                        turn.id.to_string(),
                        encode_turn_diff(turn)?
                    ],
                )
                .map_err(|error| {
                    format!(
                        "could not store harness {} turn {}: {error}",
                        harness.id, turn.id
                    )
                })?;
        }
    }

    for (index, workspace) in state.workspaces.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO workspaces (
                    id, order_index, project_id, backend_json, root_json,
                    working_directory_json, source_repository_json, source_id, source_label,
                    source_revision, jj_parent_revisions_json, git_branch, state_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    &workspace.id,
                    order_index(index)?,
                    workspace.project_id.to_string(),
                    encode_json(&workspace.backend, "workspace backend")?,
                    encode_json(&workspace.root, "workspace root")?,
                    encode_json(&workspace.working_directory, "workspace working directory")?,
                    encode_json(&workspace.source_repository, "workspace source repository")?,
                    &workspace.source_id,
                    &workspace.source_label,
                    &workspace.source_revision,
                    encode_json(&workspace.jj_parent_revisions, "workspace parent revisions")?,
                    &workspace.git_branch,
                    encode_json(&workspace.state, "workspace state")?,
                ],
            )
            .map_err(|error| format!("could not store workspace {}: {error}", workspace.id))?;
    }

    for project_id in &state.collapsed_projects {
        transaction
            .execute(
                "INSERT INTO collapsed_projects (project_id) VALUES (?1)",
                params![project_id.to_string()],
            )
            .map_err(|error| format!("could not store collapsed project {project_id}: {error}"))?;
    }
    Ok(())
}

/// Reconciles mutable metadata without rewriting potentially large normalized turn diffs.
fn sync_stored_metadata(connection: &mut Connection, state: &StoredMetadata) -> Result<(), String> {
    let transaction = connection
        .transaction()
        .map_err(|error| format!("could not begin metadata transaction: {error}"))?;
    transaction
        .execute(
            "UPDATE app_state SET
                 next_id = ?1, next_sidebar_order = ?2, last_used_harness = ?3,
                 sidebar_width = ?4, diff_sidebar_open = ?5, diff_sidebar_width = ?6,
                 diff_view_mode_json = ?7
             WHERE singleton = 1",
            params![
                state.next_id.to_string(),
                state.next_sidebar_order.to_string(),
                state.last_used_harness.map(|id| id.to_string()),
                state.sidebar_width,
                state.diff_sidebar_open,
                state.diff_sidebar_width,
                encode_json(&state.diff_view_mode, "diff view mode")?,
            ],
        )
        .map_err(|error| format!("could not update application metadata: {error}"))?;

    prepare_table_order(
        &transaction,
        "projects",
        state
            .projects
            .iter()
            .enumerate()
            .map(|(index, project)| Ok((project.id.to_string(), order_index(index)?)))
            .collect::<Result<Vec<_>, String>>()?,
    )?;
    for (index, project) in state.projects.iter().enumerate() {
        let path_json = encode_json(&project.path, "project path")?;
        let workspace_root_json = project
            .workspace_root
            .as_ref()
            .map(|path| encode_json(path, "project workspace root"))
            .transpose()?;
        transaction
            .execute(
                "INSERT INTO projects (
                     id, order_index, name, path_json, workspace_root_json,
                     keep_active_threads_in_project
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                     order_index = excluded.order_index,
                     name = excluded.name,
                     path_json = excluded.path_json,
                     workspace_root_json = excluded.workspace_root_json,
                     keep_active_threads_in_project = excluded.keep_active_threads_in_project
                 WHERE projects.order_index IS NOT excluded.order_index
                    OR projects.name IS NOT excluded.name
                    OR projects.path_json IS NOT excluded.path_json
                    OR projects.workspace_root_json IS NOT excluded.workspace_root_json
                    OR projects.keep_active_threads_in_project IS NOT excluded.keep_active_threads_in_project",
                params![
                    project.id.to_string(),
                    order_index(index)?,
                    &project.name,
                    path_json,
                    workspace_root_json,
                    project.keep_active_threads_in_project,
                ],
            )
            .map_err(|error| format!("could not update project {}: {error}", project.id))?;
    }

    prepare_table_order(
        &transaction,
        "harnesses",
        state
            .harnesses
            .iter()
            .enumerate()
            .map(|(index, harness)| Ok((harness.id.to_string(), order_index(index)?)))
            .collect::<Result<Vec<_>, String>>()?,
    )?;
    transaction
        .execute(
            "DELETE FROM turn_diffs WHERE harness_id NOT IN (SELECT id FROM harnesses)",
            [],
        )
        .map_err(|error| format!("could not remove deleted thread diffs: {error}"))?;
    for (index, harness) in state.harnesses.iter().enumerate() {
        let session_file_json = harness
            .session_file
            .as_ref()
            .map(|path| encode_json(path, "harness session path"))
            .transpose()?;
        let work_group_expansion_json =
            encode_json(&harness.work_group_expansion, "work group expansion")?;
        transaction
            .execute(
                "INSERT INTO harnesses (
                     id, order_index, project_id, title, session_file_json, nix_enabled,
                     workspace_id, last_vcs_label, archived, sidebar_order,
                     work_group_expansion_json, delegation_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                 ON CONFLICT(id) DO UPDATE SET
                     order_index = excluded.order_index,
                     project_id = excluded.project_id,
                     title = excluded.title,
                     session_file_json = excluded.session_file_json,
                     nix_enabled = excluded.nix_enabled,
                     workspace_id = excluded.workspace_id,
                     last_vcs_label = excluded.last_vcs_label,
                     archived = excluded.archived,
                     sidebar_order = excluded.sidebar_order,
                     work_group_expansion_json = excluded.work_group_expansion_json,
                     delegation_json = excluded.delegation_json
                 WHERE harnesses.order_index IS NOT excluded.order_index
                    OR harnesses.project_id IS NOT excluded.project_id
                    OR harnesses.title IS NOT excluded.title
                    OR harnesses.session_file_json IS NOT excluded.session_file_json
                    OR harnesses.nix_enabled IS NOT excluded.nix_enabled
                    OR harnesses.workspace_id IS NOT excluded.workspace_id
                    OR harnesses.last_vcs_label IS NOT excluded.last_vcs_label
                    OR harnesses.archived IS NOT excluded.archived
                    OR harnesses.sidebar_order IS NOT excluded.sidebar_order
                    OR harnesses.work_group_expansion_json IS NOT excluded.work_group_expansion_json
                    OR harnesses.delegation_json IS NOT excluded.delegation_json",
                params![
                    harness.id.to_string(),
                    order_index(index)?,
                    harness.project_id.to_string(),
                    &harness.title,
                    session_file_json,
                    harness.nix_enabled,
                    &harness.workspace_id,
                    &harness.last_vcs_label,
                    harness.archived,
                    harness.sidebar_order.to_string(),
                    work_group_expansion_json,
                    encode_json(&harness.delegation, "delegation")?,
                ],
            )
            .map_err(|error| format!("could not update thread {}: {error}", harness.id))?;
    }

    prepare_table_order(
        &transaction,
        "workspaces",
        state
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, workspace)| Ok((workspace.id.clone(), order_index(index)?)))
            .collect::<Result<Vec<_>, String>>()?,
    )?;
    for (index, workspace) in state.workspaces.iter().enumerate() {
        let backend_json = encode_json(&workspace.backend, "workspace backend")?;
        let root_json = encode_json(&workspace.root, "workspace root")?;
        let working_directory_json =
            encode_json(&workspace.working_directory, "workspace working directory")?;
        let source_repository_json =
            encode_json(&workspace.source_repository, "workspace source repository")?;
        let jj_parent_revisions_json =
            encode_json(&workspace.jj_parent_revisions, "workspace parent revisions")?;
        let state_json = encode_json(&workspace.state, "workspace state")?;
        transaction
            .execute(
                "INSERT INTO workspaces (
                     id, order_index, project_id, backend_json, root_json,
                     working_directory_json, source_repository_json, source_id, source_label,
                     source_revision, jj_parent_revisions_json, git_branch, state_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(id) DO UPDATE SET
                     order_index = excluded.order_index,
                     project_id = excluded.project_id,
                     backend_json = excluded.backend_json,
                     root_json = excluded.root_json,
                     working_directory_json = excluded.working_directory_json,
                     source_repository_json = excluded.source_repository_json,
                     source_id = excluded.source_id,
                     source_label = excluded.source_label,
                     source_revision = excluded.source_revision,
                     jj_parent_revisions_json = excluded.jj_parent_revisions_json,
                     git_branch = excluded.git_branch,
                     state_json = excluded.state_json
                 WHERE workspaces.order_index IS NOT excluded.order_index
                    OR workspaces.project_id IS NOT excluded.project_id
                    OR workspaces.backend_json IS NOT excluded.backend_json
                    OR workspaces.root_json IS NOT excluded.root_json
                    OR workspaces.working_directory_json IS NOT excluded.working_directory_json
                    OR workspaces.source_repository_json IS NOT excluded.source_repository_json
                    OR workspaces.source_id IS NOT excluded.source_id
                    OR workspaces.source_label IS NOT excluded.source_label
                    OR workspaces.source_revision IS NOT excluded.source_revision
                    OR workspaces.jj_parent_revisions_json IS NOT excluded.jj_parent_revisions_json
                    OR workspaces.git_branch IS NOT excluded.git_branch
                    OR workspaces.state_json IS NOT excluded.state_json",
                params![
                    &workspace.id,
                    order_index(index)?,
                    workspace.project_id.to_string(),
                    backend_json,
                    root_json,
                    working_directory_json,
                    source_repository_json,
                    &workspace.source_id,
                    &workspace.source_label,
                    &workspace.source_revision,
                    jj_parent_revisions_json,
                    &workspace.git_branch,
                    state_json,
                ],
            )
            .map_err(|error| format!("could not update workspace {}: {error}", workspace.id))?;
    }

    let desired_collapsed = state
        .collapsed_projects
        .iter()
        .map(ToString::to_string)
        .collect::<HashSet<_>>();
    let existing_collapsed =
        query_string_set(&transaction, "SELECT project_id FROM collapsed_projects")?;
    for project_id in existing_collapsed.difference(&desired_collapsed) {
        transaction
            .execute(
                "DELETE FROM collapsed_projects WHERE project_id = ?1",
                params![project_id],
            )
            .map_err(|error| format!("could not remove expanded project {project_id}: {error}"))?;
    }
    for project_id in desired_collapsed.difference(&existing_collapsed) {
        transaction
            .execute(
                "INSERT INTO collapsed_projects (project_id) VALUES (?1)",
                params![project_id],
            )
            .map_err(|error| format!("could not collapse project {project_id}: {error}"))?;
    }

    transaction
        .commit()
        .map_err(|error| format!("could not commit metadata transaction: {error}"))
}

fn prepare_table_order(
    transaction: &Transaction<'_>,
    table: &str,
    desired: Vec<(String, i64)>,
) -> Result<(), String> {
    let mut statement = transaction
        .prepare(&format!("SELECT id, order_index FROM {table}"))
        .map_err(|error| format!("could not inspect {table} order: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| format!("could not read {table} order: {error}"))?;
    let mut existing = HashMap::new();
    for row in rows {
        let (id, order) =
            row.map_err(|error| format!("could not read {table} order row: {error}"))?;
        existing.insert(id, order);
    }
    drop(statement);

    let desired_ids = desired
        .iter()
        .map(|(id, _)| id.clone())
        .collect::<HashSet<_>>();
    for id in existing.keys().filter(|id| !desired_ids.contains(*id)) {
        transaction
            .execute(&format!("DELETE FROM {table} WHERE id = ?1"), params![id])
            .map_err(|error| format!("could not remove {table} row {id}: {error}"))?;
    }
    // `order_index` is unique. Move changing rows into a disjoint negative range first so swaps
    // cannot collide before their later upserts assign final non-negative positions.
    for (temporary_index, (id, order)) in desired.iter().enumerate() {
        if existing.get(id).is_some_and(|current| current != order) {
            let temporary_order = (-1_i64)
                .checked_sub(
                    i64::try_from(temporary_index)
                        .map_err(|_| "too many state records to reorder".to_string())?,
                )
                .ok_or_else(|| "too many state records to reorder".to_string())?;
            transaction
                .execute(
                    &format!("UPDATE {table} SET order_index = ?1 WHERE id = ?2"),
                    params![temporary_order, id],
                )
                .map_err(|error| format!("could not prepare {table} row {id} reorder: {error}"))?;
        }
    }
    Ok(())
}

fn query_string_set(transaction: &Transaction<'_>, query: &str) -> Result<HashSet<String>, String> {
    let mut statement = transaction
        .prepare(query)
        .map_err(|error| format!("could not prepare stored id query: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("could not read stored ids: {error}"))?;
    let mut values = HashSet::new();
    for row in rows {
        values.insert(row.map_err(|error| format!("could not read stored id: {error}"))?);
    }
    Ok(values)
}

fn save_turn_diff(connection: &Connection, harness_id: Id, turn: &TurnDiff) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO turn_diffs (harness_id, turn_id, diff_json) VALUES (?1, ?2, ?3)
             ON CONFLICT(harness_id, turn_id) DO UPDATE SET diff_json = excluded.diff_json
             WHERE turn_diffs.diff_json IS NOT excluded.diff_json",
            params![
                harness_id.to_string(),
                turn.id.to_string(),
                encode_turn_diff(turn)?
            ],
        )
        .map_err(|error| {
            format!(
                "could not store thread {harness_id} turn {}: {error}",
                turn.id
            )
        })?;
    Ok(())
}

fn retain_turn_diffs(
    connection: &mut Connection,
    harness_id: Id,
    turn_ids: &[u64],
) -> Result<(), String> {
    let transaction = connection
        .transaction()
        .map_err(|error| format!("could not begin thread {harness_id} turn pruning: {error}"))?;
    retain_turn_diffs_in_transaction(&transaction, harness_id, turn_ids)?;
    transaction
        .commit()
        .map_err(|error| format!("could not commit thread {harness_id} turn pruning: {error}"))
}

fn retain_turn_diffs_in_transaction(
    transaction: &Transaction<'_>,
    harness_id: Id,
    turn_ids: &[u64],
) -> Result<(), String> {
    let retained = turn_ids
        .iter()
        .map(ToString::to_string)
        .collect::<HashSet<_>>();
    let mut statement = transaction
        .prepare("SELECT turn_id FROM turn_diffs WHERE harness_id = ?1")
        .map_err(|error| format!("could not inspect thread {harness_id} turns: {error}"))?;
    let rows = statement
        .query_map(params![harness_id.to_string()], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|error| format!("could not read thread {harness_id} turns: {error}"))?;
    let mut removed = Vec::new();
    for row in rows {
        let turn_id =
            row.map_err(|error| format!("could not read thread {harness_id} turn id: {error}"))?;
        if !retained.contains(&turn_id) {
            removed.push(turn_id);
        }
    }
    drop(statement);
    for turn_id in removed {
        transaction
            .execute(
                "DELETE FROM turn_diffs WHERE harness_id = ?1 AND turn_id = ?2",
                params![harness_id.to_string(), turn_id],
            )
            .map_err(|error| format!("could not remove thread {harness_id} turn: {error}"))?;
    }
    Ok(())
}

fn read_stored_state(connection: &Connection) -> Result<StoredState, String> {
    let (
        next_id,
        next_sidebar_order,
        last_used_harness,
        sidebar_width,
        diff_sidebar_open,
        diff_sidebar_width,
        diff_view_mode_json,
    ) = connection
        .query_row(
            "SELECT next_id, next_sidebar_order, last_used_harness, sidebar_width,
                    diff_sidebar_open, diff_sidebar_width, diff_view_mode_json
             FROM app_state WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, f32>(3)?,
                    row.get::<_, bool>(4)?,
                    row.get::<_, f32>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                ))
            },
        )
        .optional()
        .map_err(|error| format!("could not read application state: {error}"))?
        .ok_or_else(|| "state database has no application state".to_string())?;

    let next_id = decode_id(&next_id, "next id")?;
    let next_sidebar_order = decode_id(&next_sidebar_order, "next sidebar order")?;
    let last_used_harness = last_used_harness
        .as_deref()
        .map(|id| decode_id(id, "last used harness"))
        .transpose()?;

    let mut projects = Vec::new();
    let mut statement = connection
        .prepare(
            "SELECT id, name, path_json, workspace_root_json, keep_active_threads_in_project
             FROM projects ORDER BY order_index",
        )
        .map_err(|error| format!("could not prepare project state: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, bool>(4)?,
            ))
        })
        .map_err(|error| format!("could not read projects: {error}"))?;
    for row in rows {
        let (id, name, path_json, workspace_root_json, keep_active_threads_in_project) =
            row.map_err(|error| format!("could not read project: {error}"))?;
        projects.push(StoredProject {
            id: decode_id(&id, "project id")?,
            name,
            path: decode_json(&path_json, "project path")?,
            workspace_root: workspace_root_json
                .as_deref()
                .map(|bytes| decode_json(bytes, "project workspace root"))
                .transpose()?,
            keep_active_threads_in_project,
        });
    }
    drop(statement);

    let mut harnesses = Vec::new();
    let mut statement = connection
        .prepare(
            "SELECT id, project_id, title, session_file_json, nix_enabled, workspace_id,
                    last_vcs_label, archived, sidebar_order, work_group_expansion_json, delegation_json
             FROM harnesses ORDER BY order_index",
        )
        .map_err(|error| format!("could not prepare harness state: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
                row.get::<_, bool>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, bool>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Vec<u8>>(9)?,
                row.get::<_, Vec<u8>>(10)?,
            ))
        })
        .map_err(|error| format!("could not read harnesses: {error}"))?;
    for row in rows {
        let (
            id,
            project_id,
            title,
            session_file_json,
            nix_enabled,
            workspace_id,
            last_vcs_label,
            archived,
            sidebar_order,
            work_group_expansion_json,
            delegation_json,
        ) = row.map_err(|error| format!("could not read harness: {error}"))?;
        harnesses.push(StoredHarness {
            delegation: decode_json(&delegation_json, "delegation")?,
            id: decode_id(&id, "harness id")?,
            project_id: decode_id(&project_id, "harness project id")?,
            title,
            session_file: session_file_json
                .as_deref()
                .map(|bytes| decode_json(bytes, "harness session path"))
                .transpose()?,
            nix_enabled,
            workspace_id,
            last_vcs_label,
            archived,
            sidebar_order: decode_id(&sidebar_order, "harness sidebar order")?,
            turn_diffs: Vec::new(),
            work_group_expansion: decode_json(&work_group_expansion_json, "work group expansion")?,
        });
    }
    drop(statement);

    let harness_indices = harnesses
        .iter()
        .enumerate()
        .map(|(index, harness)| (harness.id.to_string(), index))
        .collect::<HashMap<_, _>>();
    let mut statement = connection
        .prepare("SELECT harness_id, diff_json FROM turn_diffs")
        .map_err(|error| format!("could not prepare turn diff state: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .map_err(|error| format!("could not read turn diffs: {error}"))?;
    for row in rows {
        let (harness_id, diff_json) =
            row.map_err(|error| format!("could not read turn diff: {error}"))?;
        if let Some(index) = harness_indices.get(&harness_id) {
            harnesses[*index]
                .turn_diffs
                .push(decode_turn_diff(&diff_json)?);
        }
    }
    for harness in &mut harnesses {
        harness.turn_diffs.sort_by_key(|turn| turn.id);
    }
    drop(statement);

    let mut workspaces = Vec::new();
    let mut statement = connection
        .prepare(
            "SELECT id, project_id, backend_json, root_json, working_directory_json,
                    source_repository_json, source_id, source_label, source_revision,
                    jj_parent_revisions_json, git_branch, state_json
             FROM workspaces ORDER BY order_index",
        )
        .map_err(|error| format!("could not prepare workspace state: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, Vec<u8>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Vec<u8>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Vec<u8>>(11)?,
            ))
        })
        .map_err(|error| format!("could not read workspaces: {error}"))?;
    for row in rows {
        let (
            id,
            project_id,
            backend_json,
            root_json,
            working_directory_json,
            source_repository_json,
            source_id,
            source_label,
            source_revision,
            jj_parent_revisions_json,
            git_branch,
            state_json,
        ) = row.map_err(|error| format!("could not read workspace: {error}"))?;
        workspaces.push(StoredWorkspace {
            id,
            project_id: decode_id(&project_id, "workspace project id")?,
            backend: decode_json(&backend_json, "workspace backend")?,
            root: decode_json(&root_json, "workspace root")?,
            working_directory: decode_json(&working_directory_json, "workspace working directory")?,
            source_repository: decode_json(&source_repository_json, "workspace source repository")?,
            source_id,
            source_label,
            source_revision,
            jj_parent_revisions: decode_json(
                &jj_parent_revisions_json,
                "workspace parent revisions",
            )?,
            git_branch,
            state: decode_json(&state_json, "workspace state")?,
        });
    }
    drop(statement);

    let mut collapsed_projects = Vec::new();
    let mut statement = connection
        .prepare("SELECT project_id FROM collapsed_projects ORDER BY project_id")
        .map_err(|error| format!("could not prepare collapsed project state: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| format!("could not read collapsed projects: {error}"))?;
    for row in rows {
        let id = row.map_err(|error| format!("could not read collapsed project: {error}"))?;
        collapsed_projects.push(decode_id(&id, "collapsed project id")?);
    }

    Ok(StoredState {
        next_id,
        next_sidebar_order,
        projects,
        harnesses,
        workspaces,
        last_used_harness,
        collapsed_projects,
        sidebar_width,
        diff_sidebar_open,
        diff_sidebar_width,
        diff_view_mode: decode_json(&diff_view_mode_json, "diff view mode")?,
    })
}

impl StoredState {
    fn into_loaded(self) -> LoadedState {
        let projects = self
            .projects
            .into_iter()
            .map(|project| Project {
                id: project.id,
                name: project.name,
                path: project.path,
                workspace_root: project.workspace_root,
                keep_active_threads_in_project: project.keep_active_threads_in_project,
            })
            .collect();
        let harnesses = self
            .harnesses
            .into_iter()
            .map(|harness| {
                let mut restored = Harness::restored(
                    harness.id,
                    harness.project_id,
                    harness.title,
                    harness.session_file,
                    harness.nix_enabled,
                    harness.workspace_id,
                    harness.last_vcs_label,
                    harness.archived,
                    harness.sidebar_order,
                    harness.turn_diffs.into_iter().map(Arc::new).collect(),
                    harness.work_group_expansion,
                );
                restored.delegation = harness.delegation;
                restored.delegation.interrupt();
                restored
            })
            .collect();
        let workspaces = self
            .workspaces
            .into_iter()
            .map(|workspace| ManagedWorkspace {
                id: workspace.id,
                project_id: workspace.project_id,
                backend: workspace.backend,
                root: workspace.root,
                working_directory: workspace.working_directory,
                source_repository: workspace.source_repository,
                source_id: workspace.source_id,
                source_label: workspace.source_label,
                source_revision: workspace.source_revision,
                jj_parent_revisions: workspace.jj_parent_revisions,
                git_branch: workspace.git_branch,
                state: workspace.state,
            })
            .collect();
        LoadedState {
            projects,
            harnesses,
            workspaces,
            next_id: self.next_id.max(1),
            next_sidebar_order: self.next_sidebar_order.max(1),
            last_used_harness: self.last_used_harness,
            collapsed_projects: self.collapsed_projects.into_iter().collect(),
            sidebar_width: self.sidebar_width,
            diff_sidebar_open: self.diff_sidebar_open,
            diff_sidebar_width: self.diff_sidebar_width,
            diff_view_mode: self.diff_view_mode,
        }
    }
}

/// Compresses large source snapshots while retaining JSON as the schema's logical format.
fn encode_turn_diff(turn: &TurnDiff) -> Result<Vec<u8>, String> {
    let json =
        serde_json::to_vec(turn).map_err(|error| format!("could not encode turn diff: {error}"))?;
    zstd::stream::encode_all(json.as_slice(), 3)
        .map_err(|error| format!("could not compress turn diff: {error}"))
}

fn decode_turn_diff(bytes: &[u8]) -> Result<TurnDiff, String> {
    // Uncompressed JSON remains readable for databases created before diff compression.
    let json = if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        zstd::stream::decode_all(bytes)
            .map_err(|error| format!("could not decompress turn diff: {error}"))?
    } else {
        bytes.to_vec()
    };
    serde_json::from_slice(&json).map_err(|error| format!("could not decode turn diff: {error}"))
}

// Legacy schema decoder used only while migrating the old per-harness blob.
fn decode_turn_diffs(bytes: &[u8]) -> Result<Vec<TurnDiff>, String> {
    let json = if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        zstd::stream::decode_all(bytes)
            .map_err(|error| format!("could not decompress turn diffs: {error}"))?
    } else {
        bytes.to_vec()
    };
    serde_json::from_slice(&json).map_err(|error| format!("could not decode turn diffs: {error}"))
}

fn encode_json<T: Serialize + ?Sized>(value: &T, description: &str) -> Result<Vec<u8>, String> {
    serde_json::to_vec(value).map_err(|error| format!("could not encode {description}: {error}"))
}

fn decode_json<T: DeserializeOwned>(bytes: &[u8], description: &str) -> Result<T, String> {
    serde_json::from_slice(bytes)
        .map_err(|error| format!("could not decode {description}: {error}"))
}

fn decode_id(value: &str, description: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|error| format!("invalid {description} {value:?}: {error}"))
}

fn order_index(index: usize) -> Result<i64, String> {
    i64::try_from(index).map_err(|_| "too many state records to store".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delegation::{AgentJob, AgentRun, WorkStatus};

    #[test]
    fn delegation_survives_database_restart_without_replaying_work() {
        let directory =
            std::env::temp_dir().join(format!("dirigent-delegation-{}", fastrand::u64(..)));
        let path = directory.join("state.sqlite3");
        let (db, _) = StateDatabase::open_at(&path).unwrap();
        let mut parent = Harness::new(1, 10, "Manager".into(), 1);
        parent.delegation.jobs.push(AgentJob {
            id: "job".into(),
            tool_call_id: "tool-call".into(),
            title: "Implement".into(),
            status: WorkStatus::Running,
            result: String::new(),
        });
        let mut child = Harness::new(2, 10, "Child".into(), 2);
        child.delegation.parent = Some(1);
        for (id, status) in [
            ("finished", WorkStatus::Completed),
            ("active", WorkStatus::Running),
        ] {
            child.delegation.runs.push(AgentRun {
                id: id.into(),
                job_id: "job".into(),
                status,
                result: "saved output".into(),
                error: None,
                stop_reason: Some("stop".into()),
            });
        }
        db.save(
            &[],
            &[parent, child],
            &[],
            11,
            3,
            Some(1),
            &HashSet::new(),
            DEFAULT_SIDEBAR_WIDTH,
            false,
            DEFAULT_DIFF_SIDEBAR_WIDTH,
            DiffViewMode::Unified,
        )
        .unwrap();
        drop(db); // flush the ordered storage worker
        let (db, loaded) = StateDatabase::open_at(&path).unwrap();
        assert_eq!(
            loaded.harnesses[0].delegation.jobs[0].status,
            WorkStatus::Interrupted
        );
        let child = &loaded.harnesses[1];
        assert_eq!(child.delegation.parent, Some(1));
        assert_eq!(child.delegation.runs[0].status, WorkStatus::Completed);
        assert_eq!(child.delegation.runs[0].result, "saved output");
        assert_eq!(child.delegation.runs[1].status, WorkStatus::Interrupted);
        assert!(child.pending_initial_prompt.is_none());
        drop(db);

        // Existing schema-6 databases acquire empty metadata without losing their threads.
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE harnesses DROP COLUMN delegation_json; PRAGMA user_version = 6;",
            )
            .unwrap();
        drop(connection);
        let (db, loaded) = StateDatabase::open_at(&path).unwrap();
        assert_eq!(loaded.harnesses.len(), 2);
        assert!(loaded.harnesses[1].delegation.parent.is_none());
        drop(db);
        fs::remove_dir_all(directory).unwrap();
    }
}
