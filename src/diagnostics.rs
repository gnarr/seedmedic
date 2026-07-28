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
    /// Whether a tracker-unreachable notification has already gone out for
    /// the outage in progress — cleared on the next success, so one outage
    /// produces one notification rather than one per poll.
    pub notified_unreachable: bool,
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
        let mut trackers = self.lock();
        let health = trackers.entry(id.clone()).or_default();
        health.last_success = Some(at);
        health.notified_unreachable = false;
    }

    pub fn record_tracker_error(&self, id: &TrackerId, at: DateTime<Utc>, message: String) {
        self.lock().entry(id.clone()).or_default().last_error = Some((at, message));
    }

    /// Marks the current outage as already notified, so it is reported once.
    pub fn mark_tracker_unreachable_notified(&self, id: &TrackerId) {
        self.lock()
            .entry(id.clone())
            .or_default()
            .notified_unreachable = true;
    }

    pub fn tracker_health(&self, id: &TrackerId) -> TrackerHealth {
        self.lock().get(id).cloned().unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TrackerId, TrackerHealth>> {
        self.trackers.lock().expect("diagnostics poisoned")
    }
}
