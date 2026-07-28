//! Bulk review actions: the same per-job validated transition applied to a
//! list of jobs, each independent of the others. docs/todos/0010-manual-review.md.
//!
//! No web test harness exists, so this exercises the loop `web::review::bulk`
//! runs directly against the store — the handler itself is only that loop
//! plus a summary, with no transition semantics of its own.

use std::sync::Arc;

use chrono::Utc;
use seedmedic::{
    clock::{Clock, TestClock},
    database,
    repair::{
        RepairState, RepairStore, ReviewReason, TransitionReason, TransitionUpdate,
        adapters::sqlite::SqliteRepairStore,
    },
    tracker::{HitAndRun, TrackerId, TrackerTorrentId},
};

#[tokio::test]
async fn a_bulk_retry_applies_unmoved_jobs_and_reports_a_conflict_for_one_that_moved() {
    let clock = Arc::new(TestClock::default());
    let store =
        SqliteRepairStore::new(database::test_pool().await, clock.clone() as Arc<dyn Clock>);
    let tracker = TrackerId::new("test-tracker");

    let mut ids = Vec::new();
    for i in 0..5 {
        let hit_and_run = HitAndRun {
            tracker: tracker.clone(),
            torrent_id: TrackerTorrentId::new(format!("t-{i}")),
            torrent_name: format!("Show.{i}"),
            info_hash: None,
            size_bytes: 100,
            deadline: None,
            observed_at: Utc::now(),
        };
        let discovered = store
            .record_discovery(&hit_and_run)
            .await
            .expect("discover");
        ids.push(discovered.id);

        // Park for review, as matching would after failing to find a
        // candidate — the bulk action does not care why a job is parked,
        // only where `review_from_state` says a retry should resume.
        let job = store
            .job(discovered.id)
            .await
            .expect("job lookup")
            .expect("job exists");
        let transition = job
            .plan_transition(
                RepairState::AwaitingReview,
                TransitionReason::Review(ReviewReason::NoCandidates),
            )
            .expect("any actionable state can be parked for review");
        store
            .apply(discovered.id, transition, TransitionUpdate::default())
            .await
            .expect("park for review");
    }

    // One job moved in the meantime: an operator already retried it by hand,
    // before the bulk action ran.
    let moved = ids[2];
    let job = store
        .job(moved)
        .await
        .expect("job lookup")
        .expect("job exists");
    let transition = job
        .plan_transition(RepairState::Discovered, TransitionReason::OperatorRetry)
        .expect("resumes its recorded step");
    store
        .apply(moved, transition, TransitionUpdate::default())
        .await
        .expect("operator retry ahead of the bulk action");

    // What `web::review::bulk_retry` does for each selected job: the same
    // validated `OperatorRetry` transition a single retry applies, never
    // stopping the batch at the first problem.
    let mut applied = 0;
    let mut conflicts = 0;
    for &id in &ids {
        let job = store
            .job(id)
            .await
            .expect("job lookup")
            .expect("job exists");
        let outcome = job.review_from_state.ok_or(()).and_then(|resume_to| {
            job.plan_transition(resume_to, TransitionReason::OperatorRetry)
                .map_err(|_| ())
        });
        match outcome {
            Err(()) => conflicts += 1,
            Ok(transition) => match store
                .apply(id, transition, TransitionUpdate::default())
                .await
            {
                Ok(_) => applied += 1,
                Err(_) => conflicts += 1,
            },
        }
    }

    assert_eq!(applied, 4, "four of the five jobs were still parked");
    assert_eq!(
        conflicts, 1,
        "the job an operator already resumed by hand must be reported, not silently skipped"
    );

    for &id in &ids {
        let job = store
            .job(id)
            .await
            .expect("job lookup")
            .expect("job exists");
        assert_eq!(
            job.state,
            RepairState::Discovered,
            "every job — bulk-retried or already moved — ends up resumed"
        );
    }
}
