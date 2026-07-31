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
    events::{Activity, ActivityKind, EventKind},
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
    let mut created = Vec::new();
    // Whether any tracker's reachability *changed* this run, so the dashboard's
    // integration pills refetch only when there is something new to show rather
    // than on every poll.
    let mut health_changed = false;

    for tracker in deps.trackers.values() {
        let warnings = match tracker.list_hit_and_runs().await {
            Ok(warnings) => warnings,
            Err(error) => {
                summary.trackers_failed += 1;
                health_changed |= deps
                    .diagnostics
                    .tracker_health(tracker.id())
                    .last_error
                    .is_none();
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
        health_changed |= deps
            .diagnostics
            .tracker_health(tracker.id())
            .last_error
            .is_some();
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
                    created.push(discovered.id);
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

    // Published after the whole run, not per tracker: a poll of five trackers is
    // one thing that happened, and five events would just be five refetches of
    // the same page.
    if !created.is_empty() {
        deps.events
            .publish(EventKind::JobsChanged { jobs: created });
    }
    if health_changed {
        deps.events.publish(EventKind::TrackersChanged);
    }
    deps.events.publish(EventKind::Activity(Activity {
        kind: ActivityKind::Discovery,
        at: Some(deps.clock.now()),
        jobs_created: summary.jobs_created,
        trackers_failed: summary.trackers_failed,
        ..Activity::default()
    }));

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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::{
        clock::TestClock, database, diagnostics::Diagnostics, notify::adapters::noop::NoopNotifier,
        repair::adapters::sqlite::SqliteRepairStore, seeding::adapters::fake::FakeTorrentClient,
        staging::adapters::unconfigured::UnconfiguredStaging,
        torrent::adapters::fake::FakeInspector,
    };

    use super::*;

    /// A fresh install has no trackers configured yet. Nothing in
    /// `discover_hit_and_runs` should assume there is at least one — it must
    /// return the zero summary without touching the store, the staging area,
    /// or any other port a real tracker would need.
    #[tokio::test]
    async fn an_empty_tracker_map_is_a_no_op() {
        let clock = std::sync::Arc::new(TestClock::default());
        let store = std::sync::Arc::new(SqliteRepairStore::new(
            database::test_pool().await,
            clock.clone() as std::sync::Arc<dyn crate::clock::Clock>,
        ));

        let deps = RepairDeps {
            store,
            trackers: std::collections::HashMap::new(),
            inspector: std::sync::Arc::new(FakeInspector),
            candidate_sources: Vec::new(),
            staging: std::sync::Arc::new(UnconfiguredStaging),
            client: std::sync::Arc::new(FakeTorrentClient::new()),
            clock,
            policy: crate::repair::SafetyPolicy::default(),
            category: None,
            worker_health: std::sync::Arc::new(crate::repair::worker::WorkerHealth::default()),
            diagnostics: std::sync::Arc::new(Diagnostics::new(std::iter::empty())),
            events: std::sync::Arc::new(crate::events::EventBus::default()),
            client_is_stub: true,
            #[cfg(feature = "metrics")]
            metrics: std::sync::Arc::new(crate::metrics::Metrics::default()),
            notifier: std::sync::Arc::new(NoopNotifier),
            tracker_unreachable_threshold: Duration::from_secs(1800),
        };

        let summary = discover_hit_and_runs(&deps).await;

        assert_eq!(summary, DiscoverySummary::default());
    }
}
