pub mod batch;
pub mod common;
pub mod dashboard;
pub mod history;
pub mod init;
pub mod interactive;
pub mod run;
pub mod status;
pub mod version;

pub fn setup_logging(verbose: bool) {
    let filter = if verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
        )
        .with_target(false)
        .init();
}
