//! The api: what the desktop's update check asks, and the download link for testers.
//!
//! ```text
//! GET /api/v0/updates/{channel}/{platform}    the channel's `Release` (JSON), 404 if none
//! GET /api/v0/download/{channel}/{platform}   307 to that release's installer
//! ```
//!
//! Both read the channel file from the bucket, cached for a minute: publishing shows up
//! within one desktop check interval.

use crate::bucket::{self, Bucket};
use anyhow::{Result, anyhow};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use dirigent_protocol::release::{Release, valid_name};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{error, info};

const CACHE_FOR: Duration = Duration::from_secs(60);

struct Api {
    bucket: Bucket,
    /// Channel file path → when it was read and what it said (`None`: no release).
    cache: Mutex<HashMap<String, (Instant, Option<Release>)>>,
}

pub async fn run(bucket: Bucket, bind: SocketAddr) -> Result<()> {
    let api = Arc::new(Api { bucket, cache: Mutex::default() });
    let app = Router::new()
        .route("/api/v0/updates/{channel}/{platform}", get(updates))
        .route("/api/v0/download/{channel}/{platform}", get(download))
        .with_state(api);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("listening on http://{bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn updates(State(api): State<Arc<Api>>, Path((channel, platform)): Path<(String, String)>, headers: HeaderMap) -> Response {
    // The desktop's user agent is `Dirigent/{version}`: who runs what, from the log.
    let client = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("-");
    info!(%channel, %platform, %client, "update check");
    match current(&api, &channel, &platform).await {
        Ok(Some(release)) => ([(header::CACHE_CONTROL, "no-cache")], Json(release)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(status) => status.into_response(),
    }
}

async fn download(State(api): State<Arc<Api>>, Path((channel, platform)): Path<(String, String)>) -> Response {
    match current(&api, &channel, &platform).await {
        Ok(Some(release)) => Redirect::temporary(&release.url).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(status) => status.into_response(),
    }
}

/// The channel's release, from the cache or the bucket. A failed read is a 502.
async fn current(api: &Arc<Api>, channel: &str, platform: &str) -> Result<Option<Release>, StatusCode> {
    if !valid_name(channel) || !valid_name(platform) {
        return Ok(None);
    }
    let path = bucket::channel_path(channel, platform);
    if let Some((at, release)) = api.cache.lock().unwrap().get(&path)
        && at.elapsed() < CACHE_FOR
    {
        return Ok(release.clone());
    }
    let url = api.bucket.url(&path);
    let read = tokio::task::spawn_blocking(move || fetch(&url)).await.map_err(|e| anyhow!("{e}")).and_then(|r| r);
    match read {
        Ok(release) => {
            api.cache.lock().unwrap().insert(path, (Instant::now(), release.clone()));
            Ok(release)
        }
        Err(e) => {
            error!("read {path}: {e:#}");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

/// GET a channel file. Missing reads as 403 (listing isn't public) or 404.
fn fetch(url: &str) -> Result<Option<Release>> {
    match ureq::get(url).call() {
        Ok(mut res) => Ok(Some(serde_json::from_str(&res.body_mut().read_to_string()?)?)),
        Err(ureq::Error::StatusCode(403 | 404)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}
