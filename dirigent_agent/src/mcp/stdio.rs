//! MCP over a command's stdio: a JSON-RPC message per line on its stdin and stdout, its
//! stderr kept for when it fails. The command runs in a process group of its own, which
//! goes with the connection: wrappers (`npx`, `uvx`) don't leave their server behind.
//! Unless what the command started is to stay (`Added::leave_started`: the apps on the
//! user's desktop): then the command alone is ended.

use super::client::{Error, MAX_MESSAGE_BYTES, Transport};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio as Pipe;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How much of the command's stderr is kept, and how much of that a failure's message has.
const STDERR_KEPT: usize = 64 * 1024;
const STDERR_SAID: usize = 2000;
/// How long the command gets to exit once its stdin closed, and then once it was told to.
const EXIT_GRACE: Duration = Duration::from_millis(500);
const KILL_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct Stdio {
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    /// There until `close` has seen it exit.
    child: Mutex<Option<Child>>,
    /// The command's process id, which is its process group's.
    pid: Option<u32>,
    /// The whole group is ended with the command, not the command alone.
    group: bool,
    stderr: Arc<Mutex<Vec<u8>>>,
    /// Cancelled once the command's stderr is read to its end.
    stderr_read: CancellationToken,
}

impl Stdio {
    /// Run `command` in `cwd` with `env` added to this process's environment. What it
    /// prints comes through the channel, which closes when the command's stdout does.
    /// `group`: what the command starts is ended with it.
    pub(crate) fn spawn(
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
        group: bool,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), Error> {
        let run = |program: &str| {
            let mut run = Command::new(program);
            run.args(args).envs(env).current_dir(cwd).stdin(Pipe::piped()).stdout(Pipe::piped()).stderr(Pipe::piped()).kill_on_drop(true);
            #[cfg(unix)]
            run.process_group(0);
            // No console window flashing up from a GUI app.
            #[cfg(windows)]
            run.creation_flags(0x0800_0000);
            run.spawn()
        };
        let spawned = run(command);
        // `npx` is `npx.cmd` there, which is only found by its whole name.
        #[cfg(windows)]
        let spawned =
            spawned.or_else(|e| [".cmd", ".bat"].iter().find_map(|extension| run(&format!("{command}{extension}")).ok()).ok_or(e));
        let mut child = spawned.map_err(|e| {
            let hint = match e.kind() {
                std::io::ErrorKind::NotFound if !cwd.is_dir() => format!(" (its directory, {}, isn't there)", cwd.display()),
                std::io::ErrorKind::NotFound => " (is it installed, and on the host's PATH?)".to_owned(),
                _ => String::new(),
            };
            Error::Other(format!("Couldn't run `{command}`: {e}{hint}"))
        })?;
        let (stdin, stdout, errors) = (child.stdin.take(), child.stdout.take().expect("piped"), child.stderr.take().expect("piped"));

        let (stderr, stderr_read) = (Arc::new(Mutex::new(Vec::new())), CancellationToken::new());
        let (kept, read) = (stderr.clone(), stderr_read.clone());
        tokio::spawn(async move {
            let (mut errors, mut chunk) = (errors, [0; 4096]);
            while let Ok(n @ 1..) = errors.read(&mut chunk).await {
                let mut kept = kept.lock().unwrap();
                kept.extend_from_slice(&chunk[..n]);
                let over = kept.len().saturating_sub(STDERR_KEPT);
                kept.drain(..over);
            }
            read.cancel();
        });
        let read = stderr_read.clone();
        let (messages, incoming) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            while let Some(line) = read_line(&mut stdout).await {
                // Anything else on stdout (a banner, a log line) is not ours to mind.
                match serde_json::from_slice::<Value>(&line) {
                    Ok(message) if message.is_object() => drop(messages.send(message)),
                    _ => tracing::debug!("mcp: not a message: {}", String::from_utf8_lossy(&line[..line.len().min(200)])),
                }
            }
            // The command is gone or going: what it said of that is all there before the
            // connection counts as closed and its failure is put into words.
            let _ = tokio::time::timeout(EXIT_GRACE, read.cancelled()).await;
        });
        let pid = child.id();
        Ok((Self { stdin: tokio::sync::Mutex::new(stdin), child: Mutex::new(Some(child)), pid, group, stderr, stderr_read }, incoming))
    }
}

/// The next line that has something on it; `None` at the end. One longer than a message
/// may be is skipped.
async fn read_line(from: &mut (impl AsyncBufRead + Unpin)) -> Option<Vec<u8>> {
    loop {
        let mut line = Vec::new();
        let read = from.take(MAX_MESSAGE_BYTES as u64 + 1).read_until(b'\n', &mut line).await;
        if !matches!(read, Ok(1..)) {
            return None;
        }
        if line.len() > MAX_MESSAGE_BYTES && !line.ends_with(b"\n") {
            tracing::warn!("mcp: a line of over {MAX_MESSAGE_BYTES} bytes skipped");
            let mut rest = Vec::new();
            while matches!(from.take(64 * 1024).read_until(b'\n', &mut rest).await, Ok(1..)) && !rest.ends_with(b"\n") {
                rest.clear();
            }
            continue;
        }
        if !line.trim_ascii().is_empty() {
            return Some(line);
        }
    }
}

#[async_trait]
impl Transport for Stdio {
    async fn send(&self, message: &Value) -> Result<(), Error> {
        let mut line = serde_json::to_vec(message).expect("a JSON value");
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        let stdin = stdin.as_mut().ok_or(Error::Closed)?;
        let written = async {
            stdin.write_all(&line).await?;
            stdin.flush().await
        };
        written.await.map_err(|_| Error::Closed)
    }

    fn last_words(&self) -> Option<String> {
        let stderr = self.stderr.lock().unwrap();
        let said = String::from_utf8_lossy(&stderr);
        let said = said.trim();
        let from = said.char_indices().rev().nth(STDERR_SAID - 1).map_or(0, |(at, _)| at);
        (!said.is_empty()).then(|| said[from..].to_owned())
    }

    /// As the spec has it: stdin closes and the command exits; one that doesn't is told
    /// to, then killed. Its process group gets the same, for what the command started
    /// (unless that is to stay: `group`).
    async fn close(&self) {
        // Unless a message is stuck on its way in: then the command isn't reading, and is told.
        if let Ok(mut stdin) = self.stdin.try_lock() {
            stdin.take();
        }
        let Some(mut child) = self.child.lock().unwrap().take() else { return };
        // Named after the child, so let go of before it: its pid is its own until then.
        let mut closing = Closing { pid: self.pid, group: self.group };
        if tokio::time::timeout(EXIT_GRACE, child.wait()).await.is_err() {
            signal(self.pid, false, self.group);
            if tokio::time::timeout(KILL_GRACE, child.wait()).await.is_err() {
                signal(self.pid, true, self.group);
                let _ = child.wait().await;
            }
        }
        // What the command started and left behind.
        if self.group {
            signal(self.pid, false, true);
        }
        closing.pid = None;
        // Its last words are all there for whoever asks why it went.
        let _ = tokio::time::timeout(EXIT_GRACE, self.stderr_read.cancelled()).await;
    }
}

/// A `close` under way, which has the child: let go of before the command and what it
/// started were ended (whoever closed the connection was dropped), they are ended as a
/// connection let go without `close` ends them.
struct Closing {
    /// `None` once they were.
    pid: Option<u32>,
    group: bool,
}

impl Drop for Closing {
    fn drop(&mut self) {
        signal(self.pid, true, self.group);
    }
}

impl Drop for Stdio {
    /// A connection let go without `close`: the command isn't asked.
    fn drop(&mut self) {
        // Before the child is let go: its pid is its own until then.
        if self.child.lock().unwrap().is_some() {
            signal(self.pid, true, self.group);
        }
    }
}

/// Tell the command with `pid` to end (`kill`: end it), and with it its process group
/// (`group`), which has what it started.
#[cfg(unix)]
fn signal(pid: Option<u32>, kill: bool, group: bool) {
    let Some(pid) = pid else { return };
    let (pid, signal) = (pid as libc::pid_t, if kill { libc::SIGKILL } else { libc::SIGTERM });
    // SAFETY: both only send a signal; a process or a group that is gone is an error they return.
    unsafe { if group { libc::killpg(pid, signal) } else { libc::kill(pid, signal) } };
}

/// Windows has no signals to ask with: the command is ended, and with it (`group`) its
/// process tree.
#[cfg(windows)]
fn signal(pid: Option<u32>, _kill: bool, group: bool) {
    let Some(pid) = pid else { return };
    let mut kill = std::process::Command::new("taskkill");
    std::os::windows::process::CommandExt::creation_flags(&mut kill, 0x0800_0000);
    let tree = group.then_some("/T");
    let _ = kill.args(["/pid", &pid.to_string(), "/F"]).args(tree).stdin(Pipe::null()).stdout(Pipe::null()).stderr(Pipe::null()).spawn();
}
