use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Default)]
pub struct Live {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    running: HashMap<String, i64>,
    jobs: HashMap<String, Jobs>,
    nix_shells: HashSet<String>,
    compacting: HashMap<String, i64>,
}

#[derive(Clone, Copy, Default)]
pub struct Jobs {
    pub waited_on: u32,
    pub left_running: u32,
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

    pub fn jobs(&self, thread_id: &str) -> Jobs {
        self.inner.lock().unwrap().jobs.get(thread_id).copied().unwrap_or_default()
    }

    pub fn set_jobs(&self, thread_id: &str, waited_on: u32, left_running: u32) {
        let mut inner = self.inner.lock().unwrap();
        match (waited_on, left_running) {
            (0, 0) => inner.jobs.remove(thread_id),
            _ => inner.jobs.insert(thread_id.to_owned(), Jobs { waited_on, left_running }),
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

    pub fn unloaded(&self, thread_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.jobs.remove(thread_id);
        inner.nix_shells.remove(thread_id);
        inner.compacting.remove(thread_id);
    }
}
