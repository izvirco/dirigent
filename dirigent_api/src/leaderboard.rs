//! The leaderboard (on when `FORGE_CLIENT_ID` is set): players sign in with their forge
//! account, their desktops upload usage (`store.rs` keeps it), and anyone can read the board.
//!
//! ```text
//! GET    /api/v0/auth/start?port=P&state=S     the desktop's browser, on to the forge's sign-in
//! GET    /api/v0/auth/callback                 the forge's answer, on to the desktop
//! POST   /api/v0/auth/claim {code}             the desktop takes its `Account`
//! PUT    /api/v0/usage                         an `Upload` (bearer token)
//! DELETE /api/v0/me                            leave the board, usage and all (bearer token)
//! GET    /api/v0/leaderboard?range=1d|7d|30d|all   the `Board`, cached for a minute
//! ```
//!
//! Signing in: the desktop listens on a loopback port and opens `auth/start` in the browser
//! with that port and a random `state`. The forge (an OpenID provider, `vir_central`'s
//! `oauth.rs`) signs the user in and answers `auth/callback`, where the api trades the code
//! for who it is (with the client secret only the api has: the forge serves no public
//! clients) and sends the browser on to `http://127.0.0.1:{port}/?state=…&code=…`. The
//! desktop checks the state and claims the code. The account only ever goes to the machine
//! the browser runs on, so a sign-in link someone else made can't hand them yours.

use crate::store::{MAX_HOURS, Store, Uploaded};
use anyhow::{Context, Result, bail};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use dirigent_protocol::leaderboard::{Account, Range, Upload};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, error, info};

/// How long a board stays cached: uploads show up within a minute.
const CACHE_FOR: Duration = Duration::from_secs(60);
/// How long a sign-in's code waits for the desktop to claim it.
const CLAIM_FOR: Duration = Duration::from_secs(300);

pub struct Leaderboard {
    store: Mutex<Store>,
    forge: Forge,
    /// Where this api is reached (`DIRIGENT_API_URL`): the forge app's redirect URI is
    /// `{url}/api/v0/auth/callback`.
    url: String,
    /// Sign-ins waiting for their desktop, by one-time code.
    claims: Mutex<HashMap<String, (Instant, Account)>>,
    /// Boards as served (JSON), with when they were made.
    cache: Mutex<HashMap<Range, (Instant, String)>>,
}

/// The forge app the api signs players in with.
struct Forge {
    url: String,
    client_id: String,
    client_secret: String,
}

/// What the forge's userinfo says about a player.
#[derive(Deserialize)]
struct Identity {
    sub: String,
    preferred_username: String,
    #[serde(default)]
    picture: String,
}

impl Leaderboard {
    /// From the environment: `FORGE_CLIENT_ID` and `FORGE_CLIENT_SECRET` (the forge app),
    /// `FORGE_URL` (default `https://forge.sebba.dev`), `DIRIGENT_API_URL` (default
    /// `https://dirigent.sebba.dev`), `DIRIGENT_API_DB` (default `leaderboard.db`).
    /// `None` without a client id: the api serves updates only.
    pub fn from_env() -> Result<Option<Arc<Self>>> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let Some(client_id) = var("FORGE_CLIENT_ID") else { return Ok(None) };
        let forge = Forge {
            url: var("FORGE_URL").unwrap_or_else(|| "https://forge.sebba.dev".into()).trim_end_matches('/').to_owned(),
            client_id,
            client_secret: var("FORGE_CLIENT_SECRET").context("FORGE_CLIENT_SECRET is not set")?,
        };
        let db = PathBuf::from(var("DIRIGENT_API_DB").unwrap_or_else(|| "leaderboard.db".into()));
        let store = Store::open(&db).with_context(|| format!("open {}", db.display()))?;
        let url = var("DIRIGENT_API_URL").unwrap_or_else(|| "https://dirigent.sebba.dev".into()).trim_end_matches('/').to_owned();
        info!(forge = %forge.url, db = %db.display(), "leaderboard on");
        Ok(Some(Arc::new(Self { store: Mutex::new(store), forge, url, claims: Mutex::default(), cache: Mutex::default() })))
    }

    fn callback(&self) -> String {
        format!("{}/api/v0/auth/callback", self.url)
    }

    /// The bearer token's player, or the status to answer with (401 for no player).
    fn player(&self, headers: &HeaderMap) -> Result<String, StatusCode> {
        let token = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        match token.map(|t| self.store.lock().unwrap().user(t.trim())) {
            Some(Ok(Some(user))) => Ok(user),
            Some(Err(e)) => {
                error!("{e:#}");
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
            _ => Err(StatusCode::UNAUTHORIZED),
        }
    }

    /// The forge's code for who it is, then joined or signed in again (blocking).
    fn sign_in(&self, code: &str) -> Result<Account> {
        let id = self.forge.identify(code, &self.callback())?;
        // Desktops fetch it: only from the forge.
        let picture = if id.picture.starts_with(&format!("{}/", self.forge.url)) { id.picture.as_str() } else { "" };
        let account = self.store.lock().unwrap().sign_in(&id.sub, &id.preferred_username, picture, now_ms())?;
        // A new player shows on the board at once.
        self.cache.lock().unwrap().clear();
        info!(player = %account.name, "signed in");
        Ok(account)
    }
}

impl Forge {
    /// The authorization code flow's back half: the code for an access token, the token for
    /// the user's claims (blocking).
    fn identify(&self, code: &str, redirect: &str) -> Result<Identity> {
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
        }
        // A forge that hangs mustn't hold the browser (and a thread) forever.
        let agent = ureq::Agent::new_with_config(ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build());
        let url = format!("{}/api/oauth/token", self.url);
        let mut res = match agent.post(&url).config().http_status_as_error(false).build().send_form([
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect),
            ("client_id", &self.client_id),
            ("client_secret", &self.client_secret),
        ]) {
            Ok(res) => res,
            Err(e) => bail!("POST {url}: {e}"),
        };
        let body = res.body_mut().read_to_string()?;
        if !res.status().is_success() {
            bail!("POST {url}: {} {body}", res.status());
        }
        let token: Token = serde_json::from_str(&body).with_context(|| format!("{url}: {body}"))?;
        let url = format!("{}/api/oauth/userinfo", self.url);
        let body = agent
            .get(&url)
            .header("authorization", format!("Bearer {}", token.access_token))
            .call()
            .with_context(|| format!("GET {url}"))?
            .body_mut()
            .read_to_string()?;
        serde_json::from_str(&body).with_context(|| format!("{url}: {body}"))
    }
}

pub fn router(lb: Arc<Leaderboard>) -> Router {
    Router::new()
        .route("/api/v0/auth/start", get(start))
        .route("/api/v0/auth/callback", get(callback))
        .route("/api/v0/auth/claim", post(claim))
        .route("/api/v0/usage", put(upload))
        .route("/api/v0/me", delete(leave))
        .route("/api/v0/leaderboard", get(board))
        .with_state(lb)
}

/// The desktop's state: what it checks the answer by. Kept to characters that need no
/// escaping and long enough not to be guessed.
fn valid_state(s: &str) -> bool {
    (16..=128).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Deserialize)]
struct Start {
    port: u16,
    state: String,
}

async fn start(State(lb): State<Arc<Leaderboard>>, Query(q): Query<Start>) -> Response {
    if q.port == 0 || !valid_state(&q.state) {
        return (StatusCode::BAD_REQUEST, "This sign-in link is broken.").into_response();
    }
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", &lb.forge.client_id)
        .append_pair("redirect_uri", &lb.callback())
        .append_pair("scope", "profile")
        // The forge hands it back to `callback` as is.
        .append_pair("state", &format!("{}.{}", q.port, q.state))
        .finish();
    Redirect::to(&format!("{}/oauth/authorize?{query}", lb.forge.url)).into_response()
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    #[serde(default)]
    state: String,
    error: Option<String>,
}

/// The forge's answer: the player signed in (or not), and the browser on to the desktop
/// with a code to claim the account with, or the error.
async fn callback(State(lb): State<Arc<Leaderboard>>, Query(q): Query<Callback>) -> Response {
    let parsed = q.state.split_once('.').and_then(|(port, state)| Some((port.parse::<u16>().ok()?, state)));
    let Some((port, state)) = parsed.filter(|(port, state)| *port != 0 && valid_state(state)) else {
        return (StatusCode::BAD_REQUEST, "This sign-in went astray: start it again from Dirigent.").into_response();
    };
    let back = |pair: (&str, &str)| {
        let query = form_urlencoded::Serializer::new(String::new()).append_pair("state", state).append_pair(pair.0, pair.1).finish();
        Redirect::to(&format!("http://127.0.0.1:{port}/?{query}")).into_response()
    };
    let Some(code) = q.code else {
        return back(("error", q.error.as_deref().unwrap_or("no code")));
    };
    let signing = lb.clone();
    match tokio::task::spawn_blocking(move || signing.sign_in(&code)).await {
        Ok(Ok(account)) => {
            let code = uuid::Uuid::new_v4().simple().to_string();
            let mut claims = lb.claims.lock().unwrap();
            claims.retain(|_, (at, _)| at.elapsed() < CLAIM_FOR);
            claims.insert(code.clone(), (Instant::now(), account));
            back(("code", &code))
        }
        Ok(Err(e)) => {
            error!("sign-in: {e:#}");
            back(("error", "server_error"))
        }
        Err(e) => {
            error!("sign-in: {e}");
            back(("error", "server_error"))
        }
    }
}

#[derive(Deserialize)]
struct Claim {
    code: String,
}

async fn claim(State(lb): State<Arc<Leaderboard>>, Json(c): Json<Claim>) -> Response {
    match lb.claims.lock().unwrap().remove(&c.code) {
        Some((at, account)) if at.elapsed() < CLAIM_FOR => ([(header::CACHE_CONTROL, "no-store")], Json(account)).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 204 once saved; 409 when the service is another player's (or one too many).
async fn upload(State(lb): State<Arc<Leaderboard>>, headers: HeaderMap, Json(up): Json<Upload>) -> Response {
    let user = match lb.player(&headers) {
        Ok(user) => user,
        Err(status) => return status.into_response(),
    };
    if up.service.is_empty() || up.service.len() > 64 || up.hours.len() > MAX_HOURS {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match lb.store.lock().unwrap().upload(&user, &up, now_ms()) {
        Ok(Uploaded::Saved(hours)) => {
            debug!(%user, service = %up.service, hours, "usage saved");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Uploaded::Refused) => StatusCode::CONFLICT.into_response(),
        Err(e) => internal(e),
    }
}

async fn leave(State(lb): State<Arc<Leaderboard>>, headers: HeaderMap) -> Response {
    let user = match lb.player(&headers) {
        Ok(user) => user,
        Err(status) => return status.into_response(),
    };
    if let Err(e) = lb.store.lock().unwrap().leave(&user) {
        return internal(e);
    }
    lb.cache.lock().unwrap().clear();
    info!(%user, "left");
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct BoardQuery {
    #[serde(default)]
    range: String,
}

async fn board(State(lb): State<Arc<Leaderboard>>, Query(q): Query<BoardQuery>) -> Response {
    let range = if q.range.is_empty() { Some(Range::default()) } else { Range::parse(&q.range) };
    let Some(range) = range else { return StatusCode::BAD_REQUEST.into_response() };
    let cached = lb.cache.lock().unwrap().get(&range).filter(|(at, _)| at.elapsed() < CACHE_FOR).map(|(_, json)| json.clone());
    let json = match cached {
        Some(json) => json,
        None => {
            let made = lb.store.lock().unwrap().board(range, now_ms()).and_then(|b| Ok(serde_json::to_string(&b)?));
            match made {
                Ok(json) => {
                    lb.cache.lock().unwrap().insert(range, (Instant::now(), json.clone()));
                    json
                }
                Err(e) => return internal(e),
            }
        }
    };
    ([(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, "no-cache")], json).into_response()
}

fn internal(e: anyhow::Error) -> Response {
    error!("{e:#}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn now_ms() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}
