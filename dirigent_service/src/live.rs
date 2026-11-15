//! What the service knows of its threads' work that isn't stored: which run (and since
//! when), have jobs, capture a nix dev shell or compact their context. Kept from what the
//! agent says of its sessions (`Service::on_out`), and from the service itself where it
//! knows sooner: a thread counts as running from its prompt, not from the session's
//! `RunStart` (`begin_run`).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Default)]
pub struct Live {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Threads with a run in progress, with its start (unix ms): from the prompt being
    /// handed over (`begin_run`) or the session's `RunStart`, until `settle`.
    running: HashMap<String, i64>,
    /// Running `async_bash` jobs per thread: those it waits on, and those it left running.
    /// Absent: none.
    jobs: HashMap<String, (u32, u32)>,
    /// Threads whose session is capturing its nix dev shell.
    nix_shells: HashSet<String>,
    /// Threads whose context is being compacted, since when (unix ms).
    compacting: HashMap<String, i64>,
}

impl Live {
    pub fn running_since(&self, thread_id: &str) -> Option<i64> {
        self.inner.lock().unwrap().running.get(thread_id).copied()
    }

    /// A prompt is on its way, or the session says its run began: count the thread as
    /// running from now, unless it already does.
    pub fn begin_run(&self, thread_id: &str) {
        self.inner.lock().unwrap().running.entry(thread_id.to_owned()).or_insert_with(crate::now);
    }

    /// The run is over: forget it and return how long it took.
    pub fn settle(&self, thread_id: &str) -> Option<i64> {
        let mut inner = self.inner.lock().unwrap();
        // A compaction never outlives its run.
        inner.compacting.remove(thread_id);
        inner.running.remove(thread_id).map(|since| crate::now() - since)
    }

    /// How many `async_bash` jobs the thread has running that it waits on, and that it left running.
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

    /// The thread's session is gone: what only a session has goes with it. A run begun for
    /// a session still to load (`begin_run`, its prompt on the way) is not the session's to
    /// end: `loaded` says whether there was one.
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
