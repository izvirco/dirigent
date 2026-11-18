use dirigent_protocol::{ClientMessage, Project, ThreadSummary};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Change {
    RemoveProject { project_id: String },
    Archive { thread_id: String, archived: bool, updated_at: i64 },
    Delete { thread_id: String },
    Rename { thread_id: String, name: String },
    Pin { thread_id: String, pinned: bool },
    Snooze { thread_id: String, snoozed: bool, updated_at: i64 },
    Seen { thread_id: String, updated_at: i64 },
    PinMessage { thread_id: String, entry_id: String, pinned: bool },
}

impl Change {
    fn request(&self, id: u64) -> ClientMessage {
        match self.clone() {
            Change::RemoveProject { project_id } => ClientMessage::RemoveProject { id, project_id },
            Change::Archive { thread_id, archived, .. } => ClientMessage::ArchiveThread { id, thread_id, archived },
            Change::Delete { thread_id } => ClientMessage::DeleteThread { id, thread_id },
            Change::Rename { thread_id, name } => ClientMessage::RenameThread { id, thread_id, name },
            Change::Pin { thread_id, pinned } => ClientMessage::PinThread { id, thread_id, pinned },
            Change::Snooze { thread_id, snoozed, .. } => ClientMessage::SnoozeThread { id, thread_id, snoozed },
            Change::Seen { thread_id, updated_at } => ClientMessage::MarkSeen { id, thread_id, updated_at },
            Change::PinMessage { thread_id, entry_id, pinned } => ClientMessage::PinMessage { id, thread_id, entry_id, pinned },
        }
    }

    pub fn apply(&self, projects: &mut Vec<Project>, threads: &mut Vec<ThreadSummary>) {
        match self {
            Change::RemoveProject { project_id } => projects.retain(|p| p.id != *project_id),
            Change::Delete { thread_id } => threads.retain(|t| t.id != *thread_id),
            Change::Archive { thread_id, archived, .. } => on(threads, thread_id, |t| t.archived = *archived),
            Change::Rename { thread_id, name } => on(threads, thread_id, |t| t.name = Some(name.clone())),
            Change::Pin { thread_id, pinned } => on(threads, thread_id, |t| t.pinned = *pinned),
            Change::Snooze { thread_id, snoozed, .. } => on(threads, thread_id, |t| t.snoozed = *snoozed),
            Change::Seen { thread_id, updated_at } => on(threads, thread_id, |t| t.seen_at = t.seen_at.max(Some(*updated_at))),
            Change::PinMessage { thread_id, entry_id, pinned } => on(threads, thread_id, |t| {
                t.pinned_entries.retain(|e| e != entry_id);
                if *pinned {
                    t.pinned_entries.push(entry_id.clone());
                }
            }),
        }
    }

    fn put_away_a_thread_that_went_on_since(&self, threads: &[ThreadSummary]) -> bool {
        let (Change::Archive { thread_id, archived: true, updated_at } | Change::Snooze { thread_id, snoozed: true, updated_at }) = self
        else {
            return false;
        };
        threads.iter().any(|t| t.id == *thread_id && t.updated_at > *updated_at)
    }
}

fn on(threads: &mut [ThreadSummary], id: &str, change: impl FnOnce(&mut ThreadSummary)) {
    if let Some(t) = threads.iter_mut().find(|t| t.id == id) {
        change(t);
    }
}

#[derive(Default)]
pub struct Outbox {
    unanswered: Vec<Change>,
    delivery: Delivery,
}

#[derive(Default, PartialEq)]
enum Delivery {
    #[default]
    HostHasNotListedItsThreads,
    Ready,
    AwaitingAnswer(u64),
}

impl Outbox {
    pub fn new(unanswered: Vec<Change>) -> Self {
        Self { unanswered, delivery: Delivery::default() }
    }

    pub fn unanswered(&self) -> &[Change] {
        &self.unanswered
    }

    pub fn push(&mut self, change: Change) {
        self.unanswered.push(change);
    }

    pub fn apply(&self, projects: &mut Vec<Project>, threads: &mut Vec<ThreadSummary>) {
        for change in &self.unanswered {
            change.apply(projects, threads);
        }
    }

    pub fn host_listed_its_threads(&mut self, threads: &[ThreadSummary]) {
        self.unanswered.retain(|change| !change.put_away_a_thread_that_went_on_since(threads));
        self.delivery = Delivery::Ready;
    }

    pub fn disconnected(&mut self) {
        self.delivery = Delivery::HostHasNotListedItsThreads;
    }

    pub fn next_request(&mut self, id: u64) -> Option<ClientMessage> {
        if self.delivery != Delivery::Ready {
            return None;
        }
        let request = self.unanswered.first()?.request(id);
        self.delivery = Delivery::AwaitingAnswer(id);
        Some(request)
    }

    pub fn answered(&mut self, id: u64) -> bool {
        if self.delivery != Delivery::AwaitingAnswer(id) {
            return false;
        }
        self.delivery = Delivery::Ready;
        self.unanswered.remove(0);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str) -> ThreadSummary {
        ThreadSummary { id: id.into(), ..Default::default() }
    }

    fn project(id: &str) -> Project {
        Project { id: id.into(), path: format!("/{id}"), origin: None, created_at: 0 }
    }

    fn archive(thread_id: &str, archived: bool) -> Change {
        Change::Archive { thread_id: thread_id.into(), archived, updated_at: 0 }
    }

    fn connected(unanswered: Vec<Change>) -> Outbox {
        let mut outbox = Outbox::new(unanswered);
        outbox.host_listed_its_threads(&[]);
        outbox
    }

    #[test]
    fn each_change_shows_as_the_host_will_list_it() {
        let mut projects = vec![project("p"), project("q")];
        let mut threads = vec![ThreadSummary { seen_at: Some(7), pinned_entries: vec!["e1".into()], ..thread("t") }, thread("u")];
        for change in [
            Change::RemoveProject { project_id: "p".into() },
            archive("t", true),
            Change::Rename { thread_id: "t".into(), name: "new".into() },
            Change::Pin { thread_id: "t".into(), pinned: true },
            Change::Snooze { thread_id: "t".into(), snoozed: true, updated_at: 0 },
            Change::Seen { thread_id: "t".into(), updated_at: 9 },
            Change::Seen { thread_id: "t".into(), updated_at: 3 },
            Change::PinMessage { thread_id: "t".into(), entry_id: "e2".into(), pinned: true },
            Change::PinMessage { thread_id: "t".into(), entry_id: "e1".into(), pinned: false },
            Change::Delete { thread_id: "u".into() },
            Change::Rename { thread_id: "not listed".into(), name: "x".into() },
        ] {
            change.apply(&mut projects, &mut threads);
        }
        assert_eq!(projects, [project("q")]);
        let changed = ThreadSummary {
            archived: true,
            name: Some("new".into()),
            pinned: true,
            snoozed: true,
            seen_at: Some(9),
            pinned_entries: vec!["e2".into()],
            ..thread("t")
        };
        assert_eq!(threads, [changed]);
    }

    #[test]
    fn changes_show_on_the_hosts_lists_until_it_answered_them() {
        let mut outbox = connected(vec![Change::RemoveProject { project_id: "p".into() }, archive("t", true)]);
        let listed = |outbox: &Outbox| {
            let (mut projects, mut threads) = (vec![project("p")], vec![thread("t")]);
            outbox.apply(&mut projects, &mut threads);
            (projects.len(), threads[0].archived)
        };
        assert_eq!(listed(&outbox), (0, true));

        outbox.next_request(1);
        assert!(outbox.answered(1));
        assert_eq!(listed(&outbox), (1, true));

        outbox.next_request(2);
        assert!(outbox.answered(2));
        assert_eq!(listed(&outbox), (1, false));
    }

    #[test]
    fn changes_reach_the_host_one_at_a_time_in_the_order_made() {
        let mut outbox = connected(vec![archive("t", true)]);
        outbox.push(archive("t", false));
        assert_eq!(outbox.next_request(1), Some(ClientMessage::ArchiveThread { id: 1, thread_id: "t".into(), archived: true }));
        assert_eq!(outbox.next_request(2), None);
        assert!(!outbox.answered(2));
        assert!(outbox.answered(1));
        assert_eq!(outbox.next_request(3), Some(ClientMessage::ArchiveThread { id: 3, thread_id: "t".into(), archived: false }));
        assert!(outbox.answered(3));
        assert_eq!(outbox.next_request(4), None);
    }

    #[test]
    fn changes_wait_for_the_hosts_thread_list_so_it_cannot_arrive_later_and_show_them_undone() {
        let mut outbox = Outbox::new(vec![archive("t", true)]);
        assert_eq!(outbox.next_request(1), None);
        outbox.host_listed_its_threads(&[]);
        assert!(outbox.next_request(2).is_some());
    }

    #[test]
    fn an_archive_or_snooze_is_dropped_when_its_thread_went_on_while_the_host_was_away() {
        let seen_at_5 = |thread_id: &str| {
            [
                Change::Archive { thread_id: thread_id.into(), archived: true, updated_at: 5 },
                Change::Snooze { thread_id: thread_id.into(), snoozed: true, updated_at: 5 },
            ]
        };
        let kept_whatever_the_thread_did = [
            Change::Archive { thread_id: "went on".into(), archived: false, updated_at: 5 },
            Change::Snooze { thread_id: "went on".into(), snoozed: false, updated_at: 5 },
            Change::Rename { thread_id: "went on".into(), name: "new".into() },
            Change::Delete { thread_id: "went on".into() },
        ];
        let mut outbox = Outbox::new([&seen_at_5("went on")[..], &seen_at_5("at rest")[..], &kept_whatever_the_thread_did[..]].concat());
        let listed = [ThreadSummary { updated_at: 9, ..thread("went on") }, ThreadSummary { updated_at: 5, ..thread("at rest") }];
        outbox.host_listed_its_threads(&listed);
        assert_eq!(outbox.unanswered(), [&seen_at_5("at rest")[..], &kept_whatever_the_thread_did[..]].concat());
    }

    #[test]
    fn a_change_the_host_never_answered_goes_out_again_after_a_reconnect() {
        let mut outbox = connected(vec![Change::Delete { thread_id: "t".into() }]);
        assert!(outbox.next_request(1).is_some());
        outbox.disconnected();
        assert_eq!(outbox.next_request(2), None);
        outbox.host_listed_its_threads(&[]);
        assert_eq!(outbox.next_request(3), Some(ClientMessage::DeleteThread { id: 3, thread_id: "t".into() }));
        assert!(!outbox.answered(1));
        assert_eq!(outbox.unanswered().len(), 1);
    }

    #[test]
    fn a_new_connections_thread_list_sends_the_change_again_though_the_old_ones_end_went_unseen() {
        let mut outbox = connected(vec![archive("t", true)]);
        assert!(outbox.next_request(1).is_some());
        outbox.host_listed_its_threads(&[]);
        assert_eq!(outbox.next_request(2), Some(ClientMessage::ArchiveThread { id: 2, thread_id: "t".into(), archived: true }));
    }
}
