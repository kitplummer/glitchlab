use anyhow::Result;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    // Basic logging for standalone server
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let repo_path = PathBuf::from(".");
    let port = 3000;
    tracing::info!("Starting dashboard on http://localhost:{}", port);
    glitchlab_dashboard::start_server(repo_path, port).await
}
