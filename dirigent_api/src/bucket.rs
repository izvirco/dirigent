//! The release bucket, and where things live in it (all under `S3_PREFIX`):
//!
//! ```text
//! releases/{version}/{file}             an installer, never overwritten
//! releases/{version}/{file}.minisig     its signature
//! channels/{channel}/{platform}.json    the channel's current `Release`, written last
//! ```
//!
//! The bucket is public: objects are read with plain GETs and their urls go to clients as
//! they are. Writes are signed with the S3 keys (`rusty-s3` signs, `ureq` sends).

use anyhow::{Context, Result, anyhow, bail};
use rusty_s3::{Credentials, S3Action, UrlStyle};
use std::time::Duration;

/// How long a signed request stays valid; it is sent right away.
const SIGNED_FOR: Duration = Duration::from_secs(600);

pub struct Bucket {
    bucket: rusty_s3::Bucket,
    prefix: String,
}

impl Bucket {
    pub fn from_env() -> Result<Self> {
        let endpoint = env("S3_ENDPOINT")?;
        let endpoint = endpoint.parse().with_context(|| format!("S3_ENDPOINT {endpoint} is not a url (https://…)"))?;
        let bucket = rusty_s3::Bucket::new(endpoint, UrlStyle::VirtualHost, env("S3_BUCKET")?, env("S3_REGION")?)?;
        Ok(Self { bucket, prefix: std::env::var("S3_PREFIX").unwrap_or_default() })
    }

    /// The keys from the environment; only writes need them.
    pub fn credentials() -> Result<Credentials> {
        Ok(Credentials::new(env("S3_ACCESS_KEY_ID")?, env("S3_SECRET_ACCESS_KEY")?))
    }

    fn key(&self, path: &str) -> String {
        format!("{}{path}", self.prefix)
    }

    /// The public url of `path`.
    pub fn url(&self, path: &str) -> String {
        self.bucket.object_url(&self.key(path)).to_string()
    }

    /// Whether `path` exists. Signed: anonymously the bucket answers 403 for a missing
    /// object (listing isn't public), which looks like any other refusal.
    pub fn exists(&self, creds: &Credentials, path: &str) -> Result<bool> {
        let key = self.key(path);
        let url = self.bucket.head_object(Some(creds), &key).sign(SIGNED_FOR);
        match ureq::head(url.as_str()).call() {
            Ok(_) => Ok(true),
            Err(ureq::Error::StatusCode(404)) => Ok(false),
            Err(e) => Err(anyhow!("HEAD {key}: {e}")),
        }
    }

    pub fn put(&self, creds: &Credentials, path: &str, body: &[u8], content_type: &str, cache_control: &str) -> Result<()> {
        let key = self.key(path);
        let url = self.bucket.put_object(Some(creds), &key).sign(SIGNED_FOR);
        ureq::put(url.as_str())
            .header("content-type", content_type)
            .header("cache-control", cache_control)
            .send(body)
            .map_err(|e| anyhow!("PUT {key}: {e}"))?;
        Ok(())
    }
}

/// `channels/{channel}/{platform}.json`.
pub fn channel_path(channel: &str, platform: &str) -> String {
    format!("channels/{channel}/{platform}.json")
}

fn env(name: &str) -> Result<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Ok(v),
        _ => bail!("{name} is not set"),
    }
}
