//! Axum implementation of Dirigent's version API.

use std::{
    collections::{HashMap, HashSet},
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use aws_sdk_s3::{
    Client as S3Client,
    config::{Credentials, Region},
    primitives::ByteStream,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::{NaiveDateTime, SecondsFormat, Utc};
use semver::Version;
use sha2::{Digest, Sha256};
use tokio::{fs::File, io::AsyncWriteExt, net::TcpListener};

use crate::contract::{PublishVersion, VersionArtifact, VersionResponse};

const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_ARTIFACTS: usize = 16;
const S3_PREFIX: &str = "dist/dirigent";

#[derive(Clone)]
pub struct ServiceState {
    inner: Arc<ServiceStateInner>,
}

struct ServiceStateInner {
    s3: S3Client,
    bucket: String,
    cdn_url: String,
    publish_token: String,
    max_artifact_bytes: u64,
}

impl ServiceState {
    pub fn new(
        s3: S3Client,
        bucket: String,
        cdn_url: String,
        publish_token: String,
        max_artifact_bytes: u64,
    ) -> Self {
        Self {
            inner: Arc::new(ServiceStateInner {
                s3,
                bucket,
                cdn_url: cdn_url.trim_end_matches('/').to_string(),
                publish_token,
                max_artifact_bytes,
            }),
        }
    }

    async fn read_release(&self, key: &str) -> Result<Option<VersionResponse>, ApiError> {
        let output = match self
            .inner
            .s3
            .get_object()
            .bucket(&self.inner.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(output) => output,
            Err(error)
                if error
                    .as_service_error()
                    .is_some_and(|error| error.is_no_such_key()) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(internal(error)),
        };
        let bytes = output.body.collect().await.map_err(internal)?.into_bytes();
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| internal(format!("invalid release metadata in S3: {error}")))
    }

    fn populate_urls(&self, release: &mut VersionResponse) {
        for artifact in &mut release.artifacts {
            artifact.url = format!(
                "{}/{}",
                self.inner.cdn_url,
                artifact_key(
                    &release.channel,
                    &release.version,
                    &artifact.target,
                    &artifact.file_name,
                )
            );
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        message: message.into(),
    }
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(%error, "version API request failed");
    ApiError {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        message: "internal server error".into(),
    }
}

fn validate_component(value: &str, label: &str) -> Result<(), ApiError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.+".contains(&byte));
    if valid {
        Ok(())
    } else {
        Err(bad_request(format!("invalid {label}")))
    }
}

fn validate_version(channel: &str, version: &str) -> Result<(), ApiError> {
    validate_component(version, "version")?;
    match channel {
        "stable" => {
            let version = Version::parse(version)
                .map_err(|error| bad_request(format!("invalid stable SemVer: {error}")))?;
            if !version.pre.is_empty() || !version.build.is_empty() {
                return Err(bad_request(
                    "stable versions cannot contain prerelease or build metadata",
                ));
            }
        }
        _ => {
            NaiveDateTime::parse_from_str(version, "%Y%m%dT%H%M%SZ")
                .map_err(|_| bad_request("branch versions must use YYYYMMDDTHHMMSSZ UTC"))?;
        }
    }
    Ok(())
}

fn ensure_newer(channel: &str, next: &str, current: &str) -> Result<(), ApiError> {
    let newer = match channel {
        "stable" => {
            Version::parse(next).map_err(internal)? > Version::parse(current).map_err(internal)?
        }
        _ => next > current,
    };
    if newer {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::CONFLICT,
            message: format!("{channel} version {next} is not newer than {current}"),
        })
    }
}

fn release_manifest_key(channel: &str, version: &str) -> String {
    format!("{S3_PREFIX}/releases/{channel}/{version}/manifest.json")
}

fn latest_key(channel: &str) -> String {
    format!("{S3_PREFIX}/channels/{channel}/latest.json")
}

fn artifact_key(channel: &str, version: &str, target: &str, file_name: &str) -> String {
    format!("{S3_PREFIX}/releases/{channel}/{version}/{target}/{file_name}")
}

pub fn router(state: ServiceState) -> Router {
    let request_limit = state
        .inner
        .max_artifact_bytes
        .saturating_mul(MAX_ARTIFACTS as u64)
        .saturating_add(2 * 1024 * 1024)
        .min(usize::MAX as u64) as usize;
    Router::new()
        .route("/health", get(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/api/v0/version/{channel}",
            get(get_version).post(post_version),
        )
        .route(
            "/releases/{channel}/latest/{target}",
            get(get_latest_download),
        )
        .layer(DefaultBodyLimit::max(request_limit))
        .with_state(state)
}

async fn get_version(
    State(state): State<ServiceState>,
    Path(channel): Path<String>,
) -> Result<Json<VersionResponse>, ApiError> {
    validate_component(&channel, "release channel").map_err(|_| ApiError {
        status: StatusCode::NOT_FOUND,
        message: "unknown release channel".into(),
    })?;
    let mut release = state
        .read_release(&latest_key(&channel))
        .await?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: "this channel has no releases".into(),
        })?;
    state.populate_urls(&mut release);
    Ok(Json(release))
}

async fn get_latest_download(
    State(state): State<ServiceState>,
    Path((channel, target)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    validate_component(&channel, "release channel").map_err(|_| ApiError {
        status: StatusCode::NOT_FOUND,
        message: "unknown release channel".into(),
    })?;
    let mut release = state
        .read_release(&latest_key(&channel))
        .await?
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: "this channel has no releases".into(),
        })?;
    state.populate_urls(&mut release);

    validate_component(&target, "artifact target")?;
    let artifact = release
        .artifacts
        .iter()
        .find(|artifact| artifact.target == target)
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("release has no {target} artifact"),
        })?;

    let mut response = axum::response::Redirect::temporary(&artifact.url).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

struct UploadedArtifact {
    target: String,
    file_name: String,
    path: PathBuf,
    size: u64,
    sha256: String,
}

async fn post_version(
    State(state): State<ServiceState>,
    headers: HeaderMap,
    Path(channel): Path<String>,
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<VersionResponse>), ApiError> {
    let expected = format!("Bearer {}", state.inner.publish_token);
    if headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        != Some(expected.as_str())
    {
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "invalid publishing token".into(),
        });
    }
    validate_component(&channel, "release channel")?;

    let temporary = tempfile::tempdir().map_err(internal)?;
    let mut manifest = None;
    let mut uploads = HashMap::new();
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|error| bad_request(error.to_string()))?
    {
        let name = field
            .name()
            .ok_or_else(|| bad_request("multipart field has no name"))?
            .to_string();
        if name == "manifest" {
            if manifest.is_some() {
                return Err(bad_request("duplicate manifest field"));
            }
            let bytes = field
                .bytes()
                .await
                .map_err(|error| bad_request(error.to_string()))?;
            if bytes.len() > 1024 * 1024 {
                return Err(bad_request("manifest is too large"));
            }
            manifest = Some(
                serde_json::from_slice::<PublishVersion>(&bytes)
                    .map_err(|error| bad_request(format!("invalid manifest: {error}")))?,
            );
            continue;
        }
        if uploads.len() >= MAX_ARTIFACTS {
            return Err(bad_request("too many artifacts"));
        }
        validate_component(&name, "artifact target")?;
        let file_name = field
            .file_name()
            .ok_or_else(|| bad_request("artifact has no file name"))?
            .to_string();
        validate_component(&file_name, "artifact file name")?;
        if uploads.contains_key(&name) {
            return Err(bad_request(format!("duplicate artifact {name}")));
        }

        let path = temporary.path().join(format!("upload-{}", uploads.len()));
        let mut file = File::create(&path).await.map_err(internal)?;
        let mut size = 0_u64;
        let mut hasher = Sha256::new();
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|error| bad_request(error.to_string()))?
        {
            size = size.saturating_add(chunk.len() as u64);
            if size > state.inner.max_artifact_bytes {
                return Err(ApiError {
                    status: StatusCode::PAYLOAD_TOO_LARGE,
                    message: format!("artifact {name} is too large"),
                });
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(internal)?;
        }
        file.flush().await.map_err(internal)?;
        uploads.insert(
            name.clone(),
            UploadedArtifact {
                target: name,
                file_name,
                path,
                size,
                sha256: format!("{:x}", hasher.finalize()),
            },
        );
    }

    let manifest = manifest.ok_or_else(|| bad_request("missing manifest field"))?;
    validate_version(&channel, &manifest.version)?;
    validate_component(&manifest.commit, "commit")?;
    if manifest.artifacts.is_empty() {
        return Err(bad_request("a release must contain at least one artifact"));
    }
    let mut declared = HashSet::new();
    for artifact in &manifest.artifacts {
        validate_component(&artifact.target, "artifact target")?;
        validate_component(&artifact.file_name, "artifact file name")?;
        if !declared.insert(&artifact.target) {
            return Err(bad_request(format!(
                "duplicate declared artifact {}",
                artifact.target
            )));
        }
        let upload = uploads
            .get(&artifact.target)
            .ok_or_else(|| bad_request(format!("missing artifact {}", artifact.target)))?;
        if upload.file_name != artifact.file_name {
            return Err(bad_request(format!(
                "file name for {} does not match the manifest",
                artifact.target
            )));
        }
    }
    if uploads.len() != declared.len() {
        return Err(bad_request("request contains an undeclared artifact"));
    }

    let manifest_key = release_manifest_key(&channel, &manifest.version);
    match state
        .inner
        .s3
        .head_object()
        .bucket(&state.inner.bucket)
        .key(&manifest_key)
        .send()
        .await
    {
        Ok(_) => {
            return Err(ApiError {
                status: StatusCode::CONFLICT,
                message: format!(
                    "{} {} has already been published",
                    channel, manifest.version
                ),
            });
        }
        Err(error)
            if error
                .as_service_error()
                .is_some_and(|error| error.is_not_found()) => {}
        Err(error) => return Err(internal(error)),
    }
    if let Some(latest) = state.read_release(&latest_key(&channel)).await? {
        ensure_newer(&channel, &manifest.version, &latest.version)?;
    }

    let mut artifacts = Vec::with_capacity(manifest.artifacts.len());
    for declared in &manifest.artifacts {
        let upload = uploads.remove(&declared.target).expect("validated above");
        let key = artifact_key(
            &channel,
            &manifest.version,
            &upload.target,
            &upload.file_name,
        );
        let body = ByteStream::from_path(&upload.path)
            .await
            .map_err(internal)?;
        state
            .inner
            .s3
            .put_object()
            .bucket(&state.inner.bucket)
            .key(key)
            .body(body)
            .content_length(upload.size as i64)
            .content_type("application/octet-stream")
            .cache_control("public, max-age=31536000, immutable")
            .if_none_match("*")
            .send()
            .await
            .map_err(internal)?;
        artifacts.push(VersionArtifact {
            target: upload.target,
            file_name: upload.file_name,
            url: String::new(),
            size: upload.size,
            sha256: upload.sha256,
        });
    }

    let release = VersionResponse {
        channel: channel.clone(),
        version: manifest.version,
        commit: manifest.commit,
        published_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        artifacts,
    };
    let bytes = serde_json::to_vec(&release).map_err(internal)?;
    state
        .inner
        .s3
        .put_object()
        .bucket(&state.inner.bucket)
        .key(&manifest_key)
        .body(ByteStream::from(bytes.clone()))
        .content_type("application/json")
        .if_none_match("*")
        .send()
        .await
        .map_err(internal)?;
    state
        .inner
        .s3
        .put_object()
        .bucket(&state.inner.bucket)
        .key(latest_key(&channel))
        .body(ByteStream::from(bytes))
        .content_type("application/json")
        .send()
        .await
        .map_err(internal)?;

    let mut response = release;
    state.populate_urls(&mut response);
    Ok((StatusCode::CREATED, Json(response)))
}

pub async fn run_from_env() -> Result<(), String> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dirigent_server=info,tower_http=info".into()),
        )
        .init();
    let s3_url = required_env("DIRIGENT_S3_URL")?;
    let bucket = required_env("DIRIGENT_S3_BUCKET")?;
    let access_key = required_env("DIRIGENT_S3_ACCESS_KEY")?;
    let secret_key = required_env("DIRIGENT_S3_SECRET_KEY")?;
    let region = env::var("DIRIGENT_S3_REGION").unwrap_or_else(|_| "auto".into());
    let cdn_url = env::var("DIRIGENT_CDN_URL").unwrap_or_else(|_| "https://cdn.sebba.dev".into());
    let publish_token = required_env("DIRIGENT_PUBLISH_TOKEN")?;
    let bind = env::var("DIRIGENT_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let address = bind
        .parse::<SocketAddr>()
        .map_err(|error| format!("invalid DIRIGENT_BIND: {error}"))?;
    let max_artifact_bytes = env::var("DIRIGENT_MAX_ARTIFACT_BYTES")
        .ok()
        .map(|value| value.parse::<u64>())
        .transpose()
        .map_err(|error| format!("invalid DIRIGENT_MAX_ARTIFACT_BYTES: {error}"))?
        .unwrap_or(DEFAULT_MAX_ARTIFACT_BYTES);
    let s3_config = aws_sdk_s3::Config::builder()
        .behavior_version_latest()
        .endpoint_url(s3_url)
        .region(Region::new(region))
        .credentials_provider(Credentials::new(
            access_key, secret_key, None, None, "dirigent",
        ))
        .force_path_style(true)
        .build();
    let state = ServiceState::new(
        S3Client::from_conf(s3_config),
        bucket,
        cdn_url,
        publish_token,
        max_artifact_bytes,
    );
    let listener = TcpListener::bind(address)
        .await
        .map_err(|error| format!("could not bind {address}: {error}"))?;
    tracing::info!(%address, "Dirigent version API listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|error| format!("version API failed: {error}"))
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

fn required_env(name: &str) -> Result<String, String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} must be set"))
}
