//! What happens while a repair sits in `Seeding`, beyond "ask the tracker".
//!
//! The tracker is still the only thing that can complete a repair, but a job
//! can sit here for days, and the client is the only thing that would notice
//! if the torrent stopped seeding in the meantime. See
//! docs/todos/0009-tracker-confirmation.md.

mod support;

use seedmedic::{
    repair::{AutoResume, RepairState, RepairStore, ReviewReason, SafetyPolicy},
    seeding::ClientTorrentState,
};
use support::{Harness, default_policy};

#[tokio::test]
async fn a_seeding_client_waits_without_spending_an_attempt() {
    let harness = Harness::new().await;
    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    harness.tick().await;
    let job = harness.job(job.id).await;
    assert_eq!(job.state, RepairState::Seeding);
    assert_eq!(
        job.attempts, 0,
        "waiting for the tracker must not cost an attempt"
    );
}

#[tokio::test]
async fn a_paused_torrent_is_resumed_through_the_gate() {
    let harness = Harness::new().await;
    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;
    assert_eq!(harness.client.resume_count(), 1);

    harness
        .client
        .force_state(harness.info_hash, ClientTorrentState::Paused);
    harness.tick().await;

    assert_eq!(
        harness.client.resume_count(),
        2,
        "confirm must have re-asked the gate and resumed rather than trusting the client"
    );
    let job = harness.job(job.id).await;
    assert_eq!(job.state, RepairState::Seeding);
}

#[tokio::test]
async fn auto_resume_never_parks_a_paused_torrent_instead_of_resuming_it() {
    let harness = Harness::new().await;
    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    harness
        .client
        .force_state(harness.info_hash, ClientTorrentState::Paused);

    let strict = SafetyPolicy {
        auto_resume: AutoResume::Never,
        ..default_policy()
    };
    harness.tick_with_policy(strict).await;

    let job = harness.job(job.id).await;
    assert_eq!(job.state, RepairState::AwaitingReview);
    assert_eq!(job.review_reason, Some(ReviewReason::AutoResumeDisabled));
    assert_eq!(
        harness.client.resume_count(),
        1,
        "a refused gate must never resume"
    );
}

#[tokio::test]
async fn a_downloading_torrent_parks_immediately() {
    let harness = Harness::new().await;
    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    harness
        .client
        .force_state(harness.info_hash, ClientTorrentState::Downloading);
    harness.tick().await;

    let job = harness.job(job.id).await;
    assert_eq!(job.state, RepairState::AwaitingReview);
    assert_eq!(
        job.review_reason,
        Some(ReviewReason::DownloadingDuringSeeding)
    );
}

#[tokio::test]
async fn a_torrent_missing_from_the_client_rewinds_to_staged_and_recovers() {
    let harness = Harness::new().await;
    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    harness.client.forget(harness.info_hash);
    // One tick both rewinds and re-drives the job forward again, so the
    // durable proof is the audit trail, not the state the tick happens to
    // land on.
    harness.tick().await;

    let history = harness.store.history(job.id).await.expect("history");
    assert!(
        history
            .iter()
            .any(|record| record.from == RepairState::Seeding
                && record.to == RepairState::Staged
                && record.reason == "reconciliation"),
        "expected a reconciliation record rewinding seeding back to staged"
    );

    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;
    assert_eq!(
        job.state,
        RepairState::Seeding,
        "the repair must recover on its own, not stay stuck"
    );
}

#[tokio::test]
async fn an_errored_torrent_rewinds_to_staged_and_recovers() {
    let harness = Harness::new().await;
    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    harness
        .client
        .set_errored(harness.info_hash, "disk read error");
    harness.tick().await;

    let history = harness.store.history(job.id).await.expect("history");
    assert!(
        history
            .iter()
            .any(|record| record.from == RepairState::Seeding
                && record.to == RepairState::Staged
                && record.reason == "reconciliation"),
        "expected a reconciliation record rewinding seeding back to staged"
    );

    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;
    assert_eq!(
        job.state,
        RepairState::Seeding,
        "the repair must recover on its own, not stay stuck"
    );
}
