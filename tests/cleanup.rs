//! Reclaiming staged data: docs/todos/0010-manual-review.md.

mod support;

use seedmedic::{
    repair::{
        AutoResume, RepairState, RepairStore, SafetyPolicy, TransitionReason, TransitionUpdate,
    },
    seeding::TorrentClient,
};
use support::{Harness, default_policy};

fn policy_that_parks_at_verified() -> SafetyPolicy {
    SafetyPolicy {
        auto_resume: AutoResume::Never,
        ..default_policy()
    }
}

/// What `web::review::abandon_and_discard` does: remove the torrent from the
/// client, discard the staging directory, then abandon — reproduced here
/// exactly, since there is no HTTP test harness.
#[tokio::test]
async fn abandoning_with_discard_removes_the_staging_directory_and_nothing_else() {
    let harness = Harness::with_policy(policy_that_parks_at_verified()).await;
    harness.discover().await;

    let parked = harness
        .run_until(40, |job| job.state == RepairState::AwaitingReview)
        .await;
    let staging_dir = parked
        .staging_dir
        .clone()
        .expect("staged before it could park at verified");
    let staged_path = harness.staging_root.join(staging_dir.as_str());
    assert!(staged_path.exists(), "the harness really did stage files");

    if let Some(info_hash) = parked.info_hash {
        harness
            .client
            .remove(info_hash, false)
            .await
            .expect("remove from client");
    }
    harness
        .deps
        .staging
        .discard(&staging_dir)
        .await
        .expect("discard staging");

    let transition = parked
        .plan_transition(RepairState::Failed, TransitionReason::OperatorAbandon)
        .expect("a parked job can be abandoned");
    harness
        .store
        .apply(
            parked.id,
            transition,
            TransitionUpdate::with_detail(serde_json::json!({
                "operator": "abandon",
                "staging_discarded": true,
            }))
            .failed_because("abandoned by operator, staging discarded"),
        )
        .await
        .expect("apply abandon");

    assert!(
        !staged_path.exists(),
        "the job's own staging directory must be gone"
    );
    // The staging root itself — where every job's directory lives — is
    // untouched; only the one job directory under it was removed.
    assert!(
        harness.staging_root.exists(),
        "discard must be confined to the job's own directory"
    );

    let failed = harness.job(parked.id).await;
    assert_eq!(failed.state, RepairState::Failed);

    let history = harness.store.history(parked.id).await.expect("history");
    assert!(
        history
            .iter()
            .any(|record| record.reason == "operator_abandon"),
        "the discard must be recorded as an ordinary operator_* transition"
    );
}

#[tokio::test]
async fn staging_usage_reports_the_actual_bytes_written() {
    let harness = Harness::new().await;
    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;
    let staging_dir = job.staging_dir.expect("staged by now");

    let usage = harness
        .deps
        .staging
        .usage(&staging_dir)
        .await
        .expect("usage");

    // The fixed two-file torrent from `support::torrent_metadata`: 1000 + 2000.
    assert_eq!(usage, 3000);
}

#[tokio::test]
async fn staging_usage_is_zero_once_discarded() {
    let harness = Harness::new().await;
    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;
    let staging_dir = job.staging_dir.expect("staged by now");

    harness
        .deps
        .staging
        .discard(&staging_dir)
        .await
        .expect("discard");

    let usage = harness
        .deps
        .staging
        .usage(&staging_dir)
        .await
        .expect("usage of a missing directory is zero, not an error");
    assert_eq!(usage, 0);
}
