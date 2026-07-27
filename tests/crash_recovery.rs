//! What happens when SeedMedic dies mid-repair.
//!
//! Three failure modes: a worker that stopped while holding a lease, external
//! state that moved on while the process was down, and external state that
//! moves while the worker is running. The first two are resolved by startup
//! reconciliation and the third by the worker itself, and in every case the
//! repair finishes without repeating side effects.

mod support;

use std::time::Duration;

use seedmedic::repair::{RepairState, RepairStore, reconcile::reconcile_on_startup};
use support::{Harness, OWNER};

#[tokio::test]
async fn a_lease_held_by_a_dead_worker_is_released_at_startup() {
    let harness = Harness::new().await;
    let job = harness.discover().await;

    // A worker claims the job and never comes back.
    let claimed = harness
        .store
        .claim(OWNER, Duration::from_secs(3600), 4)
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 1);

    // While the lease stands, nothing else can pick the job up.
    assert!(
        harness
            .store
            .claim("another-worker", Duration::from_secs(60), 4)
            .await
            .expect("claim")
            .is_empty()
    );

    let summary = reconcile_on_startup(&harness.deps, OWNER).await;
    assert_eq!(summary.leases_cleared, 1);

    let reclaimed = harness
        .store
        .claim(OWNER, Duration::from_secs(60), 4)
        .await
        .expect("claim");
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].id, job.id);
    // Reconciliation of a job that has not started does not move it.
    assert_eq!(harness.job(job.id).await.state, RepairState::Discovered);
}

#[tokio::test]
async fn staged_data_that_vanished_while_we_were_down_rewinds_the_repair() {
    let harness = Harness::new().await;
    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    // The process dies here, holding a lease, and something removes the
    // staging directory before it comes back.
    harness
        .store
        .claim(OWNER, Duration::from_secs(3600), 4)
        .await
        .expect("claim");
    std::fs::remove_dir_all(harness.staging_root.join("job-1")).expect("wipe staging");

    let summary = reconcile_on_startup(&harness.deps, OWNER).await;
    assert_eq!(summary.leases_cleared, 1);
    assert_eq!(summary.jobs_rewound, 1);

    let rewound = harness.job(job.id).await;
    assert_eq!(
        rewound.state,
        RepairState::Matched,
        "with no staged data the repair must go back to staging, not carry on seeding"
    );

    let history = harness.store.history(job.id).await.expect("history");
    assert!(
        history
            .iter()
            .any(|record| record.reason == "reconciliation"),
        "a rewind must be visible in the audit trail"
    );

    // And it recovers on its own.
    harness.tracker.clear_hit_and_run(&harness.torrent_id);
    harness
        .run_until(40, |job| job.state == RepairState::Completed)
        .await;

    assert!(
        harness
            .staging_root
            .join("job-1/Demo.Show.S01/e01.mkv")
            .exists(),
        "the files were staged again"
    );
    assert_eq!(
        harness.client.add_count(),
        1,
        "re-adding a torrent the client already has must not count as a new add"
    );
}

#[tokio::test]
async fn a_torrent_removed_from_the_client_rewinds_the_repair_to_staging() {
    let harness = Harness::new().await;
    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    // Somebody removed it from qBittorrent by hand.
    harness.client.forget(harness.info_hash);

    let summary = reconcile_on_startup(&harness.deps, OWNER).await;
    assert_eq!(summary.jobs_rewound, 1);
    assert_eq!(harness.job(job.id).await.state, RepairState::Staged);

    // The staged files are still there, so it picks up from injection.
    harness.tracker.clear_hit_and_run(&harness.torrent_id);
    harness
        .run_until(40, |job| job.state == RepairState::Completed)
        .await;
    assert_eq!(
        harness.client.add_count(),
        2,
        "it genuinely had to be re-added"
    );
}

/// The same correction, without waiting for a restart. A torrent that
/// disappears while the worker is running must not sit there burning retries.
#[tokio::test]
async fn a_torrent_that_disappears_mid_flight_rewinds_without_a_restart() {
    let harness = Harness::new().await;
    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Rechecking)
        .await;

    harness.client.forget(harness.info_hash);
    harness.tracker.clear_hit_and_run(&harness.torrent_id);

    // No reconciliation call: the worker sorts this out on its own.
    harness
        .run_until(40, |job| job.state == RepairState::Completed)
        .await;

    let history = harness.store.history(job.id).await.expect("history");
    let rewind = history
        .iter()
        .find(|record| record.reason == "reconciliation")
        .expect("the rewind is in the audit trail");
    assert_eq!(rewind.to, RepairState::Staged);
    assert!(
        harness.job(job.id).await.attempts < 3,
        "retries were not wasted"
    );
}

#[tokio::test]
async fn reconciliation_never_moves_a_repair_forward() {
    let harness = Harness::new().await;
    let job = harness.discover().await;

    // The client happens to know about this info-hash already — which says
    // nothing about whether *we* staged the right data.
    harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;
    harness
        .store
        .apply(
            job.id,
            harness
                .job(job.id)
                .await
                .plan_transition(
                    RepairState::Matched,
                    seedmedic::repair::TransitionReason::Reconciliation,
                )
                .expect("rewind is legal"),
            seedmedic::repair::TransitionUpdate::default(),
        )
        .await
        .expect("rewind");

    let summary = reconcile_on_startup(&harness.deps, OWNER).await;

    assert_eq!(summary.jobs_rewound, 0);
    assert_eq!(
        harness.job(job.id).await.state,
        RepairState::Matched,
        "external state must never be used to advance a repair"
    );
}
