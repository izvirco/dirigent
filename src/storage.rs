use std::{collections::HashSet, fs, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    model::{Harness, Id, Project},
    platform,
};

pub(crate) const DEFAULT_SIDEBAR_WIDTH: f32 = 288.0;

#[derive(Serialize, Deserialize)]
struct StoredState {
    next_id: Id,
    next_sidebar_order: u64,
    projects: Vec<StoredProject>,
    harnesses: Vec<StoredHarness>,
    last_used_harness: Option<Id>,
    collapsed_projects: Vec<Id>,
    #[serde(default = "default_sidebar_width")]
    sidebar_width: f32,
}

fn default_sidebar_width() -> f32 {
    DEFAULT_SIDEBAR_WIDTH
}

#[derive(Serialize, Deserialize)]
struct StoredProject {
    id: Id,
    name: String,
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct StoredHarness {
    id: Id,
    project_id: Id,
    title: String,
    session_file: Option<PathBuf>,
    nix_enabled: bool,
    archived: bool,
    sidebar_order: u64,
}

pub(crate) struct LoadedState {
    pub(crate) projects: Vec<Project>,
    pub(crate) harnesses: Vec<Harness>,
    pub(crate) next_id: Id,
    pub(crate) next_sidebar_order: u64,
    pub(crate) last_used_harness: Option<Id>,
    pub(crate) collapsed_projects: HashSet<Id>,
    pub(crate) sidebar_width: f32,
}

pub(crate) fn load() -> Result<LoadedState, String> {
    let path = state_path()?;
    if !path.exists() {
        return Ok(LoadedState {
            projects: Vec::new(),
            harnesses: Vec::new(),
            next_id: 1,
            next_sidebar_order: 1,
            last_used_harness: None,
            collapsed_projects: HashSet::new(),
            sidebar_width: default_sidebar_width(),
        });
    }
    let bytes =
        fs::read(&path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let state: StoredState = serde_json::from_slice(&bytes)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let projects = state
        .projects
        .into_iter()
        .map(|project| Project {
            id: project.id,
            name: project.name,
            path: project.path,
        })
        .collect();
    let harnesses = state
        .harnesses
        .into_iter()
        .map(|harness| {
            Harness::restored(
                harness.id,
                harness.project_id,
                harness.title,
                harness.session_file,
                harness.nix_enabled,
                harness.archived,
                harness.sidebar_order,
            )
        })
        .collect();
    Ok(LoadedState {
        projects,
        harnesses,
        next_id: state.next_id.max(1),
        next_sidebar_order: state.next_sidebar_order.max(1),
        last_used_harness: state.last_used_harness,
        collapsed_projects: state.collapsed_projects.into_iter().collect(),
        sidebar_width: state.sidebar_width,
    })
}

pub(crate) fn save(
    projects: &[Project],
    harnesses: &[Harness],
    next_id: Id,
    next_sidebar_order: u64,
    last_used_harness: Option<Id>,
    collapsed_projects: &HashSet<Id>,
    sidebar_width: f32,
) -> Result<(), String> {
    let path = state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
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
                archived: harness.archived,
                sidebar_order: harness.sidebar_order,
            })
            .collect(),
        last_used_harness,
        collapsed_projects,
        sidebar_width,
    };
    let bytes = serde_json::to_vec_pretty(&state)
        .map_err(|error| format!("could not encode workspace state: {error}"))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .map_err(|error| format!("could not replace {}: {error}", path.display()))
}

fn state_path() -> Result<PathBuf, String> {
    platform::state_path()
}

#[cfg(test)]
mod tests {
    use super::{StoredState, state_path};

    #[test]
    fn stores_v0_state_separately() {
        assert!(state_path().unwrap().ends_with("dirigent/v0/state.json"));
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

        assert_eq!(state.sidebar_width, super::DEFAULT_SIDEBAR_WIDTH);
    }
}
