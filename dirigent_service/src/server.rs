//! WebSocket endpoint: one task per connection, requests handled concurrently,
//! pushes fanned out from a broadcast channel.

use crate::Service;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use axum::routing::get;
use axum::serve::ListenerExt;
use dirigent_protocol::{ClientMessage, ServerMessage};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

/// Bind every address, all before serving, so a taken port fails fast.
pub async fn bind(addrs: &[SocketAddr]) -> anyhow::Result<Vec<TcpListener>> {
    let mut listeners = Vec::new();
    for addr in addrs {
        listeners.push(TcpListener::bind(addr).await?);
        info!("listening on ws://{addr}/ws");
    }
    Ok(listeners)
}

/// Serve on the bound listeners until one fails.
pub async fn serve(svc: Arc<Service>, listeners: Vec<TcpListener>) -> anyhow::Result<()> {
    let app = Router::new().route("/ws", get(upgrade)).with_state(svc);
    // With each connection's peer address, for the log.
    let serve = |l: TcpListener| axum::serve(l.tap_io(tune), app.clone().into_make_service_with_connect_info::<SocketAddr>()).into_future();
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

async fn upgrade(ws: WebSocketUpgrade, State(svc): State<Arc<Service>>, ConnectInfo(peer): ConnectInfo<SocketAddr>) -> Response {
    ws.max_frame_size(MAX_MESSAGE).max_message_size(MAX_MESSAGE).on_upgrade(move |socket| connection(socket, svc, peer))
}

async fn connection(socket: WebSocket, svc: Arc<Service>, peer: SocketAddr) {
    // Debug: scripts and tools come and go often.
    debug!(%peer, "client connected");
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();
    // Images (`Blob` replies) have a queue of their own, sent from only when nothing else
    // waits: opening a thread asks for all its images at once, possibly hundreds of MB,
    // minutes over a slow link, and every reply and push would wait behind them.
    let (blobs, mut blobs_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut push_rx = svc.push.subscribe();

    let _ = out.send(ServerMessage::Hello { host_name: svc.host_name.clone(), version: env!("CARGO_PKG_VERSION").into() });

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
                let (svc, out, blobs) = (svc.clone(), out.clone(), blobs.clone());
                tokio::spawn(async move {
                    let reply = svc.handle(req).await;
                    let queue = if matches!(reply, ServerMessage::Blob { .. }) { &blobs } else { &out };
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
    match error {
        Some(e) => warn!(%peer, "connection dropped: {e}"),
        None => debug!(%peer, "client disconnected"),
    }
}
