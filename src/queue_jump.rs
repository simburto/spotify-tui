use serde_json::{json, Value};

/// Advance the existing player queue without starting a new playback context.
#[derive(Default)]
pub struct QueueJump {
    remaining: Option<usize>,
}

impl QueueJump {
    pub fn is_active(&self) -> bool {
        self.remaining.is_some()
    }

    pub fn cancel(&mut self) {
        self.remaining = None;
    }

    pub fn start(&mut self, index: usize) -> Value {
        // The first skip plays upcoming row 0; each further skip consumes one row.
        self.remaining = Some(index);
        json!({ "type": "command", "command": "skip_next" })
    }

    pub fn on_event(&mut self, event: &Value) -> Vec<Value> {
        let Some(remaining) = self.remaining else { return Vec::new() };
        match event.get("type").and_then(Value::as_str) {
            Some("error") => {
                log::warn!("Queue jump failed: {}", event);
                self.cancel();
                Vec::new()
            }
            // A command_result only acknowledges dispatch, not a completed skip.
            // Wait for the actual track change before issuing another skip.
            Some("track_changed") => {
                if remaining > 0 {
                    self.remaining = Some(remaining - 1);
                    vec![json!({ "type": "command", "command": "skip_next" })]
                } else {
                    self.cancel();
                    vec![
                        json!({ "type": "command", "command": "play" }),
                        json!({ "type": "command", "command": "get_queue", "limit": 100 }),
                    ]
                }
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_upcoming_track_requires_one_skip_then_resumes_without_uri() {
        let mut jump = QueueJump::default();
        assert_eq!(jump.start(0)["command"], "skip_next");
        assert!(jump.is_active());
        let payloads = jump.on_event(&json!({ "type": "track_changed" }));
        assert_eq!(payloads[0], json!({ "type": "command", "command": "play" }));
        assert_eq!(payloads[1]["command"], "get_queue");
        assert!(!jump.is_active());
    }

    #[test]
    fn jumps_by_position_even_when_track_uris_are_duplicates() {
        let mut jump = QueueJump::default();
        jump.start(3); // Two manual tracks, then the second context track.
        let event = json!({ "type": "track_changed", "item": { "uri": "same" } });
        for _ in 0..3 {
            assert!(jump.on_event(&json!({ "type": "command_result", "command": "skip_next" })).is_empty());
            assert!(jump.on_event(&json!({ "type": "queue_changed" })).is_empty());
            assert_eq!(jump.on_event(&event)[0]["command"], "skip_next");
        }
        assert_eq!(jump.on_event(&event)[0]["command"], "play");
        assert!(jump.on_event(&event).is_empty());
    }

    #[test]
    fn error_or_cancellation_stops_remaining_skips() {
        let mut jump = QueueJump::default();
        jump.start(4);
        assert!(jump.on_event(&json!({ "type": "error", "message": "not active" })).is_empty());
        assert!(!jump.is_active());
        assert!(jump.on_event(&json!({ "type": "track_changed" })).is_empty());
        jump.start(2);
        jump.cancel();
        assert!(jump.on_event(&json!({ "type": "track_changed" })).is_empty());
    }
}
