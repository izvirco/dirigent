use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Default)]
pub struct Live {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    running: HashMap<String, i64>,
    jobs: HashMap<String, (u32, u32)>,
    nix_shells: HashSet<String>,
    compacting: HashMap<String, i64>,
}

impl Live {
    pub fn running_since(&self, thread_id: &str) -> Option<i64> {
        self.inner.lock().unwrap().running.get(thread_id).copied()
    }

    pub fn begin_run(&self, thread_id: &str) {
        self.inner.lock().unwrap().running.entry(thread_id.to_owned()).or_insert_with(crate::now);
    }

    pub fn settle(&self, thread_id: &str) -> Option<i64> {
        let mut inner = self.inner.lock().unwrap();
        inner.compacting.remove(thread_id);
        inner.running.remove(thread_id).map(|since| crate::now() - since)
    }

    // MUST KILL: type: the bare (u32, u32) is (waited-on, left-running) jobs; return a named struct
    pub fn jobs(&self, thread_id: &str) -> (u32, u32) {
        self.inner.lock().unwrap().jobs.get(thread_id).copied().unwrap_or_default()
    }

    pub fn set_jobs(&self, thread_id: &str, running: u32, free: u32) {
        let mut inner = self.inner.lock().unwrap();
        match (running, free) {
            (0, 0) => inner.jobs.remove(thread_id),
            jobs => inner.jobs.insert(thread_id.to_owned(), jobs),
        };
    }

    pub fn nix_shell(&self, thread_id: &str) -> bool {
        self.inner.lock().unwrap().nix_shells.contains(thread_id)
    }

    pub fn set_nix_shell(&self, thread_id: &str, active: bool) {
        let mut inner = self.inner.lock().unwrap();
        match active {
            true => inner.nix_shells.insert(thread_id.to_owned()),
            false => inner.nix_shells.remove(thread_id),
        };
    }

    pub fn compacting_since(&self, thread_id: &str) -> Option<i64> {
        self.inner.lock().unwrap().compacting.get(thread_id).copied()
    }

    pub fn set_compacting(&self, thread_id: &str, compacting: bool) {
        let mut inner = self.inner.lock().unwrap();
        match compacting {
            true => inner.compacting.insert(thread_id.to_owned(), crate::now()),
            false => inner.compacting.remove(thread_id),
        };
    }

    // MUST KILL: type: the loaded bool decides whether a run begun before its session loaded survives; use an enum
    pub fn unloaded(&self, thread_id: &str, loaded: bool) {
        let mut inner = self.inner.lock().unwrap();
        if loaded {
            inner.running.remove(thread_id);
        }
        inner.jobs.remove(thread_id);
        inner.nix_shells.remove(thread_id);
        inner.compacting.remove(thread_id);
    }
}
