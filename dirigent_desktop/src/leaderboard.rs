//! The leaderboard's desktop side: joining with the forge account, uploading every connected
//! service's usage, and fetching the board the Usage modal's Leaderboard tab shows
//! (`usage.rs`). The api is `dirigent_api`'s `leaderboard.rs`.
//!
//! Joining: signed in to the forge (`account.rs`; Join takes you to Settings › Forge
//! providers first if need be), the desktop asks the api to `join` with a forge JWT made
//! for the api's url, and keeps the `Account` it answers with in `kv`: the data dir, so a
//! test instance (`--data-dir`) starts out of it. Uploads and leaving send a fresh JWT each
//! time (`forge::token`, cached for its ten minutes). The account counts while the forge
//! account it joined as is signed in (`active`): signed out, nothing is uploaded; signed in
//! as someone else, it waits, and Join replaces it with that account.
//!
//! Desktops up to 0.0.6 joined through the api itself and saved an api token in the
//! `Account`. The api takes no such token now and the field is ignored: a saved account
//! counts like any other, while its forge user is signed in.
//!
//! Uploading: every `UPLOAD_EVERY` and whenever a service says hello, each connected
//! service's usage since joining (at most `UPLOAD_DAYS` back) is asked for per hour, summed
//! over its models and sent under the service's id. The api keeps the bigger of two
//! snapshots of an hour, so sending the same hours again (or from another desktop that
//! shares the service) counts nothing twice.

use crate::app::{Dirigent, image_from};
use crate::cache::Cache;
use crate::net::HostStatus;
use anyhow::{Context as _, Result};
use dirigent_protocol::leaderboard::{Account, Board, HOUR, Range, Slot, Upload};
use dirigent_protocol::{ClientMessage, UsageBucket};
use dirigent_service::forge;
use gpui::Context;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

const ACCOUNT_KEY: &str = "leaderboard_account";
/// How often (seconds, on `Dirigent::tick`) the services' usage is uploaded.
pub const UPLOAD_EVERY: u64 = 300;
/// How far back an upload reaches: a desktop that was off longer misses what's older.
const UPLOAD_DAYS: i64 = 30;
/// What the api answers an upload or leave with a valid forge token of someone not on the
/// board: the one status that means "left" (from another desktop). A token it refuses (401)
/// or can't check (503) is no reason to forget the player.
const NOT_ON_THE_BOARD: u16 = 403;

/// The api: `DIRIGENT_API_URL`, for tests, or the real one. Also the `resource` of the
/// forge JWTs it takes (their audience, which the api checks against its own url).
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
    /// The player this desktop joined as; `active` says if it counts.
    account: Option<Account>,
    /// A join waiting for the api.
    pub joining: bool,
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
    pub fn new(cache: &Cache) -> Self {
        let account = cache.get(ACCOUNT_KEY).and_then(|s| serde_json::from_str(&s).ok());
        Self { account, ..Self::default() }
    }

    /// The account, if it counts now: while the forge account it joined as is signed in.
    pub fn active(&self, forge: &forge::Status) -> Option<&Account> {
        let sub = forge.signed_in()?.sub.as_str();
        self.account.as_ref().filter(|a| a.id == sub)
    }

    /// `host` disconnected: its uploads' answers won't come.
    pub fn drop_uploads(&mut self, host: &str) {
        self.uploads.remove(host);
    }
}

/// Run `work` (blocking HTTP) on a thread of its own, then `done` with its result on the UI
/// thread. Not on gpui's background pool: a few slow requests would stall its other work
/// (image decoding, the background pattern).
pub fn http<T: Send + 'static>(
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
    /// "Join": as the forge account. Not signed in to the forge, it takes you where you sign
    /// in (Settings › Forge providers); you join from here once you are.
    pub fn join_board(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if self.leaderboard.joining {
            return;
        }
        if self.forge.status.signed_in().is_none() {
            return self.open_forge_settings(cx);
        }
        self.leaderboard.joining = true;
        http(
            cx,
            || -> Result<Account> {
                let token = forge::token(&api())?;
                let url = format!("{}/api/v0/join", api());
                let body = agent().post(&url).header("authorization", format!("Bearer {token}")).send_empty();
                Ok(serde_json::from_str(&body.with_context(|| format!("POST {url}"))?.body_mut().read_to_string()?)?)
            },
            |this, joined, cx| {
                this.leaderboard.joining = false;
                match joined {
                    Ok(account) => {
                        info!(player = %account.name, "joined the leaderboard");
                        let _ = this.cache.lock().unwrap().set(ACCOUNT_KEY, &serde_json::to_string(&account).unwrap_or_default());
                        this.leaderboard.account = Some(account);
                        this.leaderboard.not_ours.clear();
                        this.upload_all();
                        this.fetch_board(cx);
                    }
                    Err(e) => this.toast(format!("Couldn't join the leaderboard: {e:#}"), cx),
                }
            },
        );
        cx.notify();
    }

    /// "Leave": off the board, usage and all; out of it on every desktop.
    pub fn leave_board(&mut self, cx: &mut Context<Self>) {
        self.menu = None;
        if self.leaderboard.active(&self.forge.status).is_none() {
            return;
        }
        http(
            cx,
            move || -> Result<()> {
                let token = forge::token(&api())?;
                agent().delete(format!("{}/api/v0/me", api())).header("authorization", format!("Bearer {token}")).call()?;
                Ok(())
            },
            move |this, left, cx| match left {
                // A player the api no longer knows has left already.
                Err(e) if status(&e) != Some(NOT_ON_THE_BOARD) => this.toast(format!("Couldn't leave the leaderboard: {e:#}"), cx),
                _ => {
                    this.forget_player();
                    this.fetch_board(cx);
                }
            },
        );
    }

    fn forget_player(&mut self) {
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
        let Some(account) = self.leaderboard.active(&self.forge.status) else { return };
        let joined_at = account.joined_at;
        let connected = self.host(host).filter(|h| h.status == HostStatus::Connected);
        let Some(service) = connected.and_then(|h| h.service_id.clone()) else { return };
        if self.leaderboard.not_ours.contains(&service) {
            return;
        }
        let now = now_ms();
        let since = floor_hour(joined_at.max(now - UPLOAD_DAYS * 24 * HOUR));
        let until = floor_hour(now) + HOUR;
        let id = self.request(host, |id| ClientMessage::ListUsage { id, since, until, bucket_ms: HOUR });
        self.leaderboard.uploads.insert(host.to_owned(), (id, service));
    }

    /// A `Usage` reply: if it's for an upload, send it to the api and return true.
    pub fn on_upload_usage(&mut self, reply_to: u64, buckets: &[UsageBucket], cx: &mut Context<Self>) -> bool {
        let ours = self.leaderboard.uploads.iter().find(|(_, (id, _))| *id == reply_to).map(|(host, _)| host.clone());
        let Some((_, service)) = ours.and_then(|host| self.leaderboard.uploads.remove(&host)) else { return false };
        if self.leaderboard.active(&self.forge.status).is_none() {
            return true;
        }
        let up = Upload { service: service.clone(), hours: hours(buckets) };
        http(
            cx,
            move || -> Result<()> {
                let token = forge::token(&api())?;
                agent()
                    .put(format!("{}/api/v0/usage", api()))
                    .header("authorization", format!("Bearer {token}"))
                    .header("content-type", "application/json")
                    .send(serde_json::to_string(&up)?)?;
                Ok(())
            },
            move |this, sent, cx| match sent {
                Ok(()) => this.leaderboard.failing = false,
                // The api doesn't know this player (left from another desktop): out here too,
                // said once however many hosts' uploads find out.
                Err(e) if status(&e) == Some(NOT_ON_THE_BOARD) => {
                    if this.leaderboard.account.is_some() {
                        warn!("leaderboard upload: {e:#}");
                        this.forget_player();
                        this.toast("Off the leaderboard: it no longer knows this player. Join again to come back.", cx);
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
            .collect();
        self.fetch_pictures(urls, cx);
    }

    /// Load the pictures at `urls` not loaded (or loading) yet into `avatars`, which the
    /// forge account's (Settings › Forge providers) shares.
    pub fn fetch_pictures(&mut self, urls: impl IntoIterator<Item = String>, cx: &mut Context<Self>) {
        let lb = &self.leaderboard;
        let urls: HashSet<String> = urls.into_iter().filter(|u| u.starts_with("http") && !lb.avatars.contains_key(u)).collect();
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

fn now_ms() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn floor_hour(t: i64) -> i64 {
    t - t.rem_euclid(HOUR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dirigent_service::forge::{Account as ForgeAccount, Status};

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
    fn a_saved_account_counts_while_its_forge_account_is_signed_in() {
        let cache = Cache::in_memory().unwrap();
        let signed_in = |sub: &str| Status::SignedIn(ForgeAccount { sub: sub.into(), username: "seb".into(), picture: String::new() });
        // As 0.0.6 saved it, with the api token it uploaded with: still read, the token ignored.
        cache.set(ACCOUNT_KEY, r#"{"token":"old-token","id":"u1","name":"seb","picture":"","joined_at":7}"#).unwrap();
        let s = State::new(&cache);
        assert_eq!(s.active(&signed_in("u1")).map(|a| a.joined_at), Some(7));
        assert_eq!(s.active(&signed_in("u2")), None);
        assert_eq!(s.active(&Status::SignedOut), None);
    }
}
