//! The built-in tools: `read`, `bash`, `edit` and `write`, which every thread has, and
//! `async_bash` and `ask_user`, which a thread has while their switch is on.

pub mod bash;
pub mod files;
pub mod images;
pub mod shell;
pub mod truncate;

use crate::Host;
use crate::config::Dirs;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// What a session's tools work in.
pub struct Env {
    pub thread_id: String,
    /// The thread's directory: where commands run and relative paths start.
    pub cwd: PathBuf,
    pub dirs: Dirs,
    pub host: Arc<dyn Host>,
    /// Where what comes of the session goes (`Agent::out`), with the thread's id.
    pub out: tokio::sync::mpsc::UnboundedSender<(String, crate::Out)>,
    /// What commands get in their environment on top of the service's own: the project's nix
    /// dev shell, once the session has captured it.
    pub shell_env: RwLock<HashMap<String, String>>,
}

#[cfg(test)]
impl Env {
    /// An environment in `dir`, with a host that takes nothing in.
    pub fn test(dir: &std::path::Path) -> Self {
        Self {
            thread_id: "thread".into(),
            cwd: dir.to_owned(),
            dirs: Dirs { config: dir.join("config"), data: dir.join("data") },
            host: Arc::new(crate::testing::Deaf),
            out: tokio::sync::mpsc::unbounded_channel().0,
            shell_env: RwLock::default(),
        }
    }
}
