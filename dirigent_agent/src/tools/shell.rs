use super::Env;
use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::watch;

#[cfg(unix)]
const STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
#[cfg(unix)]
const STOP_POLL: std::time::Duration = std::time::Duration::from_millis(50);
const TAIL_LINES: usize = 30;
const TAIL_CHARS: usize = 4000;
const TAIL_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
struct Shell {
    program: PathBuf,
    args: Vec<&'static str>,
    /// The command goes in on stdin, not as an argument (the old WSL launcher mangles arguments).
    stdin: bool,
}

// MUST KILL: extract: a cache that only keeps a found shell, so a lookup that failed visibly runs again next time
fn shell() -> Result<Shell> {
    static SHELL: OnceLock<Shell> = OnceLock::new();
    if let Some(shell) = SHELL.get() {
        return Ok(shell.clone());
    }
    let shell = find_shell()?;
    Ok(SHELL.get_or_init(|| shell).clone())
}

fn find_shell() -> Result<Shell> {
    let on_path = |name: &str| {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| p.is_file())
    };
    let arg = |program: PathBuf| Shell { program, args: vec!["-c"], stdin: false };
    if !cfg!(windows) {
        let bash = Some(PathBuf::from("/bin/bash")).filter(|p| p.exists()).or_else(|| on_path("bash"));
        return Ok(arg(bash.unwrap_or_else(|| "sh".into())));
    }
    let installed = ["ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(std::env::var_os)
        .map(|dir| PathBuf::from(dir).join("Git").join("bin").join("bash.exe"))
        .find(|p| p.exists());
    let Some(bash) = installed.or_else(|| on_path("bash.exe")) else {
        bail!("No bash shell found. Install Git for Windows (https://git-scm.com/download/win), or add your bash (Cygwin, MSYS2) to PATH.");
    };
    // `System32\bash.exe` is the WSL launcher.
    let wsl =
        bash.parent().and_then(Path::file_name).is_some_and(|d| d.eq_ignore_ascii_case("System32") || d.eq_ignore_ascii_case("Sysnative"));
    Ok(if wsl { Shell { program: bash, args: vec!["-s"], stdin: true } } else { arg(bash) })
}

#[derive(Debug)]
pub struct Job {
    pub id: String,
    pub command: String,
    pub log: PathBuf,
    pid: Option<u32>,
    pub started: i64,
    exit: watch::Receiver<Option<i32>>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    ended: Option<i64>,
    killed: bool,
    tell: Option<Tell>,
}

impl Job {
    fn spawn(env: &Env, command: &str, cwd: &Path) -> Result<Arc<Job>> {
        let shell = shell()?;
        if !cwd.exists() {
            bail!("Working directory does not exist: {}\nCannot execute bash commands.", cwd.display());
        }
        let id = crate::short_id(6);
        let dir = env.dirs.data.join("jobs").join(&env.thread_id);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let log = dir.join(format!("{id}.log"));
        let out = std::fs::File::create(&log).with_context(|| format!("create {}", log.display()))?;
        let mut cmd = tokio::process::Command::new(&shell.program);
        cmd.args(&shell.args).current_dir(cwd).stdout(Stdio::from(out.try_clone()?)).stderr(Stdio::from(out));
        cmd.envs(env.shell_env.read().unwrap().iter()).env("DIRIGENT_THREAD_ID", &env.thread_id);
        if shell.stdin {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.arg(command).stdin(Stdio::null());
        }
        #[cfg(unix)]
        cmd.process_group(0);
        // No console window for it, or for the console programs it starts.
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let mut child = cmd.spawn().with_context(|| format!("start {}", shell.program.display()))?;
        if let Some(mut stdin) = child.stdin.take() {
            let command = command.to_owned();
            tokio::spawn(async move {
                let _ = tokio::io::AsyncWriteExt::write_all(&mut stdin, command.as_bytes()).await;
            });
        }
        let (ended, exit) = watch::channel(None);
        let job =
            Arc::new(Job { id, command: command.to_owned(), log, pid: child.id(), started: crate::now(), exit, state: Mutex::default() });
        let this = job.clone();
        tokio::spawn(async move {
            let code = match child.wait().await {
                Ok(status) => exit_code(status),
                Err(_) => 127,
            };
            this.state.lock().unwrap().ended = Some(crate::now());
            let _ = ended.send(Some(code));
        });
        Ok(job)
    }

    pub fn exit(&self) -> Option<i32> {
        *self.exit.borrow()
    }

    pub async fn exited(&self) -> i32 {
        let mut exit = self.exit.clone();
        loop {
            if let Some(code) = *exit.borrow_and_update() {
                return code;
            }
            if exit.changed().await.is_err() {
                return 127;
            }
        }
    }

    pub fn killed(&self) -> bool {
        self.state.lock().unwrap().killed
    }

    pub fn background(&self, tell: Tell) {
        self.state.lock().unwrap().tell = Some(tell);
    }

    fn tell(&self) -> Option<Tell> {
        self.state.lock().unwrap().tell
    }

    pub fn duration_ms(&self) -> i64 {
        self.state.lock().unwrap().ended.unwrap_or_else(crate::now) - self.started
    }

    pub fn kill(&self) {
        self.state.lock().unwrap().killed = true;
        self.stop();
    }

    pub fn kill_now(&self) {
        self.state.lock().unwrap().killed = true;
        let Some(pid) = self.pid.filter(|_| self.exit().is_none()) else { return };
        #[cfg(unix)]
        signal(pid, libc::SIGKILL);
        #[cfg(windows)]
        taskkill(pid);
    }

    /// On Windows `taskkill /T /F` ends the whole tree at once: console programs have no gentler
    /// stop to send them.
    pub fn stop(&self) {
        let Some(pid) = self.pid.filter(|_| self.exit().is_none()) else { return };
        #[cfg(unix)]
        {
            signal(pid, libc::SIGTERM);
            // The group has to be gone, not only its shell: one that dies of the signal
            // leaves the children that ignore it. Looked at often, and no more once it is
            // empty: from then on its id can come to be another group's.
            tokio::spawn(async move {
                let grace_ends = tokio::time::Instant::now() + STOP_GRACE;
                while signal(pid, 0) {
                    if tokio::time::Instant::now() >= grace_ends {
                        signal(pid, libc::SIGKILL);
                        break;
                    }
                    tokio::time::sleep(STOP_POLL).await;
                }
            });
        }
        #[cfg(windows)]
        taskkill(pid);
    }

    pub fn describe(&self, with_tail: bool) -> String {
        let ran = duration(self.duration_ms());
        let head = match self.exit() {
            None => format!("job {} running for {ran}: `{}`", self.id, self.command),
            Some(code) => {
                let how = match code {
                    _ if self.killed() => "was killed".to_owned(),
                    0 => "succeeded".to_owned(),
                    code => format!("failed with exit {code}"),
                };
                format!("job {} {how} after {ran}: `{}`", self.id, self.command)
            }
        };
        if !with_tail {
            return head;
        }
        match tail(&self.log) {
            tail if tail.is_empty() => format!("{head}\nlog: {}\n(no output)", self.log.display()),
            tail => format!("{head}\nlog: {}\nlast lines:\n{tail}", self.log.display()),
        }
    }
}

fn tail(log: &Path) -> String {
    let read = || -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(log)?;
        let from = file.metadata()?.len().saturating_sub(TAIL_BYTES);
        file.seek(std::io::SeekFrom::Start(from))?;
        let mut end = vec![];
        file.take(TAIL_BYTES).read_to_end(&mut end)?;
        Ok(end)
    };
    let bytes = read().unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    match tail.len() {
        len if len > TAIL_CHARS => format!("…{}", &tail[tail.ceil_char_boundary(len - TAIL_CHARS)..]),
        _ => tail,
    }
}

#[cfg(unix)]
fn signal(pid: u32, sig: libc::c_int) -> bool {
    // SAFETY: `killpg` only sends a signal; a group that is gone is an error it returns.
    unsafe { libc::killpg(pid as libc::pid_t, sig) == 0 }
}

#[cfg(windows)]
fn taskkill(pid: u32) {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let taskkill = PathBuf::from(root).join("System32").join("taskkill.exe");
    let mut cmd = std::process::Command::new(taskkill);
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let _ = cmd.args(["/T", "/F", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

/// A shell killed by a signal has no exit code: 128 + the signal, as shells say it.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    if let Some(signal) = std::os::unix::process::ExitStatusExt::signal(&status) {
        return 128 + signal;
    }
    status.code().unwrap_or(1)
}

pub fn duration(ms: i64) -> String {
    let s = (ms as f64 / 1000.).round() as i64;
    if s < 60 { format!("{s}s") } else { format!("{}m {:02}s", s / 60, s % 60) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tell {
    Asap,
    WhenIdle,
    Never,
}

#[derive(Default)]
pub struct Jobs {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    // MUST KILL: type: move `closed` inside the `jobs` Mutex so it can only be read and set under that lock
    closed: AtomicBool,
}

impl Jobs {
    pub fn spawn(&self, env: &Env, command: &str, cwd: &Path) -> Result<Arc<Job>> {
        let job = Job::spawn(env, command, cwd)?;
        let mut jobs = self.jobs.lock().unwrap();
        if self.closed.load(Ordering::Relaxed) {
            job.kill_now();
            bail!("the thread was unloaded");
        }
        jobs.insert(job.id.clone(), job.clone());
        Ok(job)
    }

    pub fn forget(&self, job: &Job) {
        self.jobs.lock().unwrap().remove(&job.id);
    }

    fn in_background(&self) -> Vec<(Arc<Job>, Tell)> {
        self.jobs.lock().unwrap().values().filter_map(|job| Some((job.clone(), job.tell()?))).collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Job>> {
        self.jobs.lock().unwrap().get(id).filter(|job| job.tell().is_some()).cloned()
    }

    pub fn all(&self) -> Vec<Arc<Job>> {
        let mut jobs: Vec<Arc<Job>> = self.in_background().into_iter().map(|(job, _)| job).collect();
        jobs.sort_by_key(|j| j.started);
        jobs
    }

    pub fn awaited(&self) -> Vec<Arc<Job>> {
        let running = |(job, tell): (Arc<Job>, Tell)| (job.exit().is_none() && !job.killed() && tell != Tell::Never).then_some(job);
        let mut awaited: Vec<Arc<Job>> = self.in_background().into_iter().filter_map(running).collect();
        awaited.sort_by_key(|j| j.started);
        awaited
    }

    pub fn running(&self) -> (u32, u32) {
        let jobs = self.in_background();
        let live = jobs.iter().filter(|(job, _)| job.exit().is_none() && !job.killed());
        live.fold((0, 0), |(awaited, free), (_, tell)| if *tell == Tell::Never { (awaited, free + 1) } else { (awaited + 1, free) })
    }

    pub fn kill(&self, awaited_only: bool) {
        for job in self.jobs.lock().unwrap().values() {
            if job.exit().is_none() && !(awaited_only && job.tell() == Some(Tell::Never)) {
                job.kill();
            }
        }
    }

    pub fn kill_now(&self) {
        let jobs = self.jobs.lock().unwrap();
        self.closed.store(true, Ordering::Relaxed);
        jobs.values().for_each(|job| job.kill_now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_log_is_described_by_its_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("job.log");
        let lines: Vec<String> = (1..=20_000).map(|n| format!("line {n}")).collect();
        std::fs::write(&log, lines.join("\n") + "\n").unwrap();
        assert_eq!(tail(&log), lines[lines.len() - TAIL_LINES..].join("\n"));
        std::fs::write(&log, "x".repeat(3 * TAIL_BYTES as usize) + "y\n").unwrap();
        assert_eq!(tail(&log), format!("…{}y", "x".repeat(TAIL_CHARS - 1)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_stopped_job_takes_what_ignores_the_signal_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let jobs = Jobs::default();
        let command = "(trap '' TERM; for i in $(seq 100); do echo $i; sleep 0.1; done) & wait";
        let job = jobs.spawn(&Env::test(dir.path()), command, dir.path()).unwrap();
        let written = || std::fs::metadata(&job.log).unwrap().len();
        while written() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        job.stop();
        assert_eq!(job.exited().await, 128 + libc::SIGTERM);
        tokio::time::sleep(STOP_GRACE + std::time::Duration::from_millis(300)).await;
        let at_the_kill = written();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(written(), at_the_kill, "the child still writes");
    }
}
