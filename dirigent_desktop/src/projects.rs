//! Per-host state and the cross-host project grouping the Projects page lists (and the
//! sidebar, for the pinned ones).
//!
//! A project lives on one host. Projects on different hosts are shown as one group when
//! they share a normalized git origin (automatic) or the user linked them (manual, stored
//! in the desktop cache as a group key). Threads whose project was removed are orphans.

use crate::net::HostStatus;
use dirigent_protocol::{ModelInfo, Project, ThreadSummary};
use std::collections::HashMap;

pub struct HostState {
    pub name: String,
    pub url: String,
    pub status: HostStatus,
    pub projects: Vec<Project>,
    pub threads: Vec<ThreadSummary>,
    pub models: Vec<ModelInfo>,
}

/// Pseudo group key for threads whose project was removed.
pub const ORPHANS: &str = "orphans";

/// (host, project id) → manual group key.
pub type Links = HashMap<(String, String), String>;

pub struct Member {
    pub host: String,
    pub project: Project,
}

pub struct Group {
    pub key: String,
    /// Basename of the first member's path.
    pub name: String,
    pub members: Vec<Member>,
    /// (host, thread), most recent first.
    pub threads: Vec<(String, ThreadSummary)>,
    /// Any member is in this group through a manual link.
    pub manual: bool,
}

impl Group {
    pub fn has(&self, host: &str, project_id: &str) -> bool {
        self.members.iter().any(|m| m.host == host && m.project.id == project_id)
    }

    /// The member a new thread goes to: the one on `host` if the group has it, else the
    /// host of the group's latest thread, else the first member.
    pub fn member_for(&self, host: Option<&str>) -> &Member {
        let on = |h: &str| self.members.iter().find(|m| m.host == h);
        host.and_then(on).or_else(|| self.threads.first().and_then(|(h, _)| on(h))).unwrap_or(&self.members[0])
    }
}

/// Manual link → git origin → the project itself.
pub fn group_key(host: &str, project: &Project, links: &Links) -> String {
    links
        .get(&(host.to_owned(), project.id.clone()))
        .cloned()
        .or_else(|| project.origin.clone())
        .unwrap_or_else(|| format!("{host}/{}", project.id))
}

pub fn basename(path: &str) -> &str {
    path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path)
}

/// Groups by most recent activity (then name); plus orphan threads (most recent first).
pub fn groups(hosts: &[HostState], links: &Links) -> (Vec<Group>, Vec<(String, ThreadSummary)>) {
    let mut by_key: HashMap<String, Group> = HashMap::new();
    let mut seen: Vec<String> = vec![];
    for host in hosts {
        for project in &host.projects {
            let key = group_key(&host.name, project, links);
            let manual = links.contains_key(&(host.name.clone(), project.id.clone()));
            let group = by_key.entry(key.clone()).or_insert_with(|| {
                seen.push(key.clone());
                Group { key: key.clone(), name: basename(&project.path).to_owned(), members: vec![], threads: vec![], manual: false }
            });
            group.manual |= manual;
            group.members.push(Member { host: host.name.clone(), project: project.clone() });
        }
    }

    let mut orphans = vec![];
    for host in hosts {
        // Sub-agents are listed under their parent (the orchestration panel), not here.
        for thread in host.threads.iter().filter(|t| t.top_level()) {
            let key = thread
                .project_id
                .as_ref()
                .and_then(|pid| host.projects.iter().find(|p| p.id == *pid))
                .map(|p| group_key(&host.name, p, links));
            match key.and_then(|k| by_key.get_mut(&k)) {
                Some(group) => group.threads.push((host.name.clone(), thread.clone())),
                None => orphans.push((host.name.clone(), thread.clone())),
            }
        }
    }

    let mut groups: Vec<Group> = seen.into_iter().filter_map(|k| by_key.remove(&k)).collect();
    for g in &mut groups {
        g.members.sort_by(|a, b| a.host.cmp(&b.host));
        g.threads.sort_by_key(|(_, t)| std::cmp::Reverse(t.updated_at));
    }
    let latest = |g: &Group| g.threads.first().map(|(_, t)| t.updated_at).unwrap_or(0);
    groups.sort_by(|a, b| latest(b).cmp(&latest(a)).then_with(|| a.name.cmp(&b.name)));
    orphans.sort_by_key(|(_, t)| std::cmp::Reverse(t.updated_at));
    (groups, orphans)
}

/// The pinned groups, in pin order (`pins`: group keys); keys of groups that are gone
/// are skipped.
pub fn pinned<'a>(groups: &'a [Group], pins: &[String]) -> Vec<&'a Group> {
    pins.iter().filter_map(|k| groups.iter().find(|g| g.key == *k)).collect()
}

/// `pins` after a pinned group's key changed (it was linked into another group, or
/// unlinked into its members' own keys): `new` takes `old`'s place, minus keys already
/// pinned. Unpinned `old`: unchanged.
pub fn repin(pins: &[String], old: &str, new: &[String]) -> Vec<String> {
    let Some(at) = pins.iter().position(|k| k == old) else { return pins.to_vec() };
    let mut out: Vec<String> = pins.iter().filter(|k| *k != old).cloned().collect();
    let mut fresh: Vec<String> = vec![];
    for k in new {
        if !out.contains(k) && !fresh.contains(k) {
            fresh.push(k.clone());
        }
    }
    out.splice(at..at, fresh);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, path: &str, origin: Option<&str>) -> Project {
        Project { id: id.into(), path: path.into(), origin: origin.map(str::to_owned), created_at: 0 }
    }
    fn thread(id: &str, project_id: Option<&str>, updated_at: i64) -> ThreadSummary {
        ThreadSummary {
            id: id.into(),
            project_id: project_id.map(str::to_owned),
            cwd: "/x".into(),
            name: None,
            model: None,
            thinking_level: None,
            created_at: 0,
            updated_at,
            entry_count: 0,
            running_since: None,
            last_run_ms: None,
            asks: vec![],
            archived: false,
            workspace: None,
            workspace_missing: false,
            parent_id: None,
            snoozed: false,
            pinned: false,
            seen_at: None,
            pinned_entries: vec![],
            prompted_at: None,
            jobs: 0,
            spend: Default::default(),
        }
    }
    fn host(name: &str, projects: Vec<Project>, threads: Vec<ThreadSummary>) -> HostState {
        HostState { name: name.into(), url: String::new(), status: HostStatus::Connected, projects, threads, models: vec![] }
    }

    #[test]
    fn links_by_origin_and_manual_key_and_orphans() {
        let hosts = vec![
            host(
                "local",
                vec![project("a", "/home/me/app", Some("gh/me/app")), project("n", "/home/me/notes", None)],
                vec![thread("t1", Some("a"), 5), thread("t2", Some("gone"), 9)],
            ),
            host(
                "box",
                vec![project("b", "/srv/app", Some("gh/me/app")), project("m", "/srv/notes", None)],
                vec![thread("t3", Some("b"), 7)],
            ),
        ];
        let mut links = Links::new();
        links.insert(("box".into(), "m".into()), "local/n".into());

        let (groups, orphans) = groups(&hosts, &links);
        assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["app", "notes"]);
        let app = &groups[0];
        assert_eq!(app.members.iter().map(|m| m.host.as_str()).collect::<Vec<_>>(), ["box", "local"]);
        assert_eq!(app.threads.iter().map(|(_, t)| t.id.as_str()).collect::<Vec<_>>(), ["t3", "t1"]);
        assert!(!app.manual);
        let notes = &groups[1];
        assert_eq!(notes.members.len(), 2);
        assert!(notes.manual);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].1.id, "t2");

        assert!(app.has("box", "b") && !app.has("box", "a"));
        assert_eq!(app.member_for(Some("local")).host, "local");
        assert_eq!(app.member_for(None).host, "box"); // t3 is the latest thread
        assert_eq!(notes.member_for(None).host, "box"); // no threads: first member
    }

    #[test]
    fn pinned_in_pin_order() {
        let hosts = vec![host(
            "local",
            vec![project("a", "/a", None), project("b", "/b", None), project("c", "/c", None)],
            vec![thread("t1", Some("a"), 1), thread("t2", Some("b"), 2), thread("t3", Some("c"), 3)],
        )];
        let (groups, _) = groups(&hosts, &Links::new());
        assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["c", "b", "a"]); // by activity
        // Pin order, a stale key skipped.
        let pins = ["local/a".into(), "gone".into(), "local/c".into()];
        assert_eq!(pinned(&groups, &pins).iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["a", "c"]);
    }

    #[test]
    fn repin_keeps_the_place() {
        let pins: Vec<String> = ["x", "old", "y"].map(String::from).to_vec();
        // Linked into a group: its key takes the place.
        assert_eq!(repin(&pins, "old", &["new".into()]), ["x", "new", "y"]);
        // Linked into a pinned group, or unlinked into several keys (deduped).
        assert_eq!(repin(&pins, "old", &["y".into()]), ["x", "y"]);
        assert_eq!(repin(&pins, "old", &["a".into(), "b".into(), "a".into()]), ["x", "a", "b", "y"]);
        // Not pinned: nothing changes.
        assert_eq!(repin(&pins, "z", &["new".into()]), pins);
    }
}
