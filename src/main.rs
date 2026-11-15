use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    teams_cli::updater::cleanup_stale_old_binary();
    teams_cli::cli::run_from_env().await
}
