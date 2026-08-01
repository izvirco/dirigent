use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    model::{Harness, Id, ManagedWorkspace, Project, WorkspaceBackend, WorkspaceState},
    platform,
};

pub(crate) const DEFAULT_SIDEBAR_WIDTH: f32 = 288.0;
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS app_state (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        next_id TEXT NOT NULL,
        next_sidebar_order TEXT NOT NULL,
        last_used_harness TEXT,
        sidebar_width REAL NOT NULL
    );
    CREATE TABLE IF NOT EXISTS projects (
        id TEXT PRIMARY KEY,
        order_index INTEGER NOT NULL UNIQUE,
        name TEXT NOT NULL,
        path_json BLOB NOT NULL,
        workspace_root_json BLOB
    );
    CREATE TABLE IF NOT EXISTS harnesses (
        id TEXT PRIMARY KEY,
        order_index INTEGER NOT NULL UNIQUE,
        project_id TEXT NOT NULL,
        title TEXT NOT NULL,
        session_file_json BLOB,
        nix_enabled INTEGER NOT NULL,
        workspace_id TEXT,
        archived INTEGER NOT NULL,
        sidebar_order TEXT NOT NULL
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

#[derive(Serialize, Deserialize)]
struct StoredState {
    next_id: Id,
    next_sidebar_order: u64,
    projects: Vec<StoredProject>,
    harnesses: Vec<StoredHarness>,
    #[serde(default)]
    workspaces: Vec<StoredWorkspace>,
    last_used_harness: Option<Id>,
    collapsed_projects: Vec<Id>,
    #[serde(default = "default_sidebar_width")]
    sidebar_width: f32,
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
            sidebar_width: default_sidebar_width(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StoredProject {
    id: Id,
    name: String,
    path: PathBuf,
    #[serde(default)]
    workspace_root: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct StoredHarness {
    id: Id,
    project_id: Id,
    title: String,
    session_file: Option<PathBuf>,
    nix_enabled: bool,
    #[serde(default)]
    workspace_id: Option<String>,
    archived: bool,
    sidebar_order: u64,
}

#[derive(Serialize, Deserialize)]
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
    #[serde(default)]
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
}

pub(crate) struct StateDatabase {
    connection: Connection,
}

impl StateDatabase {
    pub(crate) fn open() -> Result<(Self, LoadedState), String> {
        let database_path = platform::state_database_path()?;
        let legacy_path = platform::legacy_state_path()?;
        Self::open_at(&database_path, &legacy_path)
    }

    fn open_at(database_path: &Path, legacy_path: &Path) -> Result<(Self, LoadedState), String> {
        let connection = open_database(database_path, legacy_path)?;
        let loaded = read_stored_state(&connection).map(StoredState::into_loaded)?;
        Ok((Self { connection }, loaded))
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
                    archived: harness.archived,
                    sidebar_order: harness.sidebar_order,
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
        };
        write_stored_state(&mut self.connection, &state)
    }
}

fn open_database(database_path: &Path, legacy_path: &Path) -> Result<Connection, String> {
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

    if legacy_path.exists() {
        if !has_state {
            let bytes = fs::read(legacy_path)
                .map_err(|error| format!("could not read {}: {error}", legacy_path.display()))?;
            let state: StoredState = serde_json::from_slice(&bytes)
                .map_err(|error| format!("could not parse {}: {error}", legacy_path.display()))?;
            write_stored_state(&mut connection, &state)?;
        }
        back_up_legacy_state(legacy_path)?;
    } else if !has_state {
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
             {SCHEMA}
             PRAGMA user_version = {SCHEMA_VERSION};"
        ))
        .map_err(|error| format!("could not initialize {}: {error}", database_path.display()))
}

fn back_up_legacy_state(legacy_path: &Path) -> Result<(), String> {
    let backup_path = legacy_path.with_file_name("state.json.backup");
    if backup_path.exists() {
        fs::remove_file(&backup_path).map_err(|error| {
            format!(
                "could not replace legacy state backup {}: {error}",
                backup_path.display()
            )
        })?;
    }
    fs::rename(legacy_path, &backup_path).map_err(|error| {
        format!(
            "could not rename {} to {}: {error}",
            legacy_path.display(),
            backup_path.display()
        )
    })
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
                singleton, next_id, next_sidebar_order, last_used_harness, sidebar_width
             ) VALUES (1, ?1, ?2, ?3, ?4)",
            params![
                state.next_id.to_string(),
                state.next_sidebar_order.to_string(),
                state.last_used_harness.map(|id| id.to_string()),
                state.sidebar_width,
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
                    id, order_index, name, path_json, workspace_root_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    project.id.to_string(),
                    order_index(index)?,
                    &project.name,
                    path_json,
                    workspace_root_json,
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
                    workspace_id, archived, sidebar_order
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    harness.id.to_string(),
                    order_index(index)?,
                    harness.project_id.to_string(),
                    &harness.title,
                    session_file_json,
                    harness.nix_enabled,
                    &harness.workspace_id,
                    harness.archived,
                    harness.sidebar_order.to_string(),
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
    let (next_id, next_sidebar_order, last_used_harness, sidebar_width) = connection
        .query_row(
            "SELECT next_id, next_sidebar_order, last_used_harness, sidebar_width
             FROM app_state WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, f32>(3)?,
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
            "SELECT id, name, path_json, workspace_root_json
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
            ))
        })
        .map_err(|error| format!("could not read projects: {error}"))?;
    for row in rows {
        let (id, name, path_json, workspace_root_json) =
            row.map_err(|error| format!("could not read project: {error}"))?;
        projects.push(StoredProject {
            id: decode_id(&id, "project id")?,
            name,
            path: decode_json(&path_json, "project path")?,
            workspace_root: workspace_root_json
                .as_deref()
                .map(|bytes| decode_json(bytes, "project workspace root"))
                .transpose()?,
        });
    }
    drop(statement);

    let mut harnesses = Vec::new();
    let mut statement = connection
        .prepare(
            "SELECT id, project_id, title, session_file_json, nix_enabled, workspace_id,
                    archived, sidebar_order
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
                row.get::<_, bool>(6)?,
                row.get::<_, String>(7)?,
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
            archived,
            sidebar_order,
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
            archived,
            sidebar_order: decode_id(&sidebar_order, "harness sidebar order")?,
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
                    harness.archived,
                    harness.sidebar_order,
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
        }
    }
}

fn default_sidebar_width() -> f32 {
    DEFAULT_SIDEBAR_WIDTH
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
    use super::{DEFAULT_SIDEBAR_WIDTH, StateDatabase, StoredState};
    use std::{fs, path::PathBuf};

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
        assert!(
            crate::platform::legacy_state_path()
                .unwrap()
                .ends_with("dirigent/v0/state.json")
        );
    }

    #[test]
    fn migrates_json_and_keeps_a_backup() {
        let directory = temporary_directory("migration");
        let database = directory.join("state.sqlite3");
        let legacy = directory.join("state.json");
        fs::write(
            &legacy,
            r#"{
                "next_id": 3,
                "next_sidebar_order": 8,
                "projects": [{
                    "id": 1,
                    "name": "Dirigent",
                    "path": "/tmp/dirigent",
                    "workspace_root": "/tmp/workspaces"
                }],
                "harnesses": [{
                    "id": 2,
                    "project_id": 1,
                    "title": "Migration",
                    "session_file": "/tmp/session.jsonl",
                    "nix_enabled": true,
                    "workspace_id": "workspace-1",
                    "archived": false,
                    "sidebar_order": 7
                }],
                "workspaces": [{
                    "id": "workspace-1",
                    "project_id": 1,
                    "backend": "Git",
                    "root": "/tmp/workspaces/workspace-1",
                    "working_directory": "/tmp/workspaces/workspace-1",
                    "source_repository": "/tmp/dirigent",
                    "source_id": "main",
                    "source_label": "main",
                    "source_revision": "abc123",
                    "jj_parent_revisions": [],
                    "git_branch": "dirigent/workspace-1",
                    "state": "Ready"
                }],
                "last_used_harness": 2,
                "collapsed_projects": [1],
                "sidebar_width": 320.0
            }"#,
        )
        .unwrap();

        let (_database, loaded) = StateDatabase::open_at(&database, &legacy).unwrap();

        assert!(database.exists());
        assert!(!legacy.exists());
        assert!(directory.join("state.json.backup").exists());
        assert_eq!(loaded.next_id, 3);
        assert_eq!(loaded.next_sidebar_order, 8);
        assert_eq!(loaded.projects.len(), 1);
        assert_eq!(
            loaded.projects[0].workspace_root.as_deref(),
            Some(std::path::Path::new("/tmp/workspaces"))
        );
        assert_eq!(loaded.harnesses.len(), 1);
        assert_eq!(
            loaded.harnesses[0].workspace_id.as_deref(),
            Some("workspace-1")
        );
        assert_eq!(loaded.workspaces.len(), 1);
        assert_eq!(
            loaded.workspaces[0].backend,
            crate::model::WorkspaceBackend::Git
        );
        assert_eq!(loaded.last_used_harness, Some(2));
        assert!(loaded.collapsed_projects.contains(&1));
        assert_eq!(loaded.sidebar_width, 320.0);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn defaults_sidebar_width_for_existing_state() {
        let state: StoredState = serde_json::from_str(
            r#"{
                "next_id": 1,
                "next_sidebar_order": 1,
                "projects": [],
                "harnesses": [],
                "last_used_harness": null,
                "collapsed_projects": []
            }"#,
        )
        .unwrap();

        assert_eq!(state.sidebar_width, DEFAULT_SIDEBAR_WIDTH);
    }
}
