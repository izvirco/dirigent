//! One WebSocket connection per host, on a dedicated tokio thread. Incoming
//! messages update the cache and are forwarded to the UI through a channel;
//! outgoing requests come from the UI through a per-host channel. Hosts can be
//! added and removed while running (`hosts.toml` edits).

use crate::cache::Cache;
use crate::hosts::LOCAL;
use anyhow::Result;
use dirigent_protocol::{ClientMessage, ServerMessage};
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Request id used by the connection task itself for the refresh requests on (re)connect.
pub const CONNECT_LIST_ID: u64 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStatus {
    Disconnected,
    Connecting,
    Connected,
}

pub enum Incoming {
    Status { host: String, status: HostStatus },
    Message { host: String, msg: Box<ServerMessage> },
}

pub type UiSender = futures::channel::mpsc::UnboundedSender<Incoming>;

pub struct Net {
    hosts: HashMap<String, mpsc::UnboundedSender<ClientMessage>>,
    control: mpsc::UnboundedSender<Control>,
}

/// To the net thread: start or stop a host's connection task.
enum Control {
    Add { name: String, url: String, rx: mpsc::UnboundedReceiver<ClientMessage> },
    Remove(String),
}

impl Net {
    /// Spawn the tokio thread and one connection task per `(name, url)`.
    /// The `local` host gets an in-process service started for it if nothing is listening.
    pub fn start(hosts: Vec<(String, String)>, cache: Arc<Mutex<Cache>>, ui: UiSender) -> Self {
        let (control, mut control_rx) = mpsc::unbounded_channel::<Control>();
        std::thread::Builder::new()
            .name("net".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
                rt.block_on(async move {
                    let mut tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
                    while let Some(cmd) = control_rx.recv().await {
                        match cmd {
                            Control::Add { name, url, rx } => {
                                let task = tokio::spawn(host_loop(name.clone(), url, rx, cache.clone(), ui.clone()));
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
        for (name, url) in hosts {
            net.add(name, url);
        }
        net
    }

    pub fn add(&mut self, name: String, url: String) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.hosts.insert(name.clone(), tx);
        let _ = self.control.send(Control::Add { name, url, rx });
    }

    /// Drops the connection; the UI gets no final status for it.
    pub fn remove(&mut self, name: &str) {
        self.hosts.remove(name);
        let _ = self.control.send(Control::Remove(name.to_owned()));
    }

    pub fn send(&self, host: &str, msg: ClientMessage) {
        if let Some(tx) = self.hosts.get(host) {
            let _ = tx.send(msg);
        }
    }
}

async fn host_loop(name: String, url: String, mut rx: mpsc::UnboundedReceiver<ClientMessage>, cache: Arc<Mutex<Cache>>, ui: UiSender) {
    let status = |s: HostStatus| {
        let _ = ui.unbounded_send(Incoming::Status { host: name.clone(), status: s });
    };
    let mut backoff = Duration::from_millis(300);
    let mut embedded: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        status(HostStatus::Connecting);
        match tokio_tungstenite::connect_async(&url).await {
            Ok((ws, _)) => {
                backoff = Duration::from_millis(300);
                status(HostStatus::Connected);
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
                            if sink.send(Message::Text(text.into())).await.is_err() { break }
                        }
                        item = stream.next() => match item {
                            Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMessage>(&text) {
                                Ok(msg) => {
                                    if let Err(e) = update_cache(&cache, &name, &msg) {
                                        eprintln!("[net] cache: {e:#}");
                                    }
                                    let _ = ui.unbounded_send(Incoming::Message { host: name.clone(), msg: Box::new(msg) });
                                }
                                Err(e) => eprintln!("[net] {name}: bad message ({e}): {}", &text[..text.len().min(200)]),
                            },
                            Some(Ok(_)) => {}
                            _ => break,
                        }
                    }
                }
                status(HostStatus::Disconnected);
            }
            Err(e) => {
                // `local` refusing before we ever started the service is the normal start-up,
                // not worth a line in the log.
                if !(name == LOCAL && embedded.is_none()) {
                    eprintln!("[net] {name}: connect {url}: {e}");
                }
                // Nothing listening locally: run the service in-process. It lives and dies
                // with the desktop. If it exits (e.g. bind failed), the next refusal retries.
                if name == LOCAL && embedded.as_ref().is_none_or(|h| h.is_finished()) {
                    eprintln!("[net] starting embedded service on :{}", dirigent_service::port());
                    embedded = Some(tokio::spawn(async {
                        if let Err(e) = dirigent_service::run(dirigent_service::port()).await {
                            eprintln!("[net] embedded service stopped: {e:#}");
                        }
                    }));
                }
                // Requests made while offline are dropped, not queued for a later reconnect.
                while rx.try_recv().is_ok() {}
                status(HostStatus::Disconnected);
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
