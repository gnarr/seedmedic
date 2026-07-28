//! Tracker monitoring: turn hit-and-run warnings into repair jobs.
//!
//! Runs on its own cadence, separate from the worker, because polling trackers
//! and stepping repairs have nothing to do with each other's timing.
//!
//! Idempotent by construction: [`RepairStore::record_discovery`] is keyed on
//! `(tracker, torrent id)`, so a warning that is still outstanding on the next
//! poll updates nothing.

use serde_json::json;
use tracing::{info, warn};

use crate::{
    notify::NotificationEvent,
    repair::worker::RepairDeps,
    tracker::{TrackerError, TrackerId},
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DiscoverySummary {
    pub warnings_seen: usize,
    pub jobs_created: usize,
    pub trackers_failed: usize,
}

/// Poll every configured tracker once.
///
/// A tracker that fails is logged and skipped. One tracker being down must not
/// stop the others, and must never be read as "no warnings".
pub async fn discover_hit_and_runs(deps: &RepairDeps) -> DiscoverySummary {
    let mut summary = DiscoverySummary::default();

    for tracker in deps.trackers.values() {
        let warnings = match tracker.list_hit_and_runs().await {
            Ok(warnings) => warnings,
            Err(error) => {
                summary.trackers_failed += 1;
                deps.diagnostics.record_tracker_error(
                    tracker.id(),
                    deps.clock.now(),
                    error.to_string(),
                );
                #[cfg(feature = "metrics")]
                deps.metrics
                    .record_tracker_poll(tracker.id().as_str(), "error");
                log_tracker_error(tracker.id().as_str(), &error);
                notify_if_unreachable_too_long(deps, tracker.id()).await;
                continue;
            }
        };
        deps.diagnostics
            .record_tracker_success(tracker.id(), deps.clock.now());
        #[cfg(feature = "metrics")]
        deps.metrics
            .record_tracker_poll(tracker.id().as_str(), "success");

        summary.warnings_seen += warnings.len();
        for warning in warnings {
            match deps.store.record_discovery(&warning).await {
                // Trackers keep showing a warning until it clears, so most
                // polls see the same ones. Only say something when it is new.
                Ok(discovered) if discovered.created => {
                    summary.jobs_created += 1;
                    info!(
                        job = %discovered.id,
                        tracker = %warning.tracker,
                        torrent = %warning.torrent_id,
                        name = %warning.torrent_name,
                        "new hit-and-run"
                    );
                }
                Ok(_) => {}
                Err(error) => warn!(
                    tracker = %warning.tracker,
                    torrent = %warning.torrent_id,
                    %error,
                    "could not record hit-and-run"
                ),
            }
        }
    }

    summary
}

/// Once a tracker has been unreachable since its last known success for
/// longer than `tracker_unreachable_threshold`, notify — once per outage, not
/// once per poll. A tracker that has never once succeeded does not qualify:
/// that is a configuration problem to catch at startup, not an outage.
async fn notify_if_unreachable_too_long(deps: &RepairDeps, tracker: &TrackerId) {
    let health = deps.diagnostics.tracker_health(tracker);
    if health.notified_unreachable {
        return;
    }
    let Some(last_success) = health.last_success else {
        return;
    };

    let unreachable_for = deps.clock.now() - last_success;
    let threshold = chrono::Duration::from_std(deps.tracker_unreachable_threshold)
        .unwrap_or_else(|_| chrono::Duration::seconds(1800));
    if unreachable_for < threshold {
        return;
    }

    deps.diagnostics.mark_tracker_unreachable_notified(tracker);
    let event = NotificationEvent::TrackerUnreachable {
        tracker: tracker.to_string(),
        unreachable_for: unreachable_for
            .to_std()
            .unwrap_or(deps.tracker_unreachable_threshold),
    };
    if let Err(error) = deps.notifier.notify(&event).await {
        warn!(%error, "notification failed");
    }
}

fn log_tracker_error(tracker: &str, error: &TrackerError) {
    match error {
        TrackerError::NotImplemented(details) => info!(
            tracker,
            todo = details.todo,
            "tracker adapter is a stub; no warnings will be discovered"
        ),
        error => {
            warn!(tracker, %error, detail = %json!({ "transient": error.is_transient() }), "tracker poll failed")
        }
    }
}
