mod bucket;
mod leaderboard;
mod publish;
mod serve;
mod store;

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
            let board = leaderboard::Leaderboard::from_env()?;
            serve::run(bucket, bind.parse().with_context(|| format!("DIRIGENT_API_BIND {bind}"))?, board).await
        }
        Some("publish") => {
            let args = publish::Args::parse(args)?;
            tokio::task::spawn_blocking(move || publish::run(&bucket, args)).await?
        }
        _ => bail!("{USAGE}"),
    }
}
