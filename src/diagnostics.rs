//! Cross-cutting operational state the `/status` page reads.
//!
//! Nothing here is durable and nothing here is a decision: nothing in the
//! repair workflow may come to depend on what is recorded here. It exists
//! purely to answer "what is SeedMedic doing and can it reach everything?"
//! — see `docs/todos/0012-observability.md`.

use std::{collections::HashMap, sync::Mutex};

use chrono::{DateTime, Utc};

use crate::tracker::TrackerId;

/// What is known about one configured tracker: whether it is a stub adapter
/// (known at startup, from config), and — updated as polls happen — when it
/// last succeeded and what its last error was.
#[derive(Clone, Debug, Default)]
pub struct TrackerHealth {
    pub stub: bool,
    pub last_success: Option<DateTime<Utc>>,
    pub last_error: Option<(DateTime<Utc>, String)>,
}

#[derive(Default)]
pub struct Diagnostics {
    trackers: Mutex<HashMap<TrackerId, TrackerHealth>>,
}

impl Diagnostics {
    /// One entry per configured tracker, seeded with whether it is a stub —
    /// known up front, so the status page lists every tracker even before
    /// its first poll.
    pub fn new(stub_trackers: impl IntoIterator<Item = TrackerId>) -> Self {
        let trackers = stub_trackers
            .into_iter()
            .map(|id| {
                (
                    id,
                    TrackerHealth {
                        stub: true,
                        ..TrackerHealth::default()
                    },
                )
            })
            .collect();
        Self {
            trackers: Mutex::new(trackers),
        }
    }

    pub fn record_tracker_success(&self, id: &TrackerId, at: DateTime<Utc>) {
        self.lock().entry(id.clone()).or_default().last_success = Some(at);
    }

    pub fn record_tracker_error(&self, id: &TrackerId, at: DateTime<Utc>, message: String) {
        self.lock().entry(id.clone()).or_default().last_error = Some((at, message));
    }

    pub fn tracker_health(&self, id: &TrackerId) -> TrackerHealth {
        self.lock().get(id).cloned().unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TrackerId, TrackerHealth>> {
        self.trackers.lock().expect("diagnostics poisoned")
    }
}
