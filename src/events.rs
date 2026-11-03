//! In-process event fan-out for the board's live updates.
//!
//! The SSE layer fans out events published by the API. Each board mutation
//! publishes one event after its transaction commits (the handlers are added
//! with the board API); subscribers are independent broadcast receivers, so a
//! slow consumer is marked lagged instead of blocking the publisher.

use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast;

/// Keep idle proxies and browsers from timing out (nginx defaults, PLAN §13).
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(25);

/// Enough room for a busy board while keeping memory bounded. A subscriber
/// that falls further behind is disconnected so it can reconnect and refetch.
pub const EVENT_BUFFER: usize = 256;

/// One event as sent to clients: `{"id": 1, "type": "task.created", "data": {}}`.
#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub id: u64,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Value,
}

/// Monotonic sequence plus the broadcast sender shared by every handler.
///
/// The sequence is process-local and restarts at 1 after a restart. Clients
/// therefore treat `id` as an ordering hint only and refetch on reconnect.
pub struct EventBus {
    sender: broadcast::Sender<Event>,
    sequence: Mutex<u64>,
    heartbeat: Duration,
}

impl EventBus {
    pub fn new(heartbeat: Duration) -> Self {
        let (sender, _) = broadcast::channel(EVENT_BUFFER);
        Self {
            sender,
            sequence: Mutex::new(0),
            heartbeat,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    pub fn heartbeat(&self) -> Duration {
        self.heartbeat
    }

    /// Assign the next sequence number and fan the event out.
    ///
    /// Allocation and `send` happen under the same lock so subscribers see
    /// event IDs in strictly increasing order. `broadcast::Sender::send` never
    /// waits for receivers: with none connected it reports an error, which is
    /// expected and ignored.
    pub fn publish(&self, kind: &str, data: Value) -> Event {
        let mut sequence = self
            .sequence
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *sequence += 1;
        let event = Event {
            id: *sequence,
            kind: kind.to_owned(),
            data,
        };
        let _ = self.sender.send(event.clone());
        event
    }

    pub fn last_id(&self) -> u64 {
        *self
            .sequence
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sequence_numbers_are_monotonic_and_start_at_one() {
        let bus = EventBus::new(DEFAULT_HEARTBEAT);
        assert_eq!(bus.last_id(), 0);

        let first = bus.publish("task.created", json!({"reference": "ALP-1"}));
        let second = bus.publish("task.moved", json!({"reference": "ALP-1"}));
        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
        assert_eq!(bus.last_id(), 2);
        assert_eq!(first.kind, "task.created");
    }

    #[test]
    fn publishing_without_receivers_does_not_block_or_advance_twice() {
        let bus = EventBus::new(DEFAULT_HEARTBEAT);
        for expected in 1..=10 {
            assert_eq!(bus.publish("task.updated", json!({})).id, expected);
        }
        assert_eq!(bus.last_id(), 10);
    }

    #[tokio::test]
    async fn subscribers_receive_the_same_event_ids() {
        let bus = EventBus::new(DEFAULT_HEARTBEAT);
        let mut first = bus.subscribe();
        let mut second = bus.subscribe();

        let published = bus.publish("task.claimed", json!({"claimed_by": "agent"}));
        let from_first = first.recv().await.expect("first event");
        let from_second = second.recv().await.expect("second event");
        assert_eq!(from_first.id, published.id);
        assert_eq!(from_second.id, published.id);
        assert_eq!(from_first.data["claimed_by"], "agent");
    }

    #[tokio::test]
    async fn a_late_subscriber_only_sees_later_events() {
        let bus = EventBus::new(DEFAULT_HEARTBEAT);
        bus.publish("task.created", json!({}));
        let mut receiver = bus.subscribe();
        let next = bus.publish("task.updated", json!({}));
        assert_eq!(receiver.recv().await.expect("event").id, next.id);
    }

    #[test]
    fn concurrent_publishers_keep_broadcast_ids_in_order() {
        use std::sync::Arc;

        let bus = Arc::new(EventBus::new(DEFAULT_HEARTBEAT));
        let mut receiver = bus.subscribe();
        let mut threads = Vec::new();
        for _ in 0..8 {
            let bus = Arc::clone(&bus);
            threads.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    bus.publish("task.updated", json!({}));
                }
            }));
        }
        for thread in threads {
            thread.join().expect("publisher thread");
        }

        let ids: Vec<u64> = (0..160)
            .map(|_| receiver.try_recv().expect("broadcast event").id)
            .collect();
        assert_eq!(ids, (1..=160).collect::<Vec<_>>());
        assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
