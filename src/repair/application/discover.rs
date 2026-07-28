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

use crate::{repair::worker::RepairDeps, tracker::TrackerError};

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
                log_tracker_error(tracker.id().as_str(), &error);
                continue;
            }
        };
        deps.diagnostics
            .record_tracker_success(tracker.id(), deps.clock.now());

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
