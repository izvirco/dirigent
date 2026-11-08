//! The leaderboard (on when `DIRIGENT_API_DB` is set): players join with their forge
//! account, their desktops upload usage (`store.rs` keeps it), and anyone can read the board.
//!
//! ```text
//! POST   /api/v0/join                          join as the bearer's forge account (or take its current name)
//! PUT    /api/v0/usage                         an `Upload`
//! DELETE /api/v0/me                            leave the board, usage and all
//! GET    /api/v0/leaderboard?range=1d|7d|30d|all   the `Board`, cached for a minute
//! ```
//!
//! The desktop is signed in to the forge (Dirigent's public app, `dirigent_service`'s
//! `forge.rs`), and the bearer of the first three is a JWT the forge made for this api:
//! `aud` is `DIRIGENT_API_URL` (no trailing slash) exactly, `client_id` Dirigent's app
//! (`DIRIGENT_FORGE_CLIENT_ID`, else `dirigent_auth::CLIENT_ID`), `iss` `FORGE_URL`. Its
//! `sub`, the forge user id, is the player; nothing is stored per desktop. A token the api
//! refuses is 401 (503 while the forge's keys can't be fetched: not the token's fault); a
//! valid one of someone not on the board, to upload or leave, 403, which the desktop takes
//! for "left from another desktop" and forgets its player.

use crate::store::{MAX_HOURS, Store, Uploaded};
use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use dirigent_auth::jwt::{Claims, Expect, Rejected};
use dirigent_protocol::leaderboard::{Range, Upload};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, error, info};

/// How long a board stays cached: uploads show up within a minute.
const CACHE_FOR: Duration = Duration::from_secs(60);

pub struct Leaderboard {
    store: Mutex<Store>,
    /// The forge and Dirigent's public app on it, whose JWTs (`aud`: `url`) name players.
    forge: dirigent_auth::Forge,
    /// The forge's signing keys, for those.
    keys: dirigent_auth::jwt::Keys,
    /// Where this api is reached (`DIRIGENT_API_URL`): the audience of the tokens it takes.
    url: String,
    /// Boards as served (JSON), with when they were made.
    cache: Mutex<HashMap<Range, (Instant, String)>>,
}

impl Leaderboard {
    /// From the environment: `DIRIGENT_API_DB` (the sqlite db), `FORGE_URL` (default
    /// `dirigent_auth::DEFAULT_URL`), `DIRIGENT_FORGE_CLIENT_ID` (Dirigent's app, default
    /// `dirigent_auth::CLIENT_ID`), `DIRIGENT_API_URL` (default `https://dirigent.sebba.dev`).
    /// `None` without a db: the api serves updates only.
    pub fn from_env() -> Result<Option<Arc<Self>>> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let Some(db) = var("DIRIGENT_API_DB").map(PathBuf::from) else { return Ok(None) };
        let store = Store::open(&db).with_context(|| format!("open {}", db.display()))?;
        let forge = dirigent_auth::Forge::new(
            &var("FORGE_URL").unwrap_or_else(|| dirigent_auth::DEFAULT_URL.into()),
            &var("DIRIGENT_FORGE_CLIENT_ID").unwrap_or_else(|| dirigent_auth::CLIENT_ID.into()),
        );
        let keys = dirigent_auth::jwt::Keys::new(forge.jwks_url());
        let url = var("DIRIGENT_API_URL").unwrap_or_else(|| "https://dirigent.sebba.dev".into()).trim_end_matches('/').to_owned();
        info!(forge = %forge.url, db = %db.display(), "leaderboard on");
        Ok(Some(Arc::new(Self::new(store, forge, keys, url))))
    }

    fn new(store: Store, forge: dirigent_auth::Forge, keys: dirigent_auth::jwt::Keys, url: String) -> Self {
        Self { store: Mutex::new(store), forge, keys, url, cache: Mutex::default() }
    }

    /// The bearer's forge account, from its JWT for this api (checked on a blocking thread:
    /// it may fetch the forge's keys). Else the answer: 401, or 503 without the forge's keys.
    async fn forge_account(self: &Arc<Self>, headers: &HeaderMap) -> Result<Claims, Box<Response>> {
        let Some(token) = bearer(headers) else { return Err(answer(StatusCode::UNAUTHORIZED, "no bearer token")) };
        let lb = self.clone();
        let checked = tokio::task::spawn_blocking(move || {
            let expect = Expect { issuer: &lb.forge.url, audience: &lb.url, client_id: &lb.forge.client_id, subject: None };
            lb.keys.verify(&token, &expect, now_ms() / 1000)
        });
        match checked.await {
            Ok(Ok(claims)) => Ok(claims),
            Ok(Err(why)) => Err(Box::new(refused(why))),
            Err(e) => Err(Box::new(internal(e.into()))),
        }
    }

    /// The bearer's player: its forge account, if on the board (else 403).
    async fn player(self: &Arc<Self>, headers: &HeaderMap) -> Result<String, Box<Response>> {
        let claims = self.forge_account(headers).await?;
        let joined = self.store.lock().unwrap().joined(&claims.sub);
        match joined {
            Ok(true) => Ok(claims.sub),
            Ok(false) => Err(answer(StatusCode::FORBIDDEN, "not on the leaderboard")),
            Err(e) => Err(Box::new(internal(e))),
        }
    }
}

/// A request's bearer token.
fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    Some(value.strip_prefix("Bearer ")?.trim().to_owned())
}

pub fn router(lb: Arc<Leaderboard>) -> Router {
    Router::new()
        .route("/api/v0/join", post(join))
        .route("/api/v0/usage", put(upload))
        .route("/api/v0/me", delete(leave))
        .route("/api/v0/leaderboard", get(board))
        .with_state(lb)
}

/// The bearer's forge account joins the board, or takes its current name and picture (a
/// player keeps their `joined_at`): its `Account`.
async fn join(State(lb): State<Arc<Leaderboard>>, headers: HeaderMap) -> Response {
    let claims = match lb.forge_account(&headers).await {
        Ok(claims) => claims,
        Err(res) => return *res,
    };
    // The forge's picture of the account: what its userinfo says too.
    let picture = format!("{}/api/users/{}/avatar", lb.forge.url, claims.sub);
    let joined = lb.store.lock().unwrap().join(&claims.sub, &claims.preferred_username, &picture, now_ms());
    match joined {
        Ok(account) => {
            lb.cache.lock().unwrap().clear();
            info!(player = %account.name, "joined");
            ([(header::CACHE_CONTROL, "no-store")], Json(account)).into_response()
        }
        Err(e) => internal(e),
    }
}

/// 204 once saved; 409 when the service is another player's (or one too many).
async fn upload(State(lb): State<Arc<Leaderboard>>, headers: HeaderMap, Json(up): Json<Upload>) -> Response {
    let user = match lb.player(&headers).await {
        Ok(user) => user,
        Err(res) => return *res,
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
    let user = match lb.player(&headers).await {
        Ok(user) => user,
        Err(res) => return *res,
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

/// `status` with why, as a `player` / `forge_account` error.
fn answer(status: StatusCode, why: &str) -> Box<Response> {
    Box::new((status, why.to_owned()).into_response())
}

/// A refused forge token: 401 with why, or 503 when the forge's keys can't be had (not the
/// token's fault: a desktop mustn't take it for being signed out).
fn refused(why: Rejected) -> Response {
    let status = if matches!(why, Rejected::Keys(_)) { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::UNAUTHORIZED };
    (status, why.to_string()).into_response()
}

fn internal(e: anyhow::Error) -> Response {
    error!("{e:#}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

fn now_ms() -> i64 {
    std::time::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dirigent_auth::testing::{Signer, claims};

    const FORGE: &str = "https://forge.example";
    const API: &str = "https://api.example";

    fn board(signer: &Signer) -> Arc<Leaderboard> {
        board_with(signer.jwks())
    }

    fn board_with(jwks: String) -> Arc<Leaderboard> {
        let keys =
            dirigent_auth::jwt::Keys::with_fetch(move || if jwks.is_empty() { anyhow::bail!("forge down") } else { Ok(jwks.clone()) });
        Arc::new(Leaderboard::new(Store::in_memory(), dirigent_auth::Forge::new(FORGE, "dirigent"), keys, API.into()))
    }

    fn bearer(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, format!("Bearer {token}").parse().unwrap());
        h
    }

    #[tokio::test]
    async fn forge_tokens_name_players() {
        let signer = Signer::new();
        let lb = board(&signer);
        let exp = now_ms() / 1000 + 600;
        let jwt = |sub: &str, aud: &str, client: &str| signer.sign(&claims(FORGE, sub, aud, client, exp));
        lb.store.lock().unwrap().join("u1", "seb", "", 1).unwrap();
        assert_eq!(lb.player(&bearer(&jwt("u1", API, "dirigent"))).await.ok().as_deref(), Some("u1"));
        let status = |r: Result<String, Box<Response>>| r.err().map(|r| r.status());
        // A valid token of someone not on the board: 403, so the desktop knows it's not the
        // token. The wrong audience or app, an api token from before 0.0.7, none: 401.
        assert_eq!(status(lb.player(&bearer(&jwt("u2", API, "dirigent"))).await), Some(StatusCode::FORBIDDEN));
        assert_eq!(status(lb.player(&bearer(&jwt("u1", "https://other.example", "dirigent"))).await), Some(StatusCode::UNAUTHORIZED));
        assert_eq!(status(lb.player(&bearer(&jwt("u1", API, "another-app"))).await), Some(StatusCode::UNAUTHORIZED));
        assert_eq!(status(lb.player(&bearer("0123abcd")).await), Some(StatusCode::UNAUTHORIZED));
        assert_eq!(status(lb.player(&HeaderMap::new()).await), Some(StatusCode::UNAUTHORIZED));

        // Joining puts u2 on; a token for another audience can't join.
        assert_eq!(
            join(State(lb.clone()), bearer(&jwt("u2", "https://other.example", "dirigent"))).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(join(State(lb.clone()), bearer(&jwt("u2", API, "dirigent"))).await.status(), StatusCode::OK);
        assert_eq!(lb.player(&bearer(&jwt("u2", API, "dirigent"))).await.ok().as_deref(), Some("u2"));
    }

    #[tokio::test]
    async fn no_forge_keys_is_503_not_401() {
        let signer = Signer::new();
        let lb = board_with(String::new());
        let token = signer.sign(&claims(FORGE, "u1", API, "dirigent", now_ms() / 1000 + 600));
        let refused = lb.player(&bearer(&token)).await.err().map(|r| r.status());
        assert_eq!(refused, Some(StatusCode::SERVICE_UNAVAILABLE));
    }
}
