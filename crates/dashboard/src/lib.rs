use anyhow::Result;
use std::path::PathBuf;

mod data;
mod sse;

use askama::Template;
use askama_axum::IntoResponse;
use axum::{Router, routing::get};
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate;

async fn index() -> impl IntoResponse {
    IndexTemplate
}

pub async fn start_server(_repo_path: PathBuf, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .merge(sse::router())
        .layer(TraceLayer::new_for_http());

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    tracing::debug!("listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_start_server_does_not_fail() {
        let server_task = tokio::spawn(async {
            let result = start_server(PathBuf::from("/tmp/fake-repo"), 0).await;
            assert!(result.is_ok());
        });

        // Give the server a moment to start and then cancel it.
        tokio::time::sleep(Duration::from_millis(100)).await;
        server_task.abort();
    }
}
