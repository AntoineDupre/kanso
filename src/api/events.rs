use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderValue};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use futures_core::Stream;
use rusqlite::{params, OptionalExtension};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, Interval, MissedTickBehavior, Sleep};

use super::{now_millis, Auth};
use crate::auth::{Identity, IdentityKind};
use crate::events::Event;
use crate::server::AppState;

/// Per-connection buffer between the broadcast receiver task and the HTTP body.
const SSE_CHANNEL_BUFFER: usize = 64;

/// Bound the life of one connection even if nothing else closes it. A browser's
/// EventSource reconnects and re-authenticates when this elapses.
///
/// A client that stops reading entirely also stops hyper polling the body, so
/// no Rust future can observe it; that case is bounded by the proxy's
/// `send_timeout` / TCP timeouts in the documented deployment (PLAN §13).
const STREAM_LIFETIME: Duration = Duration::from_secs(30 * 60);

/// `GET /api/events` — Server-Sent Events for cookie or bearer identities.
///
/// The stream never touches the database; only the auth revalidation task does,
/// and it never holds the connection's lock while waiting for SQLite.
pub async fn stream(State(state): State<AppState>, auth: Auth) -> Response {
    let receiver = state.events.subscribe();
    let heartbeat = state.events.heartbeat();
    let (sender, channel) = mpsc::channel(SSE_CHANNEL_BUFFER);
    let (cancel, cancelled) = watch::channel(false);

    // A dedicated forwarding task keeps the broadcast receiver out of the HTTP
    // body. A slow client only applies backpressure to this task; publishers
    // never wait. If the broadcast channel laps the receiver, the connection is
    // closed so the browser reconnects and refetches the board.
    tokio::spawn(forward_events(receiver, sender, cancelled));
    // Logout, session expiry and token revocation must stop an existing stream
    // too, not just the next connection.
    tokio::spawn(revalidate_identity(
        state.clone(),
        auth.0,
        heartbeat,
        cancel,
    ));

    let mut response = Sse::new(EventStream::new(channel, heartbeat)).into_response();
    // nginx buffers proxied responses unless told otherwise (PLAN §13).
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

/// Re-check the session or token once per heartbeat interval.
async fn revalidate_identity(
    state: AppState,
    identity: Identity,
    interval: Duration,
    cancel: watch::Sender<bool>,
) {
    let mut ticker = tokio::time::interval_at(Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = cancel.closed() => return,
            _ = ticker.tick() => {
                if !identity_is_valid(&state, &identity).await {
                    let _ = cancel.send(true);
                    return;
                }
            }
        }
    }
}

async fn identity_is_valid(state: &AppState, identity: &Identity) -> bool {
    let now = now_millis();
    match identity.kind {
        IdentityKind::Ui => {
            let Some(session_id) = identity.session_id.clone() else {
                return false;
            };
            let result = state
                .db
                .call(move |connection| {
                    let valid: Option<i64> = connection
                        .query_row(
                            "SELECT 1 FROM ui_sessions WHERE id = ?1 AND expires_at > ?2",
                            params![session_id, now],
                            |row| row.get(0),
                        )
                        .optional()?;
                    Ok(valid.is_some())
                })
                .await;
            match result {
                Ok(valid) => valid,
                Err(error) => {
                    tracing::warn!(%error, "SSE session revalidation failed");
                    false
                }
            }
        }
        IdentityKind::Token => {
            let Some(token_id) = identity.token_id.clone() else {
                return false;
            };
            let result = state
                .db
                .call(move |connection| {
                    let valid: Option<i64> = connection
                        .query_row(
                            "SELECT 1 FROM tokens WHERE id = ?1 AND revoked_at IS NULL",
                            [token_id],
                            |row| row.get(0),
                        )
                        .optional()?;
                    Ok(valid.is_some())
                })
                .await;
            match result {
                Ok(valid) => valid,
                Err(error) => {
                    tracing::warn!(%error, "SSE token revalidation failed");
                    false
                }
            }
        }
    }
}

async fn forward_events(
    mut receiver: tokio::sync::broadcast::Receiver<Event>,
    sender: mpsc::Sender<Event>,
    mut cancel: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;
            // The HTTP body was dropped: stop holding a broadcast receiver.
            _ = sender.closed() => break,
            // Auth was revoked or the guard task ended.
            _ = cancel.changed() => break,
            result = receiver.recv() => match result {
                Ok(event) => {
                    // A stalled client is disconnected rather than parking this
                    // task forever and falling behind silently.
                    if sender.try_send(event).is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "SSE consumer lagged; closing the stream");
                    break;
                }
                Err(RecvError::Closed) => break,
            },
        }
    }
}

struct EventStream {
    receiver: mpsc::Receiver<Event>,
    heartbeat: Interval,
    deadline: Pin<Box<Sleep>>,
    finished: bool,
}

impl EventStream {
    fn new(receiver: mpsc::Receiver<Event>, heartbeat: Duration) -> Self {
        let mut heartbeat = tokio::time::interval_at(Instant::now() + heartbeat, heartbeat);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self {
            receiver,
            heartbeat,
            deadline: Box::pin(tokio::time::sleep(STREAM_LIFETIME)),
            finished: false,
        }
    }
}

impl Stream for EventStream {
    type Item = Result<SseEvent, Infallible>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }

        if this.deadline.as_mut().poll(cx).is_ready() {
            this.finished = true;
            return Poll::Ready(None);
        }

        match this.receiver.poll_recv(cx) {
            Poll::Ready(Some(event)) => Poll::Ready(Some(Ok(to_sse(event)))),
            Poll::Ready(None) => {
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Pending => match this.heartbeat.poll_tick(cx) {
                Poll::Ready(_) => Poll::Ready(Some(Ok(SseEvent::default().comment("heartbeat")))),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

fn to_sse(event: Event) -> SseEvent {
    let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_owned());
    SseEvent::default()
        .event(event.kind)
        .id(event.id.to_string())
        .data(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{EventBus, EVENT_BUFFER};
    use serde_json::json;

    fn cancellation() -> (watch::Sender<bool>, watch::Receiver<bool>) {
        watch::channel(false)
    }

    #[tokio::test]
    async fn a_lagged_receiver_closes_the_forwarder() {
        let bus = EventBus::new(Duration::from_secs(25));
        let receiver = bus.subscribe();
        for index in 0..(EVENT_BUFFER + 8) {
            bus.publish("task.updated", json!({ "index": index }));
        }

        let (_cancel, cancelled) = cancellation();
        let (sender, mut output) = mpsc::channel(8);
        let forwarder = tokio::spawn(forward_events(receiver, sender, cancelled));
        assert!(output.recv().await.is_none());
        forwarder.await.expect("forwarder task");
    }

    #[tokio::test]
    async fn forwarder_stops_when_the_body_is_dropped() {
        let bus = EventBus::new(Duration::from_secs(25));
        let receiver = bus.subscribe();
        let (_cancel, cancelled) = cancellation();
        let (sender, output) = mpsc::channel(8);
        let forwarder = tokio::spawn(forward_events(receiver, sender, cancelled));
        drop(output);
        tokio::time::timeout(Duration::from_secs(1), forwarder)
            .await
            .expect("forwarder should stop")
            .expect("forwarder task");
    }

    #[tokio::test]
    async fn forwarder_stops_when_the_channel_is_full() {
        let bus = EventBus::new(Duration::from_secs(25));
        let receiver = bus.subscribe();
        bus.publish("task.updated", json!({"index": 0}));
        bus.publish("task.updated", json!({"index": 1}));

        let (_cancel, cancelled) = cancellation();
        let (sender, mut output) = mpsc::channel(1);
        let forwarder = tokio::spawn(forward_events(receiver, sender, cancelled));
        assert!(output.recv().await.is_some());
        assert!(output.recv().await.is_none());
        forwarder.await.expect("forwarder task");
    }

    #[tokio::test]
    async fn forwarder_stops_when_cancelled() {
        let bus = EventBus::new(Duration::from_secs(25));
        let receiver = bus.subscribe();
        let (cancel, cancelled) = cancellation();
        let (sender, mut output) = mpsc::channel(8);
        let forwarder = tokio::spawn(forward_events(receiver, sender, cancelled));
        cancel.send(true).expect("cancel stream");
        assert!(output.recv().await.is_none());
        forwarder.await.expect("forwarder task");
    }
}
