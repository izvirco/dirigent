use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, info, warn};

#[derive(Debug, Serialize, Deserialize)]
pub struct Launch {
    data_dir: PathBuf,
    pub open: Option<String>,
}

fn port_file() -> PathBuf {
    dirigent_service::data_dir().join("desktop.port")
}

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
