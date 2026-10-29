//! WebSocket endpoint: one task per connection, requests handled concurrently,
//! pushes fanned out from a broadcast channel.

use crate::Service;
use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use axum::routing::get;
use dirigent_protocol::{ClientMessage, ServerMessage};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
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
    let serve = |l| axum::serve(l, app.clone().into_make_service_with_connect_info::<SocketAddr>()).into_future();
    futures_util::future::try_join_all(listeners.into_iter().map(serve)).await?;
    Ok(())
}

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
    let mut push_rx = svc.push.subscribe();

    let _ = out.send(ServerMessage::Hello { host_name: svc.host_name.clone(), version: env!("CARGO_PKG_VERSION").into() });

    // Writer: merges replies for this connection with global pushes.
    let writer = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                Some(m) = out_rx.recv() => m,
                p = push_rx.recv() => match p {
                    Ok(m) => m,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(%peer, "client lagged, dropped {n} pushes");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
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
                let (svc, out) = (svc.clone(), out.clone());
                tokio::spawn(async move {
                    let _ = out.send(svc.handle(req).await);
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
