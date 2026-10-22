//! Workflows: deterministic scripts a project keeps in `.dirigent/workflows/*.ts`, each
//! exporting a `{ name, description, input, run(d, input) }` object (see the runtime's
//! `workflow.ts`). The host describes them by loading them with bun, and runs one as a
//! `bun` child in the project directory: the script talks to this service over ws like
//! any client (`DIRIGENT_URL`), reports its steps (`ReportStep`) and its stdout / stderr
//! go to `$XDG_DATA_HOME/dirigent/runs/<run>.log`. The run ends when the process does.

use anyhow::{Result, anyhow};
use dirigent_protocol::Workflow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use tokio::process::Command;

/// Where a project keeps its workflows, relative to its directory.
pub const DIR: &str = ".dirigent/workflows";
/// A `GetRun` returns at most this much of the log, from the end.
const LOG_TAIL: usize = 64 * 1024;
const PLAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Live runs by id: the process group to kill on `AbortRun`.
#[derive(Default)]
pub struct Runs {
    pids: Mutex<HashMap<String, u32>>,
}

impl Runs {
    /// SIGTERM the run's process group (the child is its own group, so whatever it
    /// spawned goes too). False if the run isn't live here.
    pub fn abort(&self, run_id: &str) -> bool {
        let Some(pid) = self.pids.lock().unwrap().remove(run_id) else { return false };
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        // SAFETY: plain libc call with a pid we spawned.
        unsafe {
            kill(-(pid as i32), 15);
        }
        true
    }

    /// The run ended on its own.
    pub fn forget(&self, run_id: &str) {
        self.pids.lock().unwrap().remove(run_id);
    }
}

/// The scripts in the project's workflow dir, sorted by name.
pub fn scripts(project: &Path) -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir(project.join(DIR)) else { return vec![] };
    let mut files: Vec<PathBuf> =
        dir.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "ts") && p.is_file()).collect();
    files.sort();
    files
}

/// Load every script with bun and read back what it declares (`workflow.ts describe`).
pub async fn describe(runner: &Path, project: &Path) -> Result<Vec<Workflow>> {
    let files = scripts(project);
    if files.is_empty() {
        return Ok(vec![]);
    }
    let out = Command::new("bun")
        .arg("run")
        .arg(runner)
        .arg("describe")
        .args(&files)
        .current_dir(project)
        .output()
        .await
        .map_err(|e| anyhow!("run bun: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("describe workflows: {}", err.lines().last().unwrap_or("bun failed")));
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// The steps the script takes against placeholder results (`workflow.ts plan`), with the
/// run's input. A script that fails here never starts for real. Bounded by `PLAN_TIMEOUT`.
pub async fn plan(runner: &Path, project: &Path, file: &Path, env: &[(&str, String)]) -> Result<Vec<serde_json::Value>> {
    let run = Command::new("bun")
        .arg("run")
        .arg(runner)
        .arg("plan")
        .arg(file)
        .current_dir(project)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(PLAN_TIMEOUT, run).await.map_err(|_| anyhow!("planning the workflow took too long"))??;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().find(|l| l.contains("error")).or_else(|| err.lines().last()).unwrap_or("bun failed");
        return Err(anyhow!("workflow failed to plan: {}", line.trim()));
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Start the script as `bun run workflow.ts run <file>` in the project directory, logging
/// to `log`. Returns once it is spawned; `exited` resolves with its exit code.
pub fn start(
    runs: &Runs,
    runner: &Path,
    project: &Path,
    file: &Path,
    log: &Path,
    env: &[(&str, String)],
) -> Result<impl Future<Output = i64> + Send + 'static> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::File::create(log)?;
    let err = out.try_clone()?;
    let mut child = Command::new("bun")
        .arg("run")
        .arg(runner)
        .arg("run")
        .arg(file)
        .current_dir(project)
        .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .process_group(0)
        .spawn()
        .map_err(|e| anyhow!("spawn bun: {e}"))?;
    let run_id = env.iter().find(|(k, _)| *k == "DIRIGENT_RUN").map(|(_, v)| v.clone()).unwrap_or_default();
    if let Some(pid) = child.id() {
        runs.pids.lock().unwrap().insert(run_id, pid);
    }
    Ok(async move {
        match child.wait().await {
            Ok(status) => status.code().map(i64::from).unwrap_or(128),
            Err(_) => 127,
        }
    })
}

/// The last `LOG_TAIL` bytes of a run's log, if it has one.
pub fn log_tail(log: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(log) else { return String::new() };
    if text.len() <= LOG_TAIL {
        return text;
    }
    let cut = text.len() - LOG_TAIL;
    let start = text[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
    format!("…\n{}", &text[start..])
}
