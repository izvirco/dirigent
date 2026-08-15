//! Runs the Dirigent version service.

#[tokio::main]
async fn main() {
    if let Err(error) = dirigent_server::service::run_from_env().await {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
