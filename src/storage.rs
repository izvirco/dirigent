use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
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
const SCHEMA_VERSION: i64 = 5;

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
        work_group_expansion_json BLOB NOT NULL DEFAULT X'7B7D'
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

pub(crate) struct StateDatabase {
    connection: Connection,
}

impl StateDatabase {
    pub(crate) fn open() -> Result<(Self, LoadedState), String> {
        let database_path = platform::state_database_path()?;
        Self::open_at(&database_path)
    }

    fn open_at(database_path: &Path) -> Result<(Self, LoadedState), String> {
        let connection = open_database(database_path)?;
        let loaded = read_stored_state(&connection).map(StoredState::into_loaded)?;
        Ok((Self { connection }, loaded))
    }

    pub(crate) fn save_diff_sidebar(
        &mut self,
        open: bool,
        width: f32,
        view_mode: DiffViewMode,
    ) -> Result<(), String> {
        self.connection
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

    pub(crate) fn save_sidebar_layout(
        &mut self,
        sidebar_width: f32,
        diff_sidebar_width: f32,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE app_state SET sidebar_width = ?1, diff_sidebar_width = ?2
                 WHERE singleton = 1",
                params![sidebar_width, diff_sidebar_width],
            )
            .map_err(|error| format!("could not store sidebar layout: {error}"))?;
        Ok(())
    }

    pub(crate) fn save_last_used_harness(&mut self, harness_id: Option<Id>) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE app_state SET last_used_harness = ?1 WHERE singleton = 1",
                params![harness_id.map(|id| id.to_string())],
            )
            .map_err(|error| format!("could not store last used thread: {error}"))?;
        Ok(())
    }

    pub(crate) fn save_harness_session_file(
        &mut self,
        harness_id: Id,
        session_file: Option<&Path>,
    ) -> Result<(), String> {
        let session_file_json = session_file
            .map(|path| encode_json(path, "harness session path"))
            .transpose()?;
        self.connection
            .execute(
                "UPDATE harnesses SET session_file_json = ?1 WHERE id = ?2",
                params![session_file_json, harness_id.to_string()],
            )
            .map_err(|error| format!("could not store thread session path: {error}"))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn save(
        &mut self,
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
        let state = StoredState {
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
                .map(|harness| StoredHarness {
                    id: harness.id,
                    project_id: harness.project_id,
                    title: harness.title.clone(),
                    session_file: harness.session_file.clone(),
                    nix_enabled: harness.nix_enabled,
                    workspace_id: harness.workspace_id.clone(),
                    last_vcs_label: harness.last_vcs_label.clone(),
                    archived: harness.archived,
                    sidebar_order: harness.sidebar_order,
                    turn_diffs: harness.turn_diffs.clone(),
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
        };
        write_stored_state(&mut self.connection, &state)
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
    connection
        .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
        .map_err(|error| format!("could not update {}: {error}", database_path.display()))
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
                    workspace_id, last_vcs_label, archived, sidebar_order, turn_diffs_json,
                    work_group_expansion_json
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
                    encode_turn_diffs(&harness.turn_diffs)?,
                    encode_json(&harness.work_group_expansion, "work group expansion")?,
                ],
            )
            .map_err(|error| format!("could not store harness {}: {error}", harness.id))?;
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
                    last_vcs_label, archived, sidebar_order, turn_diffs_json,
                    work_group_expansion_json
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
            turn_diffs_json,
            work_group_expansion_json,
        ) = row.map_err(|error| format!("could not read harness: {error}"))?;
        harnesses.push(StoredHarness {
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
            turn_diffs: decode_turn_diffs(&turn_diffs_json)?,
            work_group_expansion: decode_json(&work_group_expansion_json, "work group expansion")?,
        });
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
                Harness::restored(
                    harness.id,
                    harness.project_id,
                    harness.title,
                    harness.session_file,
                    harness.nix_enabled,
                    harness.workspace_id,
                    harness.last_vcs_label,
                    harness.archived,
                    harness.sidebar_order,
                    harness.turn_diffs,
                    harness.work_group_expansion,
                )
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

fn encode_turn_diffs(turns: &[TurnDiff]) -> Result<Vec<u8>, String> {
    let json = serde_json::to_vec(turns)
        .map_err(|error| format!("could not encode turn diffs: {error}"))?;
    zstd::stream::encode_all(json.as_slice(), 3)
        .map_err(|error| format!("could not compress turn diffs: {error}"))
}

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
    use super::{
        DEFAULT_DIFF_SIDEBAR_WIDTH, DEFAULT_SIDEBAR_WIDTH, StateDatabase, StoredHarness,
        StoredProject, StoredState, decode_turn_diffs, encode_turn_diffs, write_stored_state,
    };
    use crate::diff::DiffViewMode;
    use std::{collections::HashMap, fs, path::PathBuf};

    fn temporary_directory(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dirigent-storage-{name}-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn stores_v0_state_in_sqlite() {
        assert!(
            crate::platform::state_database_path()
                .unwrap()
                .ends_with("dirigent/v0/state.sqlite3")
        );
    }

    #[test]
    fn initializes_new_database() {
        let directory = temporary_directory("initialization");
        let database = directory.join("state.sqlite3");

        let (state_database, loaded) = StateDatabase::open_at(&database).unwrap();

        assert!(database.exists());
        assert_eq!(loaded.next_id, 1);
        assert_eq!(loaded.next_sidebar_order, 1);
        assert!(loaded.projects.is_empty());
        assert!(loaded.harnesses.is_empty());
        assert!(loaded.workspaces.is_empty());
        assert_eq!(loaded.last_used_harness, None);
        assert!(loaded.collapsed_projects.is_empty());
        assert_eq!(loaded.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
        assert!(!loaded.diff_sidebar_open);
        assert_eq!(loaded.diff_sidebar_width, DEFAULT_DIFF_SIDEBAR_WIDTH);
        assert_eq!(loaded.diff_view_mode, DiffViewMode::Unified);

        drop(state_database);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_harness_metadata() {
        let directory = temporary_directory("harness-metadata");
        let database = directory.join("state.sqlite3");
        let (mut state_database, _) = StateDatabase::open_at(&database).unwrap();
        let mut state = StoredState::empty();
        state.projects.push(StoredProject {
            id: 1,
            name: "project".into(),
            path: directory.clone(),
            workspace_root: None,
            keep_active_threads_in_project: true,
        });
        let mut work_group_expansion = HashMap::new();
        work_group_expansion.insert("entry:user-1".into(), true);
        state.harnesses.push(StoredHarness {
            id: 2,
            project_id: 1,
            title: "thread".into(),
            session_file: None,
            nix_enabled: false,
            workspace_id: None,
            last_vcs_label: Some("main".into()),
            archived: false,
            sidebar_order: 1,
            turn_diffs: Vec::new(),
            work_group_expansion,
        });
        write_stored_state(&mut state_database.connection, &state).unwrap();
        let session_file = directory.join("session.jsonl");
        state_database.save_sidebar_layout(336.0, 720.0).unwrap();
        state_database.save_last_used_harness(Some(2)).unwrap();
        state_database
            .save_harness_session_file(2, Some(&session_file))
            .unwrap();
        drop(state_database);

        let (_, loaded) = StateDatabase::open_at(&database).unwrap();
        assert!(loaded.projects[0].keep_active_threads_in_project);
        assert_eq!(loaded.sidebar_width, 336.0);
        assert_eq!(loaded.diff_sidebar_width, 720.0);
        assert_eq!(loaded.last_used_harness, Some(2));
        assert_eq!(
            loaded.harnesses[0].session_file.as_ref(),
            Some(&session_file)
        );
        assert_eq!(loaded.harnesses[0].last_vcs_label.as_deref(), Some("main"));
        assert_eq!(
            loaded.harnesses[0].work_group_expansion.get("entry:user-1"),
            Some(&true)
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn migrates_state_created_before_diff_sidebars() {
        let directory = temporary_directory("diff-migration");
        let database = directory.join("state.sqlite3");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE app_state (
                     singleton INTEGER PRIMARY KEY,
                     next_id TEXT NOT NULL,
                     next_sidebar_order TEXT NOT NULL,
                     last_used_harness TEXT,
                     sidebar_width REAL NOT NULL
                 );
                 CREATE TABLE projects (
                     id TEXT PRIMARY KEY, order_index INTEGER, name TEXT,
                     path_json BLOB, workspace_root_json BLOB
                 );
                 CREATE TABLE harnesses (
                     id TEXT PRIMARY KEY, order_index INTEGER, project_id TEXT, title TEXT,
                     session_file_json BLOB, nix_enabled INTEGER, workspace_id TEXT,
                     archived INTEGER, sidebar_order TEXT
                 );
                 CREATE TABLE workspaces (
                     id TEXT PRIMARY KEY, order_index INTEGER, project_id TEXT,
                     backend_json BLOB, root_json BLOB, working_directory_json BLOB,
                     source_repository_json BLOB, source_id TEXT, source_label TEXT,
                     source_revision TEXT, jj_parent_revisions_json BLOB,
                     git_branch TEXT, state_json BLOB
                 );
                 CREATE TABLE collapsed_projects (project_id TEXT PRIMARY KEY);
                 INSERT INTO app_state VALUES (1, '2', '1', NULL, 288);
                 INSERT INTO projects VALUES (
                     '1', 0, 'project', CAST('\"/tmp/project\"' AS BLOB), NULL
                 );
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let (_, loaded) = StateDatabase::open_at(&database).unwrap();
        assert!(!loaded.diff_sidebar_open);
        assert_eq!(loaded.diff_sidebar_width, DEFAULT_DIFF_SIDEBAR_WIDTH);
        assert_eq!(loaded.diff_view_mode, DiffViewMode::Unified);
        assert!(!loaded.projects[0].keep_active_threads_in_project);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn compresses_persisted_turn_diffs() {
        let encoded = encode_turn_diffs(&[]).unwrap();
        assert!(encoded.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]));
        assert!(decode_turn_diffs(&encoded).unwrap().is_empty());
        assert!(decode_turn_diffs(b"[]").unwrap().is_empty());
    }
}
