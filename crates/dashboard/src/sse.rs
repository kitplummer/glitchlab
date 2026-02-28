//! Server-Sent Events (SSE) logic for live updates.
use axum::{
    Router,
    response::sse::{Event, Sse},
    routing::get,
};
use futures_util::stream::{self, Stream};
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::StreamExt as _;

async fn sse_handler() -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = stream::repeat_with(|| Event::default().data("hello world"))
        .map(Ok)
        .throttle(Duration::from_secs(1));

    Sse::new(stream)
}

pub fn router() -> Router {
    Router::new().route("/sse", get(sse_handler))
}
