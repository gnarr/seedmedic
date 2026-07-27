//! Startup reconciliation of jobs parked for review.
//!
//! A parked job is never un-parked by reconciliation — only an operator does
//! that — but its `review_from_state` must still describe a resume point
//! reality actually supports, or an operator's retry resumes into a lie.

mod support;

use seedmedic::repair::{
    JobId, RepairState, RepairStore, ReviewReason, TransitionReason,
    reconcile::reconcile_on_startup,
};
use support::{Harness, default_policy};

/// Auto-resume disabled is the natural way to park a job right at `Verified`
/// with a real staging directory and a real info-hash, so this exercises the
/// same reality checks a rewind of an actionable job would.
fn policy_that_parks_at_verified() -> seedmedic::repair::SafetyPolicy {
    seedmedic::repair::SafetyPolicy {
        auto_resume: seedmedic::repair::AutoResume::Never,
        ..default_policy()
    }
}

#[tokio::test]
async fn a_parked_jobs_resume_point_moves_back_when_its_staged_data_vanished() {
    let harness = Harness::with_policy(policy_that_parks_at_verified()).await;
    let job = harness.discover().await;

    let parked = harness
        .run_until(40, |job| job.state == RepairState::AwaitingReview)
        .await;
    assert_eq!(parked.review_from_state, Some(RepairState::Verified));
    assert_eq!(parked.review_reason, Some(ReviewReason::AutoResumeDisabled));

    // Something removes the staged data while SeedMedic is down.
    std::fs::remove_dir_all(harness.staging_root.join("job-1")).expect("wipe staging");

    let summary = reconcile_on_startup(&harness.deps, "unused-owner").await;
    assert_eq!(summary.parked_examined, 1);
    assert_eq!(summary.parked_corrected, 1);

    let reconciled = harness.job(job.id).await;
    assert_eq!(
        reconciled.state,
        RepairState::AwaitingReview,
        "reconciliation must not un-park the job"
    );
    assert_eq!(
        reconciled.review_from_state,
        Some(RepairState::Matched),
        "with no staged data, a retry must not resume past staging"
    );

    let history = harness.store.history(job.id).await.expect("history");
    assert!(
        history
            .iter()
            .any(|record| record.reason == "reconciliation"),
        "the corrected resume point must be visible in the audit trail"
    );

    // An operator's retry now resumes where reality supports, not where the
    // job originally stopped.
    let transition = reconciled
        .plan_transition(RepairState::Matched, TransitionReason::OperatorRetry)
        .expect("retry must resume the corrected point");
    harness
        .store
        .apply(
            job.id,
            transition,
            seedmedic::repair::TransitionUpdate::default(),
        )
        .await
        .expect("operator retry");

    assert_eq!(harness.job(job.id).await.state, RepairState::Matched);
}

#[tokio::test]
async fn reconciling_a_parked_job_whose_data_is_intact_leaves_it_exactly_as_parked() {
    let harness = Harness::with_policy(policy_that_parks_at_verified()).await;
    let job = harness.discover().await;

    harness
        .run_until(40, |job| job.state == RepairState::AwaitingReview)
        .await;
    let before = harness.job(job.id).await;

    let summary = reconcile_on_startup(&harness.deps, "unused-owner").await;
    assert_eq!(summary.parked_examined, 1);
    assert_eq!(
        summary.parked_corrected, 0,
        "intact data needs no correction"
    );

    let after = harness.job(job.id).await;
    assert_eq!(after.state, RepairState::AwaitingReview);
    assert_eq!(after.review_from_state, before.review_from_state);
}

#[tokio::test]
async fn reconciliation_examines_parked_jobs_separately_from_unfinished_ones() {
    let harness = Harness::with_policy(policy_that_parks_at_verified()).await;
    harness.discover().await;

    harness
        .run_until(40, |job| job.state == RepairState::AwaitingReview)
        .await;

    let summary = reconcile_on_startup(&harness.deps, "unused-owner").await;
    assert_eq!(
        summary.jobs_examined, 0,
        "a parked job is not actionable, so `unfinished` must not have surfaced it"
    );
    assert_eq!(summary.parked_examined, 1);
}

/// `set_review_resume_point` is a no-op once an operator's retry already
/// un-parked the job — there is nothing left for reconciliation to correct.
#[tokio::test]
async fn set_review_resume_point_does_nothing_once_the_job_is_no_longer_parked() {
    let harness = Harness::with_policy(policy_that_parks_at_verified()).await;
    let job = harness.discover().await;

    harness
        .run_until(40, |job| job.state == RepairState::AwaitingReview)
        .await;

    let parked = harness.job(job.id).await;
    let transition = parked
        .plan_transition(RepairState::Verified, TransitionReason::OperatorRetry)
        .expect("retry resumes at the recorded step");
    harness
        .store
        .apply(
            job.id,
            transition,
            seedmedic::repair::TransitionUpdate::default(),
        )
        .await
        .expect("operator retry");

    harness
        .store
        .set_review_resume_point(JobId(job.id.0), RepairState::Matched)
        .await
        .expect("no-op, not an error");

    assert_eq!(harness.job(job.id).await.state, RepairState::Verified);
}
