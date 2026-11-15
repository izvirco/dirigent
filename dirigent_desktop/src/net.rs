//! One WebSocket connection per host, on a dedicated tokio thread. Incoming
//! messages update the cache and are forwarded to the UI through a channel;
//! outgoing requests come from the UI through a per-host channel. Hosts can be
//! added and removed while running (`hosts.toml` edits). Each (re)connect resolves its host
//! afresh (`Host::endpoint`): an `ssh://` host through its tunnel, which a dead ssh restarts,
//! and with its bearer token, if any. A host counts as connected, and is asked anything, once
//! its `Hello` is read and says our protocol.

use crate::cache::Cache;
use crate::hosts::{Host, LOCAL};
use anyhow::Result;
use dirigent_protocol::{ClientMessage, ServerMessage};
use futures::future::BoxFuture;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{error, info, warn};

/// Request id used by the connection task itself for the refresh requests on (re)connect.
pub const CONNECT_LIST_ID: u64 = 0;

/// How long a host that can't be talked to (another protocol, silence) is left alone before it
/// is looked at again, and a local service that stopped before it is started again.
const UNFIT_RETRY: Duration = Duration::from_secs(60);

/// How long a host that took the connection gets to say its first word.
const HELLO_WAIT: Duration = Duration::from_secs(20);

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

/// What runs a host's service in-process until it fails: `embedded_service` for `local`.
type Service = fn() -> BoxFuture<'static, Result<()>>;

fn embedded_service() -> BoxFuture<'static, Result<()>> {
    Box::pin(dirigent_service::run(dirigent_service::port()))
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
                                let service = (name == LOCAL).then_some(embedded_service as Service);
                                let task = tokio::spawn(host_loop(name.clone(), host, service, rx, cache.clone(), ui.clone()));
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
    match tokio_tungstenite::connect_async_with_config(request, Some(config), false).await {
        Ok((ws, _)) => Ok(ws),
        // A refusal says why in its body (a public listener's 401: `auth.rs`).
        Err(tokio_tungstenite::tungstenite::Error::Http(res)) => {
            let body = res.body().as_deref().map(String::from_utf8_lossy).unwrap_or_default();
            let why: String = body.trim().chars().take(300).collect();
            match res.status() {
                // Its body says why, and what to fix.
                StatusCode::UNAUTHORIZED if !why.is_empty() => anyhow::bail!("refused: {why}"),
                status if why.is_empty() => anyhow::bail!("{status}"),
                status => anyhow::bail!("{status}: {why}"),
            }
        }
        Err(e) => Err(e.into()),
    }
}

type WebSocket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A host's connection, kept up for as long as the host is listed. `service`: the host is
/// this desktop's own, which runs its service when nothing listens there.
async fn host_loop(
    name: String,
    host: Host,
    service: Option<Service>,
    mut rx: mpsc::UnboundedReceiver<ClientMessage>,
    cache: Arc<Mutex<Cache>>,
    ui: UiSender,
) {
    let url = &host.url;
    let status = |s: HostStatus, error: Option<String>| {
        let _ = ui.unbounded_send(Incoming::Status { host: name.clone(), status: s, error });
    };
    let mut backoff = Duration::from_millis(300);
    let mut embedded: Option<tokio::task::JoinHandle<Result<()>>> = None;
    // Why the embedded service last stopped, until a connect succeeds: nothing listens
    // because of it, so it is what the host's error says.
    let mut stopped: Option<String> = None;
    // When the embedded service was last started.
    let mut started: Option<Instant> = None;
    // A host that stays unreachable is logged once, not on every retry.
    let mut failing = false;
    loop {
        status(HostStatus::Connecting, None);
        match connect(&host).await {
            Ok(ws) => {
                backoff = Duration::from_millis(300);
                failing = false;
                stopped = None;
                let (mut sink, mut stream) = ws.split();
                // The host's first message is read, and it is one to talk to. Until then it
                // is asked nothing and not reported connected.
                let mut greeted = false;
                // Why the host is not talked to, if it isn't: it speaks another protocol.
                let mut unfit = None;
                let silent = tokio::time::sleep(HELLO_WAIT);
                tokio::pin!(silent);
                loop {
                    tokio::select! {
                        // Something else listens there, or the host hangs: it is left alone
                        // like one that can't be talked to.
                        () = &mut silent, if !greeted => {
                            unfit = Some("It took the connection and said nothing".to_owned());
                            break;
                        }
                        Some(req) = rx.recv(), if greeted => {
                            let text = serde_json::to_string(&req).unwrap();
                            if let Err(e) = sink.send(Message::Text(text.into())).await {
                                warn!(host = %name, "send: {e}");
                                break;
                            }
                        }
                        item = stream.next() => match item {
                            Some(Ok(Message::Text(text))) => match serde_json::from_str::<ServerMessage>(&text) {
                                Ok(msg) => {
                                    // The service's first message. One that speaks another protocol
                                    // is left alone until either side is updated: what it says
                                    // would not be read, or read wrong.
                                    if let ServerMessage::Hello { host_name, version, protocol, .. } = &msg {
                                        info!(host = %name, machine = %host_name, %version, protocol, "connected");
                                        if *protocol != dirigent_protocol::PROTOCOL {
                                            let ours = dirigent_protocol::release::VERSION;
                                            let which = if *protocol < dirigent_protocol::PROTOCOL { "that host" } else { "this app" };
                                            unfit = Some(format!("It runs Dirigent {version}, which {ours} here can't talk to: update {which}"));
                                            break;
                                        }
                                    }
                                    if !std::mem::replace(&mut greeted, true) {
                                        // What was asked while the host was down (or left alone)
                                        // is dropped, not sent now: the UI gave back what it had
                                        // pending when it saw `Disconnected`.
                                        while rx.try_recv().is_ok() {}
                                        status(HostStatus::Connected, None);
                                        // Refresh everything we cache for this host.
                                        for req in [
                                            ClientMessage::ListProjects { id: CONNECT_LIST_ID },
                                            ClientMessage::ListThreads { id: CONNECT_LIST_ID },
                                            ClientMessage::ListModels { id: CONNECT_LIST_ID },
                                        ] {
                                            let _ = sink.send(Message::Text(serde_json::to_string(&req).unwrap().into())).await;
                                        }
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
                let wait = unfit.as_ref().map(|why| {
                    warn!(host = %name, "{why}");
                    UNFIT_RETRY
                });
                status(HostStatus::Disconnected, unfit);
                // Looked at again now and then: an update may have come. Hung up meanwhile.
                if let Some(wait) = wait {
                    drop((sink, stream));
                    tokio::time::sleep(wait).await;
                }
            }
            Err(e) => {
                // `local` refusing before we ever started the service is the normal start-up,
                // not worth a line in the log or an error in Settings.
                let starting = service.is_some() && embedded.is_none() && stopped.is_none();
                if !starting && !std::mem::replace(&mut failing, true) {
                    warn!(host = %name, "connect {url}: {e:#} (retrying)");
                }
                // Nothing listening locally: run the service in-process. It lives and dies
                // with the desktop. If it exits (bind failed, pi's threads didn't migrate),
                // the next refusal retries, quietly: a service that keeps failing the same
                // way is logged once, like the connect.
                if let Some(run) = service
                    && embedded.as_ref().is_none_or(|h| h.is_finished())
                {
                    if let Some(ended) = embedded.take() {
                        let why = match ended.await {
                            Ok(Ok(())) => "its listeners closed".to_owned(),
                            Ok(Err(e)) => format!("{e:#}"),
                            Err(e) => e.to_string(),
                        };
                        if stopped.as_ref() != Some(&why) {
                            error!("embedded service stopped: {why}");
                        }
                        stopped = Some(why);
                    }
                    // One that stopped is started again now and then, not at every refusal:
                    // what stopped it (pi's threads not migrating: a full disk) costs as much
                    // every time.
                    if stopped.is_none() || started.is_none_or(|at| at.elapsed() >= UNFIT_RETRY) {
                        if stopped.is_none() {
                            info!("starting embedded service on :{}", dirigent_service::port());
                        }
                        embedded = Some(tokio::spawn(run()));
                        started = Some(Instant::now());
                    }
                }
                // Requests made while offline are dropped, not queued for a later reconnect.
                while rx.try_recv().is_ok() {}
                let error = match &stopped {
                    Some(why) => format!("Its service stopped: {why}"),
                    None => format!("{e:#}"),
                };
                status(HostStatus::Disconnected, (!starting).then_some(error));
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc::UnboundedReceiver;
    use tokio::net::TcpListener;

    /// A host's loop on `url`: what it tells the UI, and the way to ask the host things.
    fn start(name: &str, url: String, service: Option<Service>) -> (UnboundedReceiver<Incoming>, mpsc::UnboundedSender<ClientMessage>) {
        let (ui, ui_rx) = futures::channel::mpsc::unbounded();
        let (tx, rx) = mpsc::unbounded_channel();
        let cache = Arc::new(Mutex::new(Cache::in_memory().unwrap()));
        tokio::spawn(host_loop(name.to_owned(), Host::new(url), service, rx, cache, ui));
        (ui_rx, tx)
    }

    /// The next status the UI is told.
    async fn status(ui: &mut UnboundedReceiver<Incoming>) -> (HostStatus, Option<String>) {
        loop {
            if let Incoming::Status { status, error, .. } = ui.next().await.expect("the host's loop ended") {
                return (status, error);
            }
        }
    }

    /// A service that greets its one client with `hello`: its address, and the first
    /// `count` requests it gets (fewer if the client hangs up first).
    async fn host(hello: String, count: usize) -> (String, tokio::task::JoinHandle<Vec<ClientMessage>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let asked = tokio::spawn(async move {
            let mut ws = tokio_tungstenite::accept_async(listener.accept().await.unwrap().0).await.unwrap();
            ws.send(Message::Text(hello.into())).await.unwrap();
            let mut asked = vec![];
            while asked.len() < count
                && let Some(Ok(Message::Text(text))) = ws.next().await
            {
                asked.push(serde_json::from_str(&text).unwrap());
            }
            asked
        });
        (url, asked)
    }

    #[tokio::test]
    async fn host_is_asked_nothing_before_its_hello_is_read() {
        let queued = ClientMessage::ListDirs { id: 7, path: "asked while it was down".into() };
        // One from before `PROTOCOL` is never called connected, and gets no request at all.
        let (url, asked) = host(r#"{"type":"hello","host_name":"h","version":"0.0.9"}"#.into(), 1).await;
        let (mut ui, tx) = start("old", url, None);
        tx.send(queued.clone()).unwrap();
        assert_eq!(status(&mut ui).await, (HostStatus::Connecting, None));
        let (state, why) = status(&mut ui).await;
        assert_eq!(state, HostStatus::Disconnected);
        assert!(why.unwrap().ends_with("update that host"));
        assert_eq!(asked.await.unwrap(), []);

        // One on ours is connected once it said so, and what was asked before is dropped.
        let (host_name, version, service_id) = ("h".into(), "0".into(), "s".into());
        let hello = ServerMessage::Hello { host_name, version, service_id, protocol: dirigent_protocol::PROTOCOL };
        let (url, asked) = host(serde_json::to_string(&hello).unwrap(), 4).await;
        let (mut ui, tx) = start("new", url, None);
        tx.send(queued).unwrap();
        assert_eq!(status(&mut ui).await, (HostStatus::Connecting, None));
        assert_eq!(status(&mut ui).await, (HostStatus::Connected, None));
        tx.send(ClientMessage::ListModels { id: 8 }).unwrap();
        let refresh = CONNECT_LIST_ID;
        assert_eq!(
            asked.await.unwrap(),
            [
                ClientMessage::ListProjects { id: refresh },
                ClientMessage::ListThreads { id: refresh },
                ClientMessage::ListModels { id: refresh },
                ClientMessage::ListModels { id: 8 }
            ]
        );
    }

    #[tokio::test]
    async fn local_service_that_does_not_start_is_the_hosts_error() {
        fn broken() -> BoxFuture<'static, Result<()>> {
            Box::pin(async { anyhow::bail!("migrate service.db: disk full") })
        }
        // A port nothing listens on.
        let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let (mut ui, _tx) = start(LOCAL, format!("ws://127.0.0.1:{port}/ws"), Some(broken));
        // The first refusal starts the service and is no error; the next is the service's.
        assert_eq!(status(&mut ui).await, (HostStatus::Connecting, None));
        assert_eq!(status(&mut ui).await, (HostStatus::Disconnected, None));
        assert_eq!(status(&mut ui).await, (HostStatus::Connecting, None));
        let why = "Its service stopped: migrate service.db: disk full";
        assert_eq!(status(&mut ui).await, (HostStatus::Disconnected, Some(why.to_owned())));
    }
}
