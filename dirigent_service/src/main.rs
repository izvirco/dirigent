//! Standalone `dirigent_service` binary (for remote hosts). The desktop embeds the lib instead.
//! Takes `--data-dir` / `--config-dir` (see `dirigent_service::init_dirs`), and
//! `key add <name> | key list | key revoke <name>` for the public listener's API keys
//! (`auth.rs`) instead of running.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let operands = dirigent_service::init_dirs()?;
    match operands.first().map(String::as_str) {
        None => {}
        Some("key") => return dirigent_service::key_command(&operands[1..]),
        Some(arg) => anyhow::bail!("unknown argument {arg}\n{}\ncommands: {}", dirigent_service::USAGE, dirigent_service::KEY_USAGE),
    }
    dirigent_service::init_logging("service");
    if let Err(e) = dirigent_service::run(dirigent_service::port()).await {
        tracing::error!("{e:#}");
        std::process::exit(1);
    }
    Ok(())
}
