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

    /// Reconcile tracker entries with a freshly loaded configuration.
    ///
    /// `Diagnostics` is part of `bootstrap::Persistent` (see
    /// `docs/todos/0016-a-swappable-runtime.md`): it outlives every reload, so
    /// without this the tracker error history an operator is looking at when
    /// they change a setting would be exactly what a reload throws away.
    /// Called after every reload with the new configuration's trackers: an
    /// entry for a tracker no longer configured is dropped, an entry for a
    /// newly configured one is added, and every surviving entry's `stub` flag
    /// is refreshed to match — none of that touches `last_success` or
    /// `last_error` for a tracker that remains.
    pub fn reseed(&self, configured: impl IntoIterator<Item = (TrackerId, bool)>) {
        let configured: HashMap<TrackerId, bool> = configured.into_iter().collect();
        let mut trackers = self.lock();
        trackers.retain(|id, _| configured.contains_key(id));
        for (id, stub) in configured {
            trackers.entry(id).or_default().stub = stub;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TrackerId, TrackerHealth>> {
        self.trackers.lock().expect("diagnostics poisoned")
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn reseed_drops_a_tracker_no_longer_configured() {
        let diagnostics = Diagnostics::new([TrackerId::new("gone")]);

        diagnostics.reseed([(TrackerId::new("kept"), false)]);

        assert!(!diagnostics.lock().contains_key(&TrackerId::new("gone")));
    }

    #[test]
    fn reseed_keeps_history_for_a_tracker_that_remains() {
        let id = TrackerId::new("kept");
        let diagnostics = Diagnostics::default();
        diagnostics.record_tracker_success(&id, Utc::now());

        diagnostics.reseed([(id.clone(), false)]);

        assert!(diagnostics.tracker_health(&id).last_success.is_some());
    }

    #[test]
    fn reseed_refreshes_the_stub_flag_of_a_surviving_tracker() {
        let id = TrackerId::new("switched");
        let diagnostics = Diagnostics::new([id.clone()]);
        assert!(diagnostics.tracker_health(&id).stub);

        diagnostics.reseed([(id.clone(), false)]);

        assert!(!diagnostics.tracker_health(&id).stub);
    }

    #[test]
    fn reseed_adds_an_entry_for_a_newly_configured_tracker() {
        let diagnostics = Diagnostics::default();

        diagnostics.reseed([(TrackerId::new("new"), true)]);

        assert!(diagnostics.tracker_health(&TrackerId::new("new")).stub);
    }
}
