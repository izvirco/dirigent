//! dirigent's agent: what turns a thread's history and a prompt into a model's work. It
//! talks to the model (`proxy`), runs the loop of replies and tool calls (`session`, `run`),
//! and has the tools (`tools`, `codemode`, `mcp`). A library: the host (`dirigent_service`)
//! owns the threads and their storage, loads one into a session here when it has work,
//! and hears what comes of it through `Host`.
//!
//! `DESIGN.md` has the whole picture.

pub mod codemode;
pub mod config;
pub mod mcp;
pub mod proxy;
pub mod tool;

use dirigent_protocol::Ask;
use dirigent_protocol::transcript::{Entry, Event};
use std::sync::Arc;
use tool::Tool;

/// What comes of a session, for its host to keep and pass on.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// The thread's next entry (`seq` is the host's to give): what was said is only said
    /// once the host has it.
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
    /// A step of the thread's computer use.
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

/// What the agent needs of its host. Called from the sessions' tasks: every method returns
/// at once (none may wait on a session).
pub trait Host: Send + Sync + 'static {
    /// Take what came of the thread's session.
    fn emit(&self, thread_id: &str, out: Out);

    /// Keep an image's bytes and return their blob id (their sha256, hex): what entries
    /// name images by.
    fn put_blob(&self, mime: &str, bytes: &[u8]) -> String;

    /// A blob's mime type and bytes.
    fn blob(&self, id: &str) -> Option<(String, Vec<u8>)>;

    /// Whether one of the thread's sub-agents runs or has jobs: what `async_bash wait` may
    /// still hear from. `true` when the host can't say.
    fn children_busy(&self, thread_id: &str) -> bool;

    /// The host's own tools for a session of the thread (`dirigent`).
    fn tools(&self, thread_id: &str) -> Vec<Arc<dyn Tool>>;
}
