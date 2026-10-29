//! dirigent_api: the update api at `https://dirigent.sebba.dev/api/v0/` (run behind a TLS
//! proxy), and the `publish` command the release workflow uploads a release with.
//!
//! Releases are files in a public S3 bucket (`bucket.rs`): `publish` is its only writer and
//! needs the bucket's keys; `serve` only reads, anonymously, so the server holds no secrets.
//!
//! ```text
//! dirigent_api serve
//! dirigent_api publish --channel beta --platform windows-x86_64 --version 0.1.0 [--notes TEXT] FILE
//! dirigent_api keygen FILE     a new signing key: the secret one to FILE, the public one printed
//! ```
//!
//! Environment: `S3_ENDPOINT` (`https://fsn1.your-objectstorage.com`), `S3_REGION`,
//! `S3_BUCKET`, `S3_PREFIX` (optional, e.g. `test/` to keep tests away from real releases);
//! `publish` also `S3_ACCESS_KEY_ID`, `S3_SECRET_ACCESS_KEY` and `DIRIGENT_SIGNING_KEY` (the
//! minisign secret key file's text); `serve` takes `DIRIGENT_API_BIND` (default
//! `127.0.0.1:7790`). `DIRIGENT_LOG` sets the log filter (default `info`).

mod bucket;
mod publish;
mod serve;

use anyhow::{Context, Result, bail};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: dirigent_api serve\n       dirigent_api publish --channel C --platform P --version V [--notes TEXT] FILE\n       dirigent_api keygen FILE";

#[tokio::main]
async fn main() {
    let filter = EnvFilter::try_from_env("DIRIGENT_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init();
    if let Err(e) = run().await {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next();
    if command.as_deref() == Some("keygen") {
        return publish::keygen(args.next().context(USAGE)?.as_ref());
    }
    let bucket = bucket::Bucket::from_env()?;
    match command.as_deref() {
        Some("serve") => {
            let bind = std::env::var("DIRIGENT_API_BIND").unwrap_or_else(|_| "127.0.0.1:7790".into());
            serve::run(bucket, bind.parse().with_context(|| format!("DIRIGENT_API_BIND {bind}"))?).await
        }
        Some("publish") => {
            let args = publish::Args::parse(args)?;
            tokio::task::spawn_blocking(move || publish::run(&bucket, args)).await?
        }
        _ => bail!("{USAGE}"),
    }
}
