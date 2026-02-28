use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct DashboardArgs {
    pub repo: PathBuf,
    pub port: u16,
}

pub async fn execute(args: DashboardArgs) -> Result<()> {
    tracing::info!("Starting dashboard on http://localhost:{}", args.port);
    glitchlab_dashboard::start_server(args.repo, args.port).await
}
