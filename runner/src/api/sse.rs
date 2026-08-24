//! Live progress over Server-Sent Events.
//!
//! Two streams, one vocabulary: a per-run stream for a client following a
//! single run, and a global firehose the UI task list and menu bar subscribe to.
//! Event names are exactly the strings in `errand_core::models::Event`, which
//! are exactly what webhooks and Tauri events carry.

use axum::extract::{Path, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use futures::stream::Stream;
use std::convert::Infallible;
use std::time::Duration;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::state::AppState;

fn to_sse(ev: &errand_core::models::Event) -> Option<SseEvent> {
    let data = serde_json::to_value(ev).ok()?;
    // The wire payload is the `data` half; the event name rides the SSE
    // `event:` field so clients can dispatch without parsing the body.
    let payload = data.get("data").cloned().unwrap_or(serde_json::Value::Null);
    Some(
        SseEvent::default()
            .event(ev.name())
            .data(payload.to_string()),
    )
}

/// `GET /v1/events`: everything, for the task list and the menu bar.
pub async fn global_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| match res {
        Ok(ev) => to_sse(&ev).map(Ok),
        // A lagging subscriber has missed events; drop the notice rather than
        // killing the stream, since the client can re-read state from the API.
        Err(_) => None,
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

/// `GET /v1/runs/{id}/stream`: one run, closing itself when the run ends.
pub async fn run_stream(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.subscribe();
    let wanted = run_id.clone();
    let stream = BroadcastStream::new(rx).filter_map(move |res| {
        let ev = res.ok()?;
        if ev.run_id() == Some(wanted.as_str()) {
            to_sse(&ev).map(Ok)
        } else {
            None
        }
    });
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
