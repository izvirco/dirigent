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

const CACHE_FOR: Duration = Duration::from_secs(60);

pub struct Leaderboard {
    store: Mutex<Store>,
    forge: dirigent_auth::Forge,
    keys: dirigent_auth::jwt::Keys,
    url: String,
    cache: Mutex<HashMap<Range, (Instant, String)>>,
}

impl Leaderboard {
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

async fn join(State(lb): State<Arc<Leaderboard>>, headers: HeaderMap) -> Response {
    let claims = match lb.forge_account(&headers).await {
        Ok(claims) => claims,
        Err(res) => return *res,
    };
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

fn answer(status: StatusCode, why: &str) -> Box<Response> {
    Box::new((status, why.to_owned()).into_response())
}

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
        assert_eq!(status(lb.player(&bearer(&jwt("u2", API, "dirigent"))).await), Some(StatusCode::FORBIDDEN));
        assert_eq!(status(lb.player(&bearer(&jwt("u1", "https://other.example", "dirigent"))).await), Some(StatusCode::UNAUTHORIZED));
        assert_eq!(status(lb.player(&bearer(&jwt("u1", API, "another-app"))).await), Some(StatusCode::UNAUTHORIZED));
        assert_eq!(status(lb.player(&bearer("0123abcd")).await), Some(StatusCode::UNAUTHORIZED));
        assert_eq!(status(lb.player(&HeaderMap::new()).await), Some(StatusCode::UNAUTHORIZED));

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
