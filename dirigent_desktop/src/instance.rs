//! One desktop per data dir. A second launch (Explorer's "Open in Dirigent", a shortcut
//! clicked while Dirigent runs) hands its folder to the running desktop, which comes to
//! the front, and exits.
//!
//! The running desktop listens on a loopback port, written to `<data dir>/desktop.port`. A
//! launch connects there and sends one JSON line, `{"data_dir": …, "open": …}`; the desktop
//! answers `ok` if it runs on that data dir. Anything else (no file, nothing listening, a
//! port reused by another program or another data dir's desktop) and the launch becomes
//! the running desktop itself. Keyed by data dir, so test instances (`scripts/ui.sh`)
//! never hand off to the real one.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, info, warn};

/// What a launch asks of the running desktop: come to the front, and open `open` (a folder)
/// if given.
#[derive(Debug, Serialize, Deserialize)]
pub struct Launch {
    data_dir: PathBuf,
    pub open: Option<String>,
}

fn port_file() -> PathBuf {
    dirigent_service::data_dir().join("desktop.port")
}

/// Hand `open` to the desktop already running on this data dir. `true`: it took it, so this
/// launch is done.
pub fn hand_off(open: Option<&str>) -> bool {
    let Some(port) = std::fs::read_to_string(port_file()).ok().and_then(|p| p.trim().parse::<u16>().ok()) else {
        return false;
    };
    let launch = Launch { data_dir: dirigent_service::data_dir(), open: open.map(str::to_owned) };
    let ask = || -> std::io::Result<bool> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        writeln!(stream, "{}", serde_json::to_string(&launch)?)?;
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply)?;
        Ok(reply.trim() == "ok")
    };
    match ask() {
        Ok(took) => took,
        Err(e) => {
            debug!("no running desktop on :{port}: {e}");
            false
        }
    }
}

/// Become the running desktop: later launches arrive on the returned channel.
pub fn listen() -> Option<futures::channel::mpsc::UnboundedReceiver<Launch>> {
    let listener = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(l) => l,
        Err(e) => {
            warn!("can't listen for other launches: {e}");
            return None;
        }
    };
    let port = listener.local_addr().ok()?.port();
    if let Err(e) = std::fs::write(port_file(), port.to_string()) {
        warn!("write {}: {e}", port_file().display());
        return None;
    }
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let data_dir = dirigent_service::data_dir();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut line = String::new();
            let mut reader = BufReader::new(&stream);
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let launch = serde_json::from_str::<Launch>(&line).ok().filter(|l| l.data_dir == data_dir);
            let _ = (&stream).write_all(if launch.is_some() { b"ok\n" } else { b"no\n" });
            if let Some(launch) = launch {
                info!(open = ?launch.open, "another launch handed over");
                if tx.unbounded_send(launch).is_err() {
                    return;
                }
            }
        }
    });
    Some(rx)
}
