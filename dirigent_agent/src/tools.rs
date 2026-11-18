pub mod bash;
pub mod computer;
pub mod files;
pub mod images;
pub mod shell;
pub mod truncate;

use crate::Host;
use crate::config::Dirs;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

pub struct Env {
    pub thread_id: String,
    pub cwd: PathBuf,
    pub dirs: Dirs,
    pub host: Arc<dyn Host>,
    pub out: tokio::sync::mpsc::UnboundedSender<(String, crate::Out)>,
    pub shell_env: RwLock<HashMap<String, String>>,
}

#[cfg(test)]
impl Env {
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
