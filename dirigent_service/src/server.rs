use crate::Service;
use crate::auth::Gate;
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

pub async fn bind(addrs: Vec<(SocketAddr, Option<Arc<Gate>>)>) -> anyhow::Result<Vec<(TcpListener, Option<Arc<Gate>>)>> {
    let mut listeners = Vec::new();
    for (addr, gate) in addrs {
        let public = gate.is_some();
        listeners.push((TcpListener::bind(addr).await?, gate));
        info!("listening on ws://{addr}/ws{}", if public { " (public: bearer token required)" } else { "" });
    }
    Ok(listeners)
}

#[derive(Clone)]
struct Listener {
    svc: Arc<Service>,
    gate: Option<Arc<Gate>>,
}

pub async fn serve(svc: Arc<Service>, listeners: Vec<(TcpListener, Option<Arc<Gate>>)>) -> anyhow::Result<()> {
    let serve = |(l, gate): (TcpListener, Option<Arc<Gate>>)| {
        let app = Router::new().route("/ws", get(upgrade)).with_state(Listener { svc: svc.clone(), gate });
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

#[derive(Clone)]
pub struct Conn {
    pub id: u64,
    pub out: mpsc::UnboundedSender<ServerMessage>,
    pub reading: Arc<std::sync::Mutex<Option<crate::screen::Reading>>>,
}

async fn upgrade(
    ws: WebSocketUpgrade,
    State(Listener { svc, gate }): State<Listener>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    match crate::auth::admit(gate, svc.clone(), headers).await {
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
    debug!(%peer, "client connected");
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let conn = Conn { id: NEXT.fetch_add(1, Ordering::Relaxed), out: out.clone(), reading: Default::default() };
    let (blobs, mut blobs_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let (screens, mut screens_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let mut push_rx = svc.push.subscribe();

    let _ = out.send(ServerMessage::Hello {
        host_name: svc.host_name.clone(),
        version: dirigent_protocol::release::VERSION.into(),
        service_id: svc.service_id.clone(),
        protocol: dirigent_protocol::PROTOCOL,
    });

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
                Some(m) = screens_rx.recv() => m,
                Some(m) = blobs_rx.recv() => m,
                else => break,
            };
            let text = serde_json::to_string(&msg).unwrap();
            if ws_tx.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

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
            Message::Close(_) => break,
            _ => continue,
        };
        match serde_json::from_str::<ClientMessage>(&text) {
            Ok(req) => {
                let (svc, conn, blobs, screens) = (svc.clone(), conn.clone(), blobs.clone(), screens.clone());
                tokio::spawn(async move {
                    let reply = svc.handle(req, &conn).await;
                    let queue = match reply {
                        ServerMessage::Blob { .. } => &blobs,
                        ServerMessage::Screen { .. } => &screens,
                        _ => &conn.out,
                    };
                    let _ = queue.send(reply);
                });
            }
            Err(e) => {
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
