//! One WebSocket connection per host, on a dedicated tokio thread. Incoming
//! messages update the cache and are forwarded to the UI through a channel;
//! outgoing requests come from the UI through a per-host channel. Hosts can be
//! added and removed while running (`hosts.toml` edits). Each (re)connect resolves its host
//! afresh (`Host::endpoint`): an `ssh://` host through its tunnel, which a dead ssh restarts,
//! and with its bearer token, if any.

use crate::cache::Cache;
use crate::hosts::{Host, LOCAL};
use anyhow::Result;
use dirigent_protocol::{ClientMessage, ServerMessage};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{error, info, warn};

/// Request id used by the connection task itself for the refresh requests on (re)connect.
pub const CONNECT_LIST_ID: u64 = 0;

/// Largest message (and frame: the host sends each message as one) we read; the host
/// reads the same (`dirigent_service` `server.rs`).
const MAX_MESSAGE: usize = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStatus {
    Disconnected,
    Connecting,
    Connected,
}

pub enum Incoming {
    /// With why the last connect failed (ssh's stderr, a 401), until one succeeds.
    Status {
        host: String,
        status: HostStatus,
        error: Option<String>,
    },
    Message {
        host: String,
        msg: Box<ServerMessage>,
    },
}

pub type UiSender = futures::channel::mpsc::UnboundedSender<Incoming>;

pub struct Net {
    hosts: HashMap<String, mpsc::UnboundedSender<ClientMessage>>,
    control: mpsc::UnboundedSender<Control>,
}

/// To the net thread: start or stop a host's connection task.
enum Control {
    Add { name: String, host: Host, rx: mpsc::UnboundedReceiver<ClientMessage> },
    Remove(String),
}

impl Net {
    /// Spawn the tokio thread and one connection task per `(name, host)`.
    /// The `local` host gets an in-process service started for it if nothing is listening.
    pub fn start(hosts: Vec<(String, Host)>, cache: Arc<Mutex<Cache>>, ui: UiSender) -> Self {
        let (control, mut control_rx) = mpsc::unbounded_channel::<Control>();
        std::thread::Builder::new()
            .name("net".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(async move {
                    let mut tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
                    while let Some(cmd) = control_rx.recv().await {
                        match cmd {
                            Control::Add { name, host, rx } => {
                                let task = tokio::spawn(host_loop(name.clone(), host, rx, cache.clone(), ui.clone()));
                                tasks.insert(name, task);
                            }
                            Control::Remove(name) => {
                                if let Some(task) = tasks.remove(&name) {
                                    task.abort();
                                }
                            }
                        }
                    }
                });
            })
            .expect("spawn net thread");
        let mut net = Self { hosts: HashMap::new(), control };
        for (name, host) in hosts {
            net.add(name, host);
        }
        net
    }

    pub fn add(&mut self, name: String, host: Host) {
        info!(host = %name, url = %host.url, "host added");
        let (tx, rx) = mpsc::unbounded_channel();
        self.hosts.insert(name.clone(), tx);
        let _ = self.control.send(Control::Add { name, host, rx });
    }

    /// Drops the connection; the UI gets no final status for it.
    pub fn remove(&mut self, name: &str) {
        info!(host = %name, "host removed");
        self.hosts.remove(name);
        let _ = self.control.send(Control::Remove(name.to_owned()));
    }

    pub fn send(&self, host: &str, msg: ClientMessage) {
        if let Some(tx) = self.hosts.get(host) {
            let _ = tx.send(msg);
        }
    }
}

/// Connect to `host` as it resolves now, with its bearer token if it has one.
async fn connect(host: &Host) -> Result<WebSocket> {
    let endpoint = host.endpoint().await?;
    let mut request = endpoint.url.as_str().into_client_request()?;
    if let Some(bearer) = &endpoint.bearer {
        request.headers_mut().insert(AUTHORIZATION, format!("Bearer {bearer}").parse()?);
    }
    let config = WebSocketConfig::default().max_frame_size(Some(MAX_MESSAGE)).max_message_size(Some(MAX_MESSAGE));
    Ok(tokio_tungstenite::connect_async_with_config(request, Some(config), false).await?.0)
}

type WebSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn host_loop(name: String, host: Host, mut rx: mpsc::UnboundedReceiver<ClientMessage>, cache: Arc<Mutex<Cache>>, ui: UiSender) {
    let url = &host.url;
    let status = |s: HostStatus, error: Option<String>| {
        let _ = ui.unbounded_send(Incoming::Status { host: name.clone(), status: s, error });
    };
    let mut backoff = Duration::from_millis(300);
    let mut embedded: Option<tokio::task::JoinHandle<()>> = None;
    // A host that stays unreachable is logged once, not on every retry.
    let mut failing = false;
    loop {
        status(HostStatus::Connecting, None);
        match connect(&host).await {
            Ok(ws) => {
                backoff = Duration::from_millis(300);
                failing = false;
                status(HostStatus::Connected, None);
                let (mut sink, mut stream) = ws.split();
                // Refresh everything we cache for this host.
                for req in [
                    ClientMessage::ListProjects { id: CONNECT_LIST_ID },
                    ClientMessage::ListThreads { id: CONNECT_LIST_ID },
                    ClientMessage::ListModels { id: CONNECT_LIST_ID },
                ] {
                    let _ = sink.send(Message::Text(serde_json::to_string(&req).unwrap().into())).await;
                }
                loop {
                    tokio::select! {
                        Some(req) = rx.recv() => {
                            let text = serde_json::to_string(&req).unwrap();
                            if let Err(e) = sink.send(Message::Text(text.into())).await {
                                warn!(host = %name, "send: {e}");
                                break;
                            }
                        }
                        item = stream.next() => match item {
                            Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMessage>(&text) {
                                Ok(msg) => {
                                    // The service's first message; its version explains a remote host lagging behind.
                                    if let ServerMessage::Hello { host_name, version, .. } = &msg {
                                        info!(host = %name, machine = %host_name, %version, "connected");
                                    }
                                    if let Err(e) = update_cache(&cache, &name, &msg) {
                                        error!(host = %name, "cache: {e:#}");
                                    }
                                    let _ = ui.unbounded_send(Incoming::Message { host: name.clone(), msg: Box::new(msg) });
                                }
                                Err(e) => warn!(host = %name, "bad message ({e}): {}", &text[..text.floor_char_boundary(200)]),
                            },
                            Some(Ok(_)) => {}
                            Some(Err(e)) => {
                                warn!(host = %name, "read: {e}");
                                break;
                            }
                            None => break,
                        }
                    }
                }
                // What was sent meanwhile is dropped too, not sent on the reconnect: the UI
                // gives back what it had pending once it sees `Disconnected`.
                while rx.try_recv().is_ok() {}
                info!(host = %name, "disconnected");
                status(HostStatus::Disconnected, None);
            }
            Err(e) => {
                // `local` refusing before we ever started the service is the normal start-up,
                // not worth a line in the log or an error in Settings.
                let starting = name == LOCAL && embedded.is_none();
                if !starting && !std::mem::replace(&mut failing, true) {
                    warn!(host = %name, "connect {url}: {e:#} (retrying)");
                }
                // Nothing listening locally: run the service in-process. It lives and dies
                // with the desktop. If it exits (e.g. bind failed), the next refusal retries,
                // quietly: a service that keeps failing is logged once, like the connect.
                if name == LOCAL && embedded.as_ref().is_none_or(|h| h.is_finished()) {
                    let quiet = failing;
                    if !quiet {
                        info!("starting embedded service on :{}", dirigent_service::port());
                    }
                    embedded = Some(tokio::spawn(async move {
                        if let Err(e) = dirigent_service::run(dirigent_service::port()).await
                            && !quiet
                        {
                            error!("embedded service stopped: {e:#}");
                        }
                    }));
                }
                // Requests made while offline are dropped, not queued for a later reconnect.
                while rx.try_recv().is_ok() {}
                status(HostStatus::Disconnected, (!starting).then(|| format!("{e:#}")));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }
    }
}

/// Mirror host data into the cache before the UI sees it.
fn update_cache(cache: &Mutex<Cache>, host: &str, msg: &ServerMessage) -> Result<()> {
    let mut cache = cache.lock().unwrap();
    match msg {
        ServerMessage::Projects { projects, .. } => cache.set_projects(host, projects),
        ServerMessage::Models { models, .. } => cache.set_models(host, models),
        ServerMessage::Threads { threads, .. } => cache.set_threads(host, threads),
        ServerMessage::Thread { thread_id, entries, .. } => cache.set_entries(host, thread_id, entries),
        ServerMessage::Entry { thread_id, entry } => cache.add_entry(host, thread_id, entry),
        ServerMessage::Blob { blob_id, mime_type, data, .. } => {
            let bytes = base64::Engine::decode(&base64::prelude::BASE64_STANDARD, data)?;
            cache.put_blob(blob_id, mime_type, &bytes)
        }
        _ => Ok(()),
    }
}
