//! The leaderboard's desktop side: joining with a forge account, uploading every connected
//! service's usage, and fetching the board the Usage modal's Leaderboard tab shows
//! (`usage.rs`). The api is `dirigent_api`'s `leaderboard.rs`, which explains the sign-in.
//!
//! Joining: a loopback listener on a random port, then the browser at the api's
//! `auth/start` with that port and a random state. The browser comes back to the listener
//! with a one-time code (or an error), which is traded for the `Account` (`claim`). The
//! account is kept in `kv`: the data dir, so a test instance (`--data-dir`) starts out of it.
//!
//! Uploading: every `UPLOAD_EVERY` and whenever a service says hello, each connected
//! service's usage since joining (at most `UPLOAD_DAYS` back) is asked for per hour, summed
//! over its models and sent under the service's id. The api keeps the bigger of two
//! snapshots of an hour, so sending the same hours again (or from another desktop that
//! shares the service) counts nothing twice.

use crate::app::{Dirigent, image_from};
use crate::net::HostStatus;
use anyhow::{Context as _, Result, bail};
use dirigent_protocol::leaderboard::{Account, Board, HOUR, Range, Slot, Upload};
use dirigent_protocol::{ClientMessage, UsageBucket};
use gpui::Context;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn};

const ACCOUNT_KEY: &str = "leaderboard_account";
/// How often (seconds, on `Dirigent::tick`) the services' usage is uploaded.
pub const UPLOAD_EVERY: u64 = 300;
/// How far back an upload reaches: a desktop that was off longer misses what's older.
const UPLOAD_DAYS: i64 = 30;
/// How long a join waits for the browser.
const JOIN_FOR: Duration = Duration::from_secs(600);

/// The api: `DIRIGENT_API_URL`, for tests, or the real one.
fn api() -> String {
    std::env::var("DIRIGENT_API_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "https://dirigent.sebba.dev".into())
        .trim_end_matches('/')
        .to_owned()
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .user_agent(format!("Dirigent/{}", env!("CARGO_PKG_VERSION")))
            .timeout_global(Some(Duration::from_secs(30)))
            .build(),
    )
}

/// The leaderboard's state, on `Dirigent`.
#[derive(Default)]
pub struct State {
    pub account: Option<Account>,
    /// A join waiting for the browser: set to cancel it.
    pub joining: Option<Arc<AtomicBool>>,
    pub range: Range,
    /// The board for `range`, as last fetched; `None` until one arrives.
    pub board: Option<Board>,
    /// Why the last fetch failed.
    pub error: Option<String>,
    /// Bumped by every fetch: only the latest one's answer is shown.
    fetch: u64,
    /// Each host's latest `ListUsage` for an upload: its request id and the service id it's
    /// for. A newer one replaces it, so one that never got an answer doesn't linger.
    uploads: HashMap<String, (u64, String)>,
    /// Services the api said are another player's: not uploaded again.
    not_ours: HashSet<String>,
    /// The last upload failed: logged once until one works again.
    failing: bool,
    /// Pictures by url: `None` while loading (or when it didn't load).
    pub avatars: HashMap<String, Option<Arc<gpui::Image>>>,
}

impl State {
    pub fn new(cache: &crate::cache::Cache) -> Self {
        let account = cache.get(ACCOUNT_KEY).and_then(|s| serde_json::from_str(&s).ok());
        Self { account, ..Self::default() }
    }

    pub fn you(&self) -> Option<&str> {
        self.account.as_ref().map(|a| a.id.as_str())
    }

    /// `host` disconnected: its uploads' answers won't come.
    pub fn drop_uploads(&mut self, host: &str) {
        self.uploads.remove(host);
    }
}

/// Run `work` (blocking HTTP) on a thread of its own, then `done` with its result on the UI
/// thread. Not on gpui's background pool: a few slow requests would stall its other work
/// (image decoding, the background pattern).
fn http<T: Send + 'static>(
    cx: &mut Context<Dirigent>,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&mut Dirigent, T, &mut Context<Dirigent>) + 'static,
) {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    cx.spawn(async move |this, cx| {
        let Ok(out) = rx.await else { return };
        this.update(cx, |this, cx| {
            done(this, out, cx);
            cx.notify();
        })
        .ok();
    })
    .detach();
}

impl Dirigent {
    /// "Join": sign in with the forge in the browser (see the module doc).
    pub fn join_board(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if self.leaderboard.joining.is_some() {
            return;
        }
        let (port, listener) = match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).and_then(|l| Ok((l.local_addr()?.port(), l))) {
            Ok(bound) => bound,
            Err(e) => return self.toast(format!("Couldn't start the sign-in: {e}"), cx),
        };
        let state = uuid::Uuid::new_v4().simple().to_string();
        let url = format!("{}/api/v0/auth/start?port={port}&state={state}", api());
        info!(%url, "joining the leaderboard");
        cx.open_url(&url);
        let cancel = Arc::new(AtomicBool::new(false));
        self.leaderboard.joining = Some(cancel.clone());
        let (tx, rx) = futures::channel::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(wait_for_browser(listener, &state, &cancel));
        });
        cx.spawn(async move |this, cx| {
            // Gone without an answer (it panicked): given up, like a cancel.
            let joined = rx.await.unwrap_or(Ok(None));
            this.update(cx, |this, cx| {
                this.leaderboard.joining = None;
                match joined {
                    Ok(Some(account)) => {
                        info!(player = %account.name, "joined the leaderboard");
                        let _ = this.cache.lock().unwrap().set(ACCOUNT_KEY, &serde_json::to_string(&account).unwrap_or_default());
                        this.leaderboard.account = Some(account);
                        this.leaderboard.not_ours.clear();
                        this.upload_all();
                        this.fetch_board(cx);
                    }
                    Ok(None) => {}
                    Err(e) => this.toast(format!("Couldn't join the leaderboard: {e:#}"), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub fn cancel_join(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.leaderboard.joining.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    /// "Leave": off the board, usage and all; signed out on every desktop.
    pub fn leave_board(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        let Some(token) = self.leaderboard.account.as_ref().map(|a| a.token.clone()) else { return };
        http(
            cx,
            move || -> Result<()> {
                agent().delete(format!("{}/api/v0/me", api())).header("authorization", format!("Bearer {token}")).call()?;
                Ok(())
            },
            |this, left, cx| match left {
                // A sign-in the api no longer knows has left already.
                Err(e) if status(&e) != Some(401) => this.toast(format!("Couldn't leave the leaderboard: {e:#}"), cx),
                _ => {
                    this.sign_out();
                    this.fetch_board(cx);
                }
            },
        );
    }

    fn sign_out(&mut self) {
        self.leaderboard.account = None;
        self.leaderboard.not_ours.clear();
        let _ = self.cache.lock().unwrap().remove(ACCOUNT_KEY);
    }

    /// Upload every connected service's usage (`UPLOAD_EVERY`, and on joining).
    pub fn upload_all(&mut self) {
        let hosts: Vec<String> = self.hosts.iter().map(|h| h.name.clone()).collect();
        for host in hosts {
            self.upload_usage(&host);
        }
    }

    /// Ask `host` for its usage per hour since joining; `on_upload_usage` sends it on.
    /// Nothing while out of the leaderboard, or for a service that doesn't send its id.
    pub fn upload_usage(&mut self, host: &str) {
        let Some(account) = &self.leaderboard.account else { return };
        let connected = self.host(host).filter(|h| h.status == HostStatus::Connected);
        let Some(service) = connected.and_then(|h| h.service_id.clone()) else { return };
        if self.leaderboard.not_ours.contains(&service) {
            return;
        }
        let now = now_ms();
        let since = floor_hour(account.joined_at.max(now - UPLOAD_DAYS * 24 * HOUR));
        let until = floor_hour(now) + HOUR;
        let id = self.request(host, |id| ClientMessage::ListUsage { id, since, until, bucket_ms: HOUR });
        self.leaderboard.uploads.insert(host.to_owned(), (id, service));
    }

    /// A `Usage` reply: if it's for an upload, send it to the api and return true.
    pub fn on_upload_usage(&mut self, reply_to: u64, buckets: &[UsageBucket], cx: &mut Context<Self>) -> bool {
        let ours = self.leaderboard.uploads.iter().find(|(_, (id, _))| *id == reply_to).map(|(host, _)| host.clone());
        let Some((_, service)) = ours.and_then(|host| self.leaderboard.uploads.remove(&host)) else { return false };
        let Some(token) = self.leaderboard.account.as_ref().map(|a| a.token.clone()) else { return true };
        let up = Upload { service: service.clone(), hours: hours(buckets) };
        http(
            cx,
            move || -> Result<()> {
                agent()
                    .put(format!("{}/api/v0/usage", api()))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .send(serde_json::to_string(&up)?)?;
                Ok(())
            },
            move |this, sent, cx| match sent {
                Ok(()) => this.leaderboard.failing = false,
                // The api forgot this sign-in (left from another desktop): out here too, said
                // once however many hosts' uploads find out.
                Err(e) if status(&e) == Some(401) => {
                    if this.leaderboard.account.is_some() {
                        this.sign_out();
                        this.toast("Signed out of the leaderboard: the sign-in is no longer valid", cx);
                    }
                }
                Err(e) if status(&e) == Some(409) => {
                    warn!(%service, "leaderboard: this service is another player's (or one too many), not uploading it");
                    this.leaderboard.not_ours.insert(service);
                }
                Err(e) => {
                    if !std::mem::replace(&mut this.leaderboard.failing, true) {
                        warn!("leaderboard upload: {e:#}");
                    }
                }
            },
        );
        true
    }

    /// Fetch the board for the range shown; the one shown stays up until it's in.
    pub fn fetch_board(&mut self, cx: &mut Context<Self>) {
        self.leaderboard.fetch += 1;
        let (fetch, range) = (self.leaderboard.fetch, self.leaderboard.range);
        http(
            cx,
            move || -> Result<Board> {
                let url = format!("{}/api/v0/leaderboard?range={}", api(), range.as_str());
                let body = agent().get(&url).call().with_context(|| format!("GET {url}"))?.body_mut().read_to_string()?;
                Ok(serde_json::from_str(&body)?)
            },
            move |this, board, cx| {
                if this.leaderboard.fetch != fetch {
                    return;
                }
                match board {
                    Ok(board) => {
                        this.leaderboard.board = Some(board);
                        this.leaderboard.error = None;
                        this.fetch_avatars(cx);
                    }
                    Err(e) => {
                        warn!("leaderboard: {e:#}");
                        this.leaderboard.error = Some(format!("{e:#}"));
                    }
                }
            },
        );
    }

    pub fn set_board_range(&mut self, range: Range, cx: &mut Context<Self>) {
        if self.leaderboard.range != range {
            self.leaderboard.range = range;
            self.leaderboard.board = None;
            self.usage.hover = None;
        }
        self.fetch_board(cx);
    }

    /// Load the pictures of the board's players and of the account not loaded yet.
    fn fetch_avatars(&mut self, cx: &mut Context<Self>) {
        let lb = &self.leaderboard;
        let urls: Vec<String> = lb
            .board
            .iter()
            .flat_map(|b| b.players.iter().map(|p| p.picture.clone()))
            .chain(lb.account.iter().map(|a| a.picture.clone()))
            .filter(|u| u.starts_with("http") && !lb.avatars.contains_key(u))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        for url in urls {
            self.leaderboard.avatars.insert(url.clone(), None);
            let get = url.clone();
            http(
                cx,
                move || -> Result<Vec<u8>> { Ok(agent().get(&get).call()?.body_mut().with_config().limit(4 << 20).read_to_vec()?) },
                move |this, bytes, _| {
                    let image = bytes.ok().and_then(|b| {
                        let mime = image::guess_format(&b).ok()?.to_mime_type();
                        image_from(mime, b)
                    });
                    this.leaderboard.avatars.insert(url, image);
                },
            );
        }
    }
}

/// The HTTP status of a failed request, if it got one.
fn status(e: &anyhow::Error) -> Option<u16> {
    match e.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::StatusCode(code)) => Some(*code),
        _ => None,
    }
}

/// A host's usage per hour and model, summed per hour: what an upload carries.
fn hours(buckets: &[UsageBucket]) -> Vec<Slot> {
    let mut by_hour: BTreeMap<i64, Slot> = BTreeMap::new();
    for b in buckets {
        let s = by_hour.entry(b.start).or_insert_with(|| Slot { start: b.start, ..Slot::default() });
        s.input += b.input;
        s.output += b.output;
        s.cache_read += b.cache_read;
        s.cache_write += b.cache_write;
        s.cost += b.cost;
        s.messages += b.messages;
    }
    by_hour.into_values().collect()
}

/// Serve the browser's return from the sign-in on `listener` until it brings our `state`:
/// then claim the account and tell the browser how it went. `None` if cancelled or given up.
fn wait_for_browser(listener: TcpListener, state: &str, cancel: &AtomicBool) -> Result<Option<Account>> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + JOIN_FOR;
    while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        // Anything else that finds the port (a favicon fetch) gets a 404.
        let query = read_query(&mut stream).unwrap_or_default();
        if query.get("state").map(String::as_str) != Some(state) {
            let _ = respond(&mut stream, "404 Not Found", "Not found");
            continue;
        }
        let joined = match (query.get("code"), query.get("error")) {
            (Some(code), _) => claim(code),
            (None, error) => Err(anyhow::anyhow!("the forge said {}", error.map_or("no", String::as_str))),
        };
        let page = match &joined {
            Ok(a) => format!("You're on the leaderboard, {}. Back to Dirigent!", html(&a.name)),
            Err(e) => format!("Couldn't join the leaderboard: {}", html(&format!("{e:#}"))),
        };
        let _ = respond(&mut stream, "200 OK", &page);
        return joined.map(Some);
    }
    Ok(None)
}

/// The query of an HTTP request's target (`GET /?a=b HTTP/1.1`), decoded.
fn read_query(stream: &mut TcpStream) -> Result<HashMap<String, String>> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = vec![0; 8192];
    let mut len = 0;
    while len < buf.len() && !buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut buf[len..])?;
        if n == 0 {
            break;
        }
        len += n;
    }
    let head = String::from_utf8_lossy(&buf[..len]);
    let target = head.lines().next().and_then(|l| l.split(' ').nth(1)).unwrap_or("");
    Ok(parse_query(target))
}

fn parse_query(target: &str) -> HashMap<String, String> {
    let query = target.split_once('?').map_or("", |(_, q)| q);
    query.split('&').filter_map(|pair| pair.split_once('=')).map(|(k, v)| (unescape(k), unescape(v))).collect()
}

/// `a%20b+c` → `a b c`.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                i += 1;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn respond(stream: &mut TcpStream, status: &str, text: &str) -> std::io::Result<()> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Dirigent</title>\
         <body style=\"font:16px system-ui;display:grid;place-items:center;height:90vh\"><p>{text}</p>"
    );
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// The sign-in's one-time code for the account.
fn claim(code: &str) -> Result<Account> {
    let url = format!("{}/api/v0/auth/claim", api());
    let body = serde_json::json!({ "code": code }).to_string();
    let mut res = match agent().post(&url).header("content-type", "application/json").send(body) {
        Ok(res) => res,
        Err(ureq::Error::StatusCode(404)) => bail!("the sign-in expired, try again"),
        Err(e) => return Err(e).with_context(|| format!("POST {url}")),
    };
    Ok(serde_json::from_str(&res.body_mut().read_to_string()?)?)
}

fn now_ms() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn floor_hour(t: i64) -> i64 {
    t - t.rem_euclid(HOUR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploads_sum_models_per_hour() {
        let b = |start: i64, model: &str, cost: f64| UsageBucket {
            start,
            provider: "p".into(),
            model: model.into(),
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
            cost,
            messages: 1,
        };
        let h = hours(&[b(HOUR, "a", 1.), b(0, "a", 0.5), b(HOUR, "b", 2.)]);
        assert_eq!(h.iter().map(|s| (s.start, s.cost, s.messages, s.cache_read)).collect::<Vec<_>>(), [(0, 0.5, 1, 3), (HOUR, 3., 2, 6)]);
    }

    #[test]
    fn browser_return_query() {
        let q = parse_query("/?state=abc&code=x%2Fy&error=access+denied");
        assert_eq!((q["state"].as_str(), q["code"].as_str(), q["error"].as_str()), ("abc", "x/y", "access denied"));
        assert!(parse_query("/favicon.ico").is_empty());
        assert_eq!(unescape("100%"), "100%");
    }
}
