//! Standalone `dirigent_service` binary (for remote hosts). The desktop embeds the lib instead.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dirigent_service::run(dirigent_service::port()).await
}
