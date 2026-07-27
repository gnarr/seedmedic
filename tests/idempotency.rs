//! Every transition must be safe to replay. These tests hold the store to that.

mod support;

use seedmedic::repair::{
    Applied, RepairState, RepairStore, StoreError, TransitionReason, TransitionUpdate,
};
use support::Harness;

#[tokio::test]
async fn rediscovering_the_same_warning_does_not_create_a_second_job() {
    let harness = Harness::new().await;

    let first = harness.discover().await;
    harness.worker().discover().await;
    harness.worker().discover().await;

    let jobs = harness.store.jobs(10).await.expect("jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, first.id);

    // The audit trail records the discovery once, not once per poll.
    let history = harness.store.history(first.id).await.expect("history");
    assert_eq!(history.len(), 1);
}

#[tokio::test]
async fn replaying_a_transition_changes_nothing_and_is_not_an_error() {
    let harness = Harness::new().await;
    let job = harness.discover().await;

    let transition = job
        .plan_transition(RepairState::TorrentFetched, TransitionReason::Progress)
        .expect("legal transition");

    assert_eq!(
        harness
            .store
            .apply(job.id, transition, TransitionUpdate::default())
            .await
            .expect("first apply"),
        Applied::Applied
    );

    // Exactly what happens when a worker crashes after the side effect but
    // before the transition, then retries.
    assert_eq!(
        harness
            .store
            .apply(job.id, transition, TransitionUpdate::default())
            .await
            .expect("replay is not an error"),
        Applied::AlreadyInTargetState
    );

    let history = harness.store.history(job.id).await.expect("history");
    assert_eq!(
        history
            .iter()
            .filter(|record| record.to == RepairState::TorrentFetched)
            .count(),
        1,
        "a replayed transition must not write a second audit row"
    );
}

#[tokio::test]
async fn a_transition_from_a_state_the_job_has_left_is_refused() {
    let harness = Harness::new().await;
    let job = harness.discover().await;

    let stale = job
        .plan_transition(RepairState::TorrentFetched, TransitionReason::Progress)
        .expect("legal transition");

    harness
        .store
        .apply(job.id, stale, TransitionUpdate::default())
        .await
        .expect("first apply");

    // Meanwhile the job moved on.
    let moved = harness.job(job.id).await;
    let next = moved
        .plan_transition(RepairState::Matched, TransitionReason::Progress)
        .expect("legal transition");
    harness
        .store
        .apply(job.id, next, TransitionUpdate::default())
        .await
        .expect("second apply");

    // Now the original transition is neither a replay nor valid.
    match harness
        .store
        .apply(job.id, stale, TransitionUpdate::default())
        .await
    {
        Err(StoreError::Conflict {
            actual, expected, ..
        }) => {
            assert_eq!(actual, RepairState::Matched);
            assert_eq!(expected, RepairState::Discovered);
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn re_running_the_whole_workflow_does_not_repeat_side_effects() {
    let harness = Harness::new().await;
    harness.discover().await;
    harness
        .run_until(40, |job| {
            job.state == seedmedic::repair::RepairState::Seeding
        })
        .await;

    // Keep ticking: the job is waiting on the tracker, and nothing else should
    // happen while it does.
    for _ in 0..5 {
        harness.tick().await;
        harness.clock.advance(chrono::Duration::seconds(30));
    }

    assert_eq!(harness.client.add_count(), 1);
    assert_eq!(harness.client.recheck_count(), 1);
    assert_eq!(harness.client.resume_count(), 1);
    assert_eq!(
        harness.tracker.fetch_count(),
        1,
        "the .torrent is stored on the job; there is no reason to download it twice"
    );
}
