//! WebSocket endpoint: one task per connection, requests handled concurrently,
//! pushes fanned out from a broadcast channel.

use crate::Service;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use dirigent_protocol::{ClientMessage, ServerMessage};
use futures_util::{SinkExt, StreamExt};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};

/// Listen on every address (all bound before serving, so a taken port fails fast).
pub async fn serve(svc: Arc<Service>, addrs: &[SocketAddr]) -> anyhow::Result<()> {
    let app = Router::new().route("/ws", get(upgrade)).with_state(svc);
    let mut listeners = Vec::new();
    for addr in addrs {
        listeners.push(tokio::net::TcpListener::bind(addr).await?);
        eprintln!("[service] listening on ws://{addr}/ws");
    }
    futures_util::future::try_join_all(listeners.into_iter().map(|l| axum::serve(l, app.clone()).into_future())).await?;
    Ok(())
}

async fn upgrade(ws: WebSocketUpgrade, State(svc): State<Arc<Service>>) -> Response {
    ws.on_upgrade(move |socket| connection(socket, svc))
}

async fn connection(socket: WebSocket, svc: Arc<Service>) {
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
                        eprintln!("[service] client lagged, dropped {n} pushes");
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

    while let Some(Ok(msg)) = ws_rx.next().await {
        let Message::Text(text) = msg else { continue };
        match serde_json::from_str::<ClientMessage>(&text) {
            Ok(req) => {
                let (svc, out) = (svc.clone(), out.clone());
                tokio::spawn(async move {
                    let _ = out.send(svc.handle(req).await);
                });
            }
            Err(e) => {
                let _ = out.send(ServerMessage::Error { reply_to: None, message: format!("bad request: {e}") });
            }
        }
    }
    writer.abort();
}
