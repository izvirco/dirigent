//! WebSocket endpoint: one task per connection, requests handled concurrently,
//! pushes fanned out from a broadcast channel. The open listeners (loopback, tailscale)
//! take anyone; the public one (`auth.rs`) only a known bearer token.

use crate::Service;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::serve::ListenerExt;
use dirigent_protocol::{ClientMessage, ServerMessage};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

/// Bind every address, all before serving, so a taken port fails fast. `true` marks the
/// public listener.
pub async fn bind(addrs: &[(SocketAddr, bool)]) -> anyhow::Result<Vec<(TcpListener, bool)>> {
    let mut listeners = Vec::new();
    for &(addr, public) in addrs {
        listeners.push((TcpListener::bind(addr).await?, public));
        info!("listening on ws://{addr}/ws{}", if public { " (public: bearer token required)" } else { "" });
    }
    Ok(listeners)
}

/// What a listener's requests are handled with.
#[derive(Clone)]
struct Listener {
    svc: Arc<Service>,
    /// Every upgrade needs a bearer token (`auth::admit`).
    public: bool,
}

/// Serve on the bound listeners until one fails.
pub async fn serve(svc: Arc<Service>, listeners: Vec<(TcpListener, bool)>) -> anyhow::Result<()> {
    // With each connection's peer address, for the log.
    let serve = |(l, public): (TcpListener, bool)| {
        let app = Router::new().route("/ws", get(upgrade)).with_state(Listener { svc: svc.clone(), public });
        axum::serve(l.tap_io(tune), app.into_make_service_with_connect_info::<SocketAddr>()).into_future()
    };
    futures_util::future::try_join_all(listeners.into_iter().map(serve)).await?;
    Ok(())
}

/// Keep at most this much unsent data in a connection's kernel send buffer. Left alone,
/// Linux grows the buffer to megabytes and fills it with images, and what is in it goes out
/// first: on a slow link a reply would wait seconds behind them however `connection` orders
/// its writes. Only unsent data counts, not data in flight, so throughput is the same.
#[cfg(target_os = "linux")]
const NOTSENT_LOWAT: u32 = 128 << 10;

/// Set up an accepted connection's socket (`NOTSENT_LOWAT`; Windows has no such option).
#[cfg(target_os = "linux")]
fn tune(tcp: &mut TcpStream) {
    if let Err(e) = socket2::SockRef::from(&*tcp).set_tcp_notsent_lowat(NOTSENT_LOWAT) {
        warn!("set TCP_NOTSENT_LOWAT: {e}");
    }
}

#[cfg(not(target_os = "linux"))]
fn tune(_: &mut TcpStream) {}

/// Largest message we read, and frame too: clients send each message as one frame, so
/// axum's 16 MiB frame default would cut off big requests far below its message limit.
const MAX_MESSAGE: usize = 64 << 20;

/// The connection a request came in on: what a login sends its steps to, and is cancelled
/// with (`Service::disconnected`).
#[derive(Clone)]
pub struct Conn {
    pub id: u64,
    pub out: mpsc::UnboundedSender<ServerMessage>,
}

/// Checked before the upgrade: a public listener's request without a known bearer gets a
/// 401 and never reaches the service.
async fn upgrade(
    ws: WebSocketUpgrade,
    State(Listener { svc, public }): State<Listener>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    match crate::auth::admit(public, &svc.storage, &headers) {
        Ok(Some(who)) => debug!(%peer, %who, "authorized"),
        Ok(None) => {}
        Err(why) => {
            warn!(%peer, "refused: {why}");
            return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], why).into_response();
        }
    }
    ws.max_frame_size(MAX_MESSAGE).max_message_size(MAX_MESSAGE).on_upgrade(move |socket| connection(socket, svc, peer))
}

async fn connection(socket: WebSocket, svc: Arc<Service>, peer: SocketAddr) {
    // Debug: scripts and tools come and go often.
    debug!(%peer, "client connected");
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let conn = Conn { id: NEXT.fetch_add(1, Ordering::Relaxed), out: out.clone() };
    // Images (`Blob` replies) have a queue of their own, sent from only when nothing else
    // waits: opening a thread asks for all its images at once, possibly hundreds of MB,
    // minutes over a slow link, and every reply and push would wait behind them.
    let (blobs, mut blobs_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut push_rx = svc.push.subscribe();

    let _ = out.send(ServerMessage::Hello {
        host_name: svc.host_name.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        service_id: svc.service_id.clone(),
    });

    // Writer: this connection's replies, then global pushes, then images. One image already
    // being written still goes first: a message can't be cut into.
    let writer = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                biased;
                Some(m) = out_rx.recv() => m,
                p = push_rx.recv() => match p {
                    Ok(m) => m,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(%peer, "client lagged, dropped {n} pushes");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                Some(m) = blobs_rx.recv() => m,
                else => break,
            };
            let text = serde_json::to_string(&msg).unwrap();
            if ws_tx.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // Why the connection ended, if a read failed: mostly a client that went without a close
    // frame, but it may be a message over the limit, which the client only sees as the
    // connection dropping.
    let mut error = None;
    while let Some(msg) = ws_rx.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(e) => {
                error = Some(e);
                break;
            }
        };
        let text = match msg {
            Message::Text(text) => text,
            // Done: reading on would only race the client's socket going away.
            Message::Close(_) => break,
            _ => continue,
        };
        match serde_json::from_str::<ClientMessage>(&text) {
            Ok(req) => {
                let (svc, conn, blobs) = (svc.clone(), conn.clone(), blobs.clone());
                tokio::spawn(async move {
                    let reply = svc.handle(req, &conn).await;
                    let queue = if matches!(reply, ServerMessage::Blob { .. }) { &blobs } else { &conn.out };
                    let _ = queue.send(reply);
                });
            }
            Err(e) => {
                // Answer the request's id if it has one (a newer client asking for something
                // this host doesn't know yet), so the client's request fails instead of hanging.
                warn!(%peer, "bad request: {e}");
                let reply_to = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("id")?.as_u64());
                let _ = out.send(ServerMessage::Error { reply_to, message: format!("bad request: {e}") });
            }
        }
    }
    writer.abort();
    svc.disconnected(conn.id);
    match error {
        Some(e) => warn!(%peer, "connection dropped: {e}"),
        None => debug!(%peer, "client disconnected"),
    }
}
