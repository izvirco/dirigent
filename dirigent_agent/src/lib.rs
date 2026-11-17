pub mod codemode;
pub mod config;
mod context;
pub mod import;
pub mod mcp;
mod prompt;
pub mod proxy;
mod run;
pub mod session;
#[cfg(test)]
mod testing;
pub mod tool;
mod tools;

use async_trait::async_trait;
use config::Dirs;
use dirigent_protocol::Ask;
use dirigent_protocol::transcript::{Entry, Event};
use proxy::{Chunk, Model, Reply, Request};
use session::{Session, Thread};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tool::Tool;

pub use context::complete;

#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// The thread's next entry (`seq` is the host's to give).
    Entry(Entry),
    Event(Event),
    /// The model called `ask_user`: the host keeps the ask until the user answers it.
    Ask(Ask),
    /// How many `async_bash` jobs run: those the thread waits on (their end wakes it), and
    /// those it left running (`free`: servers, watchers).
    Jobs {
        running: u32,
        free: u32,
    },
    /// The session started (`true`) or finished capturing the project's nix dev shell.
    NixShell(bool),
    Screen(Screen),
}

/// A step of a thread's computer use: the picture of the window it works in, when that
/// changed, what the agent did there and where it clicked (as fractions of the picture),
/// and whether that was on the user's desktop (not on a screen of the thread's own).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Screen {
    /// PNG bytes.
    pub image: Option<Vec<u8>>,
    pub action: Option<String>,
    pub cursor: Option<[f32; 2]>,
    pub desktop: bool,
}

pub trait Host: Send + Sync + 'static {
    /// Take what came of the thread's session. One at a time and in the order it came
    /// about, across all threads: an entry is only said once the host has it.
    fn emit(&self, thread_id: &str, out: Out);

    /// Keep an image's bytes and return their blob id (their sha256, hex): what entries
    /// name images by.
    fn put_blob(&self, mime: &str, bytes: &[u8]) -> String;

    fn blob(&self, id: &str) -> Option<(String, Vec<u8>)>;

    /// Whether one of the thread's sub-agents runs or has jobs: what `async_bash wait` may
    /// still hear from. `true` when the host can't say.
    fn children_busy(&self, thread_id: &str) -> bool;

    fn tools(&self, thread_id: &str) -> Vec<Arc<dyn Tool>>;
}

#[async_trait]
pub trait Llm: Send + Sync + 'static {
    /// The model `id`, as it is served now. An error says why it can't be used, in words
    /// for the user (it isn't served, no proxy is set up).
    async fn model(&self, id: &str) -> anyhow::Result<Model>;

    /// Stream one reply: its pieces go to `on` as they come, the whole of it is returned.
    /// Never fails: a failure is in the `Reply`. `cancel` ends it with what it has.
    async fn stream(&self, request: &Request<'_>, on: &mut (dyn FnMut(Chunk) + Send), cancel: &CancellationToken) -> Reply;
}

#[async_trait]
impl Llm for proxy::Proxy {
    async fn model(&self, id: &str) -> anyhow::Result<Model> {
        proxy::Proxy::model(self, id).await
    }

    async fn stream(&self, request: &Request<'_>, on: &mut (dyn FnMut(Chunk) + Send), cancel: &CancellationToken) -> Reply {
        proxy::Proxy::stream(self, request, on, cancel).await
    }
}

pub struct Agent {
    pub dirs: Dirs,
    host: Arc<dyn Host>,
    pub llm: Arc<dyn Llm>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    out: mpsc::UnboundedSender<(String, Out)>,
}

impl Agent {
    /// Must be called within a tokio runtime. One per data directory: what an agent before
    /// it left of its threads' screens there is forgotten.
    pub fn new(dirs: Dirs, host: Arc<dyn Host>, llm: Arc<dyn Llm>) -> Arc<Self> {
        tools::computer::forget_screens(&dirs);
        let (out, mut outs) = mpsc::unbounded_channel::<(String, Out)>();
        tokio::spawn({
            let host = host.clone();
            async move {
                while let Some((thread_id, out)) = outs.recv().await {
                    // MUST KILL: extract: an emit_catching_panics(host, thread_id, out) fn out of Agent::new's delivery task
                    let taken = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.emit(&thread_id, out)));
                    if let Err(panic) = taken {
                        tracing::error!(thread = %thread_id, "the host panicked over what came of a session: {}", panic_message(&*panic));
                    }
                }
            }
        });
        Arc::new(Self { dirs, host, llm, sessions: Mutex::default(), out })
    }

    /// The thread's session: the live one if it has one (`thread` is not looked at then),
    /// else a new one from `thread`.
    pub fn load(&self, thread: Thread) -> Arc<Session> {
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(session) = sessions.get(&thread.id) {
            return session.clone();
        }
        let id = thread.id.clone();
        let session = Session::new(&self.dirs, self.host.clone(), self.llm.clone(), self.out.clone(), thread);
        sessions.insert(id, session.clone());
        session
    }

    pub fn session(&self, thread_id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(thread_id).cloned()
    }

    /// Drop the thread's session: its run stops, its jobs are killed, nothing more comes of it.
    pub fn unload(&self, thread_id: &str) {
        if let Some(session) = self.sessions.lock().unwrap().remove(thread_id) {
            session.unload();
        }
    }

    /// Unload the sessions that do nothing (no run, no message on its way in, no jobs:
    /// unloading would kill them, even those left running) and have done nothing for longer
    /// than `max_idle`. Returns their threads. Found and dropped under one lock: no prompt
    /// finds its session gone under it.
    pub fn unload_idle(&self, max_idle: Duration) -> Vec<String> {
        let mut sessions = self.sessions.lock().unwrap();
        let idle: Vec<String> = sessions.iter().filter(|(_, s)| !s.busy() && s.idle_for() >= max_idle).map(|(id, _)| id.clone()).collect();
        for id in &idle {
            if let Some(session) = sessions.remove(id) {
                session.unload();
            }
        }
        idle
    }

    /// The host is going (the desktop quits, the service is told to stop): end what the
    /// sessions started, none of which may outlive it. Every job's process group is killed
    /// before this first waits, and at once: the process is about to exit, and no task
    /// lives to see a grace through. Then the sessions' MCP servers and screens are closed,
    /// which waits for their commands to exit (a few seconds at the most). No session is
    /// left, nothing more comes of any, and no job starts in one. Call it last.
    pub async fn shutdown(&self) {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().drain().map(|(_, session)| session).collect();
        for session in &sessions {
            session.end(true);
        }
        futures_util::future::join_all(sessions.iter().map(|session| session.close())).await;
    }
}

pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic.downcast_ref::<&str>().copied().or_else(|| panic.downcast_ref::<String>().map(String::as_str)).unwrap_or("no message")
}

// MUST KILL: rename: now -> unix_ms
pub(crate) fn now() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub(crate) fn short_id(len: usize) -> String {
    uuid::Uuid::new_v4().simple().to_string()[..len].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Prompt;
    use crate::testing::{Script, Step, agent, prompt, session, text, thread};

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_the_jobs_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (_, agent) = agent(dir.path(), Script::new(vec![]), vec![]);
        let session = agent.load(thread(dir.path()));
        let job = session.jobs.spawn(&session.env, "trap '' TERM; sleep 30", dir.path()).unwrap();
        agent.shutdown().await;
        let ended = tokio::time::timeout(Duration::from_secs(1), job.exited()).await;
        assert_eq!(ended.expect("the job outlived the shutdown"), 128 + libc::SIGKILL);
        assert!(agent.session("thread").is_none());
        assert!(session.jobs.spawn(&session.env, "true", dir.path()).is_err());
    }

    #[tokio::test]
    async fn a_session_taking_a_prompt_in_is_not_unloaded_as_idle() {
        let dir = tempfile::tempdir().unwrap();
        let (host, agent) = agent(dir.path(), Script::new(vec![Step::Say(vec![text("hi")])]), vec![]);
        let session = agent.load(thread(dir.path()));
        let sent = tokio::spawn({
            let session = session.clone();
            async move { session.prompt(Prompt { model: Some("slow".into()), ..prompt("go") }).await }
        });
        while !session.busy() {
            tokio::task::yield_now().await;
        }
        assert!(session.state.lock().unwrap().run.is_none());
        assert!(agent.unload_idle(Duration::ZERO).is_empty());
        sent.await.unwrap().unwrap();
        host.settled(1).await;
        assert_eq!(agent.unload_idle(Duration::ZERO), ["thread"]);
    }

    #[tokio::test]
    async fn the_host_gets_what_comes_after_something_it_panicked_over() {
        let dir = tempfile::tempdir().unwrap();
        let (host, session) = session(dir.path(), Script::new(vec![]));
        host.trip.store(true, std::sync::atomic::Ordering::Relaxed);
        session.warn("lost".into());
        session.warn("kept".into());
        host.until(|outs| *outs == [Out::Event(Event::Warning { message: "kept".into() })]).await;
    }
}
