use crate::net::HostStatus;
use crate::outbox::Outbox;
use dirigent_protocol::{ModelInfo, Project, ProxyUpdate, ThreadSummary};
use std::collections::HashMap;

pub struct HostState {
    pub name: String,
    pub config: dirigent_service::hosts::Host,
    pub status: HostStatus,
    pub error: Option<String>,
    pub projects: Vec<Project>,
    pub threads: Vec<ThreadSummary>,
    pub models: Vec<ModelInfo>,
    pub service_id: Option<String>,
    pub outbox: Outbox,
    pub proxy_update: Option<ProxyUpdate>,
}

pub const ORPHANS: &str = "orphans";

// MUST KILL: type: Links' (host, project id) key → a ProjectRef struct
pub type Links = HashMap<(String, String), String>;

pub type Names = HashMap<String, String>;

pub struct Member {
    pub host: String,
    pub project: Project,
    pub linked: bool,
}

pub struct Group {
    pub key: String,
    pub name: String,
    pub dir_name: String,
    pub members: Vec<Member>,
    // MUST KILL: rename: threads → threads_newest_first
    pub threads: Vec<(String, ThreadSummary)>,
}

impl Group {
    pub fn has(&self, host: &str, project_id: &str) -> bool {
        self.members.iter().any(|m| m.host == host && m.project.id == project_id)
    }

    pub fn threads_of(&self, member: &Member) -> usize {
        self.threads.iter().filter(|(h, t)| *h == member.host && t.project_id.as_deref() == Some(&member.project.id)).count()
    }

    pub fn member_for(&self, host: Option<&str>) -> &Member {
        let on = |h: &str| self.members.iter().find(|m| m.host == h);
        host.and_then(on).or_else(|| self.threads.first().and_then(|(h, _)| on(h))).unwrap_or(&self.members[0])
    }
}

pub fn group_key(host: &str, project: &Project, links: &Links) -> String {
    links
        .get(&(host.to_owned(), project.id.clone()))
        .cloned()
        .or_else(|| project.origin.clone())
        .unwrap_or_else(|| format!("{host}/{}", project.id))
}

/// A host's path's last part. Split at `\` too: the path may be a Windows host's.
pub fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(path)
}

pub fn same_local_path(a: &str, b: &str) -> bool {
    let norm = |p: &str| {
        let p = p.trim_end_matches(['/', '\\']);
        if cfg!(windows) { p.replace('/', "\\").to_lowercase() } else { p.to_owned() }
    };
    norm(a) == norm(b)
}

// MUST KILL: extract: groups' naming rule (the first member not linked in names the group) → a fn of its own
// MUST KILL: extract: groups' two stable sorts → one sort_by_key on (order position, Reverse(latest), name)
pub fn groups(hosts: &[HostState], links: &Links, names: &Names, order: &[String]) -> (Vec<Group>, Vec<(String, ThreadSummary)>) {
    let mut by_key: HashMap<String, Group> = HashMap::new();
    let mut seen: Vec<String> = vec![];
    for host in hosts {
        for project in &host.projects {
            let key = group_key(&host.name, project, links);
            let linked = links.contains_key(&(host.name.clone(), project.id.clone()));
            let group = by_key.entry(key.clone()).or_insert_with(|| {
                seen.push(key.clone());
                Group { key: key.clone(), name: String::new(), dir_name: String::new(), members: vec![], threads: vec![] }
            });
            if group.members.iter().all(|m| m.linked && !linked) {
                group.dir_name = basename(&project.path).to_owned();
                group.name = names.get(&key).cloned().unwrap_or_else(|| group.dir_name.clone());
            }
            group.members.push(Member { host: host.name.clone(), project: project.clone(), linked });
        }
    }

    let mut orphans = vec![];
    for host in hosts {
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
    groups.sort_by_key(|g| order.iter().position(|k| *k == g.key).unwrap_or(usize::MAX));
    orphans.sort_by_key(|(_, t)| std::cmp::Reverse(t.updated_at));
    (groups, orphans)
}

// MUST KILL: extract: search's 0 / 1 / 2 ranks → a Match enum (NamePrefix, InName, InPathOrHost) ordered best first
pub fn search(groups: Vec<Group>, query: &str) -> Vec<Group> {
    let query = query.to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut ranked: Vec<(usize, Group)> = groups
        .into_iter()
        .filter_map(|g| {
            let name = g.name.to_lowercase();
            let rest = g.members.iter().map(|m| format!("{} {} ", m.host, m.project.path)).collect::<String>().to_lowercase();
            let mut rank = 0;
            for w in &words {
                rank += if name.starts_with(w) {
                    0
                } else if name.contains(w) {
                    1
                } else if rest.contains(w) {
                    2
                } else {
                    return None;
                };
            }
            Some((rank, g))
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, g)| g).collect()
}

pub fn pinned<'a>(groups: &'a [Group], pins: &[String]) -> Vec<&'a Group> {
    groups.iter().filter(|g| pins.contains(&g.key)).collect()
}

pub fn moved(order: &[String], groups: &[Group], key: &str, onto: &str) -> Vec<String> {
    let mut out = order.to_vec();
    out.extend(groups.iter().filter(|g| !order.contains(&g.key)).map(|g| g.key.clone()));
    if let (Some(from), Some(to)) = (out.iter().position(|k| k == key), out.iter().position(|k| k == onto)) {
        let k = out.remove(from);
        out.insert(to, k);
    }
    out
}

// MUST KILL: rename: drop_slot's y → y_below_list_top
pub fn drop_slot(heights: &[f32], gap: f32, dragged: usize, y: f32) -> usize {
    let Some(&own) = heights.get(dragged) else { return dragged };
    let (mut top, mut slot) = (0., dragged);
    for (i, height) in heights.iter().enumerate() {
        let bottom = top + height;
        if i < dragged && y < top + own {
            return i;
        }
        if i > dragged && y > bottom - own {
            slot = i;
        }
        top = bottom + gap;
    }
    slot
}

pub fn rekey(keys: &[String], old: &str, new: &[String]) -> Vec<String> {
    let Some(at) = keys.iter().position(|k| k == old) else { return keys.to_vec() };
    let mut out: Vec<String> = keys.iter().filter(|k| *k != old).cloned().collect();
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
            effort: None,
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
            free_jobs: 0,
            spend: Default::default(),
            forked_from: None,
            setup: None,
            compacting_since: None,
            screen: false,
            locked: false,
        }
    }
    fn host(name: &str, projects: Vec<Project>, threads: Vec<ThreadSummary>) -> HostState {
        HostState {
            name: name.into(),
            config: dirigent_service::hosts::Host::new(String::new()),
            error: None,
            status: HostStatus::Connected,
            projects,
            threads,
            models: vec![],
            service_id: None,
            outbox: Outbox::default(),
            proxy_update: None,
        }
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

        let (groups, orphans) = groups(&hosts, &links, &Names::new(), &[]);
        assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["app", "notes"]);
        let app = &groups[0];
        assert_eq!(app.members.iter().map(|m| m.host.as_str()).collect::<Vec<_>>(), ["box", "local"]);
        assert_eq!(app.threads.iter().map(|(_, t)| t.id.as_str()).collect::<Vec<_>>(), ["t3", "t1"]);
        assert!(app.members.iter().all(|m| !m.linked));
        let notes = &groups[1];
        assert_eq!(notes.members.len(), 2);
        assert_eq!(notes.members.iter().map(|m| (m.host.as_str(), m.linked)).collect::<Vec<_>>(), [("box", true), ("local", false)]);
        assert_eq!(app.members.iter().map(|m| app.threads_of(m)).collect::<Vec<_>>(), [1, 1]);
        assert_eq!(notes.threads_of(&notes.members[0]), 0);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].1.id, "t2");

        assert!(app.has("box", "b") && !app.has("box", "a"));
        assert_eq!(app.member_for(Some("local")).host, "local");
        assert_eq!(app.member_for(None).host, "box");
        assert_eq!(notes.member_for(None).host, "box");

        let links: Links = [(("local".to_owned(), "a".to_owned()), "box/m".to_owned())].into();
        let (groups, _) = super::groups(&hosts, &links, &Names::new(), &[]);
        let joined = groups.iter().find(|g| g.key == "box/m").unwrap();
        assert_eq!((joined.name.as_str(), joined.dir_name.as_str(), joined.members.len()), ("notes", "notes", 2));
    }

    #[test]
    fn user_order_then_activity() {
        let hosts = vec![host(
            "local",
            vec![project("a", "/a", None), project("b", "/b", None), project("c", "/c", None), project("d", "/d", None)],
            vec![thread("t1", Some("a"), 1), thread("t2", Some("b"), 2), thread("t3", Some("c"), 3), thread("t4", Some("d"), 4)],
        )];
        let names: Names = [("local/b".to_owned(), "bee".to_owned())].into();
        let names_of = |gs: &[&Group]| gs.iter().map(|g| g.name.clone()).collect::<Vec<_>>();
        let (groups, _) = groups(&hosts, &Links::new(), &names, &[]);
        assert_eq!(names_of(&groups.iter().collect::<Vec<_>>()), ["d", "c", "bee", "a"]);
        let order: Vec<String> = ["local/a", "gone", "local/b"].map(String::from).to_vec();
        let (groups, _) = super::groups(&hosts, &Links::new(), &names, &order);
        assert_eq!(names_of(&groups.iter().collect::<Vec<_>>()), ["a", "bee", "d", "c"]);
        let pins = ["local/c".into(), "local/a".into()];
        assert_eq!(names_of(&pinned(&groups, &pins)), ["a", "c"]);
    }

    #[test]
    fn search_puts_names_before_paths() {
        let hosts = vec![host(
            "local",
            vec![
                project("a", "/home/me/work/sebba.dev", None),
                project("b", "/home/me/work/homework", None),
                project("c", "/home/me/workspace", None),
                project("d", "/home/me/notes", None),
            ],
            vec![],
        )];
        let names = |query: &str| {
            let (groups, _) = groups(&hosts, &Links::new(), &Names::new(), &[]);
            search(groups, query).into_iter().map(|g| g.name).collect::<Vec<_>>()
        };
        assert_eq!(names(""), ["homework", "notes", "sebba.dev", "workspace"]);
        assert_eq!(names("Work"), ["workspace", "homework", "sebba.dev"]);
        assert_eq!(names("local no"), ["notes"]);
    }

    #[test]
    fn moved_takes_the_place_of_onto() {
        let hosts = vec![host("local", vec![project("a", "/a", None), project("b", "/b", None), project("c", "/c", None)], vec![])];
        let order: Vec<String> = ["gone", "local/b"].map(String::from).to_vec();
        let (groups, _) = groups(&hosts, &Links::new(), &Names::new(), &order);
        assert_eq!(moved(&order, &groups, "local/b", "local/c"), ["gone", "local/a", "local/c", "local/b"]);
        assert_eq!(moved(&order, &groups, "local/c", "local/b"), ["gone", "local/c", "local/b", "local/a"]);
        assert_eq!(moved(&order, &groups, "local/a", "local/a"), ["gone", "local/b", "local/a", "local/c"]);
    }

    #[test]
    fn drop_slot_follows_the_mouse_past_rows_of_any_height() {
        let even = [36., 36., 36.];
        assert_eq!(drop_slot(&even, 4., 0, 38.), 0);
        assert_eq!(drop_slot(&even, 4., 0, 45.), 1);
        assert_eq!(drop_slot(&even, 4., 0, 500.), 2);
        assert_eq!(drop_slot(&even, 4., 2, 50.), 1);
        assert_eq!(drop_slot(&even, 4., 2, -5.), 0);
        let down = [36., 120., 36.];
        assert_eq!(drop_slot(&down, 4., 0, 100.), 0);
        assert_eq!(drop_slot(&down, 4., 0, 130.), 1);
        let up = [120., 36., 36.];
        assert_eq!(drop_slot(&up, 4., 1, 130.), 1);
        assert_eq!(drop_slot(&up, 4., 1, 100.), 1);
        assert_eq!(drop_slot(&up, 4., 1, 30.), 0);
        assert_eq!(drop_slot(&down, 4., 0, 30.), 0);
        assert_eq!(drop_slot(&even, 4., 7, 50.), 7);
    }

    #[test]
    fn rekey_keeps_the_place() {
        let keys: Vec<String> = ["x", "old", "y"].map(String::from).to_vec();
        assert_eq!(rekey(&keys, "old", &["new".into()]), ["x", "new", "y"]);
        assert_eq!(rekey(&keys, "old", &["y".into()]), ["x", "y"]);
        assert_eq!(rekey(&keys, "old", &["a".into(), "b".into(), "a".into()]), ["x", "a", "b", "y"]);
        assert_eq!(rekey(&keys, "z", &["new".into()]), keys);
    }
}
