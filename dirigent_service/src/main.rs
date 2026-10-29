//! Standalone `dirigent_service` binary (for remote hosts). The desktop embeds the lib instead.
//! Takes `--data-dir` / `--config-dir` (see `dirigent_service::init_dirs`).

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dirigent_service::init_dirs()?;
    dirigent_service::init_logging("service");
    if let Err(e) = dirigent_service::run(dirigent_service::port()).await {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
    Ok(())
}
