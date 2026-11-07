//! SSH tunnels to `ssh://` hosts (`hosts.toml`): the system `ssh` forwards a free local port
//! to the remote service's loopback listener, and we connect to `ws://127.0.0.1:<port>/ws`.
//! ssh is the auth: the remote service sees a loopback client and asks nothing more.
//!
//! Process-global, one ssh per host url: the desktop's connections (`net.rs`) and its
//! embedded service's `ListHosts` (for agents) share them; a remote `dirigent_service` has
//! its own, for its own `hosts.toml`. A tunnel starts on the first `open` and runs until
//! ssh exits (then the next `open` starts another), its host leaves `hosts.toml` (`retain`)
//! or the process ends (`close_all`). Only on Linux does the kernel enforce the last one
//! (`PR_SET_PDEATHSIG`); elsewhere a parent that crashes can leave its `ssh -N` behind.
//! `BatchMode`: no password or host key prompts; a host has to work with `ssh <host>`
//! non-interactively first (keys, `known_hosts`).

use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeMap;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Where an `ssh://[user@]host[:ssh port][/service port]` url points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ssh {
    user: Option<String>,
    /// A name, an ssh_config alias or an address (IPv6 without brackets).
    host: String,
    /// The ssh port; none = ssh's own default (or the alias's `Port`).
    port: Option<u16>,
    /// The remote service's port.
    service_port: u16,
}

impl Ssh {
    pub fn parse(url: &str) -> Result<Self> {
        let rest = url.trim().strip_prefix("ssh://").context("not an ssh:// url")?.trim_end_matches('/');
        let (authority, service_port) = match rest.split_once('/') {
            Some((a, p)) => (a, p.parse().with_context(|| format!("bad service port {p:?} in {url}"))?),
            None => (rest, crate::DEFAULT_PORT),
        };
        let (user, host_port) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u.to_owned()), h),
            None => (None, authority),
        };
        let (host, port) = if let Some(v6) = host_port.strip_prefix('[') {
            let (host, after) = v6.split_once(']').context("unclosed [ in host")?;
            (host, after.strip_prefix(':'))
        } else {
            match host_port.split_once(':') {
                // One colon: host:port. More is a bare IPv6 address.
                Some((h, p)) if !p.contains(':') => (h, Some(p)),
                _ => (host_port, None),
            }
        };
        let port = port.map(|p| p.parse().with_context(|| format!("bad ssh port {p:?} in {url}"))).transpose()?;
        // A leading `-` would be an ssh option.
        let ok = |s: &str| !s.is_empty() && !s.starts_with('-') && !s.contains(char::is_whitespace);
        ensure!(ok(host), "bad host in {url}");
        ensure!(user.as_deref().is_none_or(ok), "bad user in {url}");
        Ok(Self { user, host: host.to_owned(), port, service_port })
    }

    /// `ssh`'s arguments for a tunnel from `127.0.0.1:<local>`.
    fn args(&self, local: u16) -> Vec<String> {
        let mut args = vec!["-N".to_owned(), "-L".to_owned(), format!("127.0.0.1:{local}:127.0.0.1:{}", self.service_port)];
        for option in ["ExitOnForwardFailure=yes", "BatchMode=yes", "ServerAliveInterval=15", "ServerAliveCountMax=3", "ConnectTimeout=15"]
        {
            args.extend(["-o".to_owned(), option.to_owned()]);
        }
        if let Some(port) = self.port {
            args.extend(["-p".to_owned(), port.to_string()]);
        }
        args.push(match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        });
        args
    }
}

impl std::fmt::Display for Ssh {
    /// Canonical: the service port only when it isn't the default.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ssh://")?;
        if let Some(user) = &self.user {
            write!(f, "{user}@")?;
        }
        if self.host.contains(':') {
            write!(f, "[{}]", self.host)?
        } else {
            f.write_str(&self.host)?
        }
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        if self.service_port != crate::DEFAULT_PORT {
            write!(f, "/{}", self.service_port)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
enum State {
    Starting,
    /// Forwarding from this local port.
    Up(u16),
    /// ssh exited (or was stopped): why.
    Exited(String),
}

struct Tunnel {
    state: watch::Receiver<State>,
    ssh: Arc<Child>,
}

/// A tunnel's ssh, shared by its thread (which waits for it) and whoever stops it.
#[derive(Default)]
struct Child {
    process: Mutex<Option<std::process::Child>>,
    stopped: AtomicBool,
}

impl Tunnel {
    /// Kill its ssh now, not on the thread's next look: the process may be about to exit.
    /// Running or starting: not exited, and its thread still there to say when it does
    /// (a thread that panicked never would).
    fn alive(&self) -> bool {
        !matches!(*self.state.borrow(), State::Exited(_)) && self.state.has_changed().is_ok()
    }

    fn stop(&self) {
        self.ssh.stopped.store(true, Ordering::Relaxed);
        if let Some(p) = self.ssh.process.lock().unwrap().as_mut() {
            let _ = p.kill();
        }
    }
}

/// By normalized `ssh://` url.
static TUNNELS: Mutex<BTreeMap<String, Tunnel>> = Mutex::new(BTreeMap::new());

/// How long ssh may take to log in and forward before `open` gives up on it: a hung
/// ssh-agent, `ProxyCommand` or DNS would otherwise leave every caller waiting.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// The local `ws://` url that reaches `url`'s service: through its running tunnel, or a
/// new one once it forwards. An ssh that exits first fails this with its stderr; one that
/// isn't forwarding within `START_TIMEOUT` is stopped, and the next `open` starts afresh.
pub async fn open(url: &str) -> Result<String> {
    let (mut state, ssh) = {
        let mut tunnels = TUNNELS.lock().unwrap();
        match tunnels.get(url) {
            Some(t) if t.alive() => (t.state.clone(), t.ssh.clone()),
            _ => {
                let t = start(Ssh::parse(url)?);
                let found = (t.state.clone(), t.ssh.clone());
                tunnels.insert(url.to_owned(), t);
                found
            }
        }
    };
    let state = tokio::time::timeout(START_TIMEOUT, state.wait_for(|s| !matches!(s, State::Starting))).await;
    match state {
        Ok(Ok(s)) => match &*s {
            State::Up(port) => Ok(format!("ws://127.0.0.1:{port}/ws")),
            State::Exited(why) => bail!("{why}"),
            State::Starting => unreachable!(),
        },
        // Its thread died without saying why.
        Ok(Err(_)) => bail!("ssh tunnel gone"),
        Err(_) => {
            let mut tunnels = TUNNELS.lock().unwrap();
            // Unless another caller replaced it meanwhile.
            if tunnels.get(url).is_some_and(|t| Arc::ptr_eq(&t.ssh, &ssh)) {
                tunnels.remove(url).unwrap().stop();
            }
            bail!("ssh: no tunnel in {} s", START_TIMEOUT.as_secs())
        }
    }
}

/// Stop the tunnels to hosts no longer in `hosts.toml`.
pub fn retain<'a>(urls: impl IntoIterator<Item = &'a str>) {
    let keep: Vec<&str> = urls.into_iter().collect();
    TUNNELS.lock().unwrap().retain(|url, t| {
        let keep = keep.contains(&url.as_str());
        if !keep {
            t.stop();
        }
        keep
    });
}

/// Stop every tunnel: the process is quitting.
pub fn close_all() {
    retain([]);
}

/// Run ssh for `ssh` on a thread of its own, which also waits for it: on Linux the kernel
/// kills ssh when the thread that started it ends (`PR_SET_PDEATHSIG`), so it must outlive
/// ssh, and does.
fn start(target: Ssh) -> Tunnel {
    let (tx, rx) = watch::channel(State::Starting);
    let ssh = Arc::new(Child::default());
    let shared = ssh.clone();
    std::thread::Builder::new()
        .name("ssh tunnel".into())
        .spawn(move || {
            let why = run(&target, &tx, &shared).unwrap_or_else(|e| format!("{e:#}"));
            // A start that fails is its caller's error to report (once, not every retry), and
            // a stop isn't news.
            if shared.stopped.load(Ordering::Relaxed) {
                debug!(host = %target, "ssh tunnel stopped");
            } else if matches!(*tx.borrow(), State::Up(_)) {
                warn!(host = %target, "ssh tunnel ended: {why}");
            } else {
                debug!(host = %target, "ssh tunnel failed: {why}");
            }
            let _ = tx.send(State::Exited(why));
        })
        .expect("spawn ssh tunnel thread");
    Tunnel { state: rx, ssh }
}

/// One ssh lifetime; returns why it ended.
fn run(ssh: &Ssh, state: &watch::Sender<State>, child: &Child) -> Result<String> {
    // Free now; `ExitOnForwardFailure` makes ssh fail rather than run without a forward if
    // something takes it meanwhile.
    let local = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?.local_addr()?.port();
    let mut cmd = crate::platform::command("ssh");
    cmd.args(ssh.args(local)).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `prctl` is async-signal-safe, all a `pre_exec` closure may call.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
    }
    let mut process = cmd.spawn().context("run ssh")?;
    // Its stderr, read to the end (a full pipe would block ssh): what we say when it exits.
    let mut stderr = process.stderr.take().unwrap();
    let (text_tx, text_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        let _ = text_tx.send(text);
    });
    {
        let mut slot = child.process.lock().unwrap();
        // Stopped while it started.
        if child.stopped.load(Ordering::Relaxed) {
            let _ = process.kill();
        }
        *slot = Some(process);
    }
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, local));
    let mut up = false;
    let status = loop {
        if let Some(status) = child.process.lock().unwrap().as_mut().unwrap().try_wait()? {
            break status;
        }
        // ssh listens once it has logged in.
        if !up && TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            up = true;
            info!(host = %ssh, port = local, "ssh tunnel up");
            let _ = state.send(State::Up(local));
        }
        // Quick while it logs in; once up only ssh exiting is left to notice.
        std::thread::sleep(Duration::from_millis(if up { 1000 } else { 200 }));
    };
    if child.stopped.load(Ordering::Relaxed) {
        return Ok("stopped".into());
    }
    // Assumes ssh's stderr closes as it exits (a `ControlPersist` master detaches to
    // /dev/null); a child that kept it open only costs the message, not the wait.
    let stderr = text_rx.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    // The last line: ssh's own error, after any warnings.
    Ok(match stderr.lines().map(str::trim).rfind(|l| !l.is_empty()) {
        Some(last) if last.starts_with("ssh:") => last.to_owned(),
        Some(last) => format!("ssh: {last}"),
        None => format!("ssh exited ({status})"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let s = Ssh::parse("ssh://seb@box.example.com:2222/7778").unwrap();
        assert_eq!(s, Ssh { user: Some("seb".into()), host: "box.example.com".into(), port: Some(2222), service_port: 7778 });
        assert_eq!(s.to_string(), "ssh://seb@box.example.com:2222/7778");
        assert_eq!(Ssh::parse("ssh://alias/").unwrap().to_string(), "ssh://alias");
        assert_eq!(Ssh::parse("ssh://[fd7a::1]:22").unwrap().host, "fd7a::1");
        assert_eq!(Ssh::parse("ssh://fd7a::1").unwrap().to_string(), "ssh://[fd7a::1]");
        for bad in ["ssh://", "ssh://-oProxyCommand=x", "ssh://-u@h", "ssh://h:port", "ssh://h/x", "ws://h"] {
            assert!(Ssh::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn builds_ssh_args() {
        let o = |opt: &str| ["-o".to_owned(), opt.to_owned()];
        let opts = [
            o("ExitOnForwardFailure=yes"),
            o("BatchMode=yes"),
            o("ServerAliveInterval=15"),
            o("ServerAliveCountMax=3"),
            o("ConnectTimeout=15"),
        ]
        .concat();
        let forward = ["-N", "-L", "127.0.0.1:5000:127.0.0.1:7777"].map(String::from);
        assert_eq!(Ssh::parse("ssh://alias").unwrap().args(5000), [&forward[..], &opts, &["alias".to_owned()]].concat());
        let args = Ssh::parse("ssh://seb@h:2222/7778").unwrap().args(5000);
        assert_eq!(args[2], "127.0.0.1:5000:127.0.0.1:7778");
        assert_eq!(args[args.len() - 3..], ["-p", "2222", "seb@h"]);
    }
}
