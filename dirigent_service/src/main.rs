#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let operands = dirigent_service::init_dirs()?;
    match operands.first().map(String::as_str) {
        None => {}
        Some("key") => return dirigent_service::key_command(&operands[1..]),
        Some(cmd @ ("login" | "logout" | "whoami")) if operands.len() == 1 => return dirigent_service::forge::command(cmd),
        Some(arg) => anyhow::bail!(
            "unknown argument {arg}\n{}\ncommands: {} | {}",
            dirigent_service::USAGE,
            dirigent_service::KEY_USAGE,
            dirigent_service::forge::LOGIN_USAGE
        ),
    }
    dirigent_service::init_logging("service");
    tokio::select! {
        ran = dirigent_service::run(dirigent_service::port()) => {
            if let Err(e) = ran {
                tracing::error!("{e:#}");
                std::process::exit(1);
            }
        }
        () = told_to_stop() => {
            tracing::info!("told to stop");
            let _ = tokio::task::spawn_blocking(dirigent_service::shutdown).await;
        }
    }
    Ok(())
}

async fn told_to_stop() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                () = ctrl_c => {}
                _ = term.recv() => {}
            },
            Err(_) => ctrl_c.await,
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}
