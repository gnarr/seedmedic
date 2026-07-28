//! Operator review actions that change what an automated decision would do,
//! rather than just moving a job: docs/todos/0010-manual-review.md.

mod support;

use seedmedic::{
    repair::{
        AutoResume, JobPatch, MaterializationPolicy, RepairState, RepairStore, ReviewReason,
        SafetyPolicy, TransitionReason, TransitionUpdate,
    },
    seeding::DataCompleteness,
};
use support::{Harness, default_policy};

fn policy_that_parks_on_auto_resume() -> SafetyPolicy {
    SafetyPolicy {
        auto_resume: AutoResume::Never,
        ..default_policy()
    }
}

/// The web layer's `approve_resume` action, reproduced directly against the
/// store — there is no HTTP test harness, so this exercises exactly what that
/// handler does: set `resume_approved` and retry back to the recorded step,
/// both in the one transition it writes.
async fn approve_resume(harness: &Harness, job: &seedmedic::repair::RepairJob) {
    let resume_to = job
        .review_from_state
        .expect("a job parked for review always records where it stopped");
    let transition = job
        .plan_transition(resume_to, TransitionReason::OperatorRetry)
        .expect("retry resumes exactly the recorded step");

    harness
        .store
        .apply(
            job.id,
            transition,
            TransitionUpdate::with_detail(serde_json::json!({ "operator": "approve_resume" }))
                .patch(JobPatch {
                    resume_approved: Some(true),
                    ..JobPatch::default()
                }),
        )
        .await
        .expect("approve resume");
}

#[tokio::test]
async fn approving_a_resume_lets_a_complete_verified_job_resume() {
    let harness = Harness::with_policy(policy_that_parks_on_auto_resume()).await;
    let job = harness.discover().await;

    let parked = harness
        .run_until(40, |job| job.state == RepairState::AwaitingReview)
        .await;
    assert_eq!(parked.review_reason, Some(ReviewReason::AutoResumeDisabled));
    assert_eq!(parked.review_from_state, Some(RepairState::Verified));
    assert_eq!(
        harness.client.resume_count(),
        0,
        "must not have resumed before approval"
    );

    approve_resume(&harness, &parked).await;

    let resumed = harness.job(job.id).await;
    assert_eq!(resumed.state, RepairState::Verified);
    assert!(resumed.resume_approved);

    let seeding = harness
        .run_until(10, |job| job.state == RepairState::Seeding)
        .await;
    assert_eq!(seeding.state, RepairState::Seeding);
    assert_eq!(harness.client.resume_count(), 1);
}

/// The one thing approval must never touch: `assess_data`'s absolute rule
/// that incomplete data aliasing the library never resumes. Setting
/// `resume_approved` up front, before the job ever reaches review, proves the
/// override in `decide_resume` really is scoped to `AutoResume::Never` alone.
#[tokio::test]
async fn an_approved_job_with_incomplete_aliased_data_still_refuses_to_resume() {
    let policy = SafetyPolicy {
        auto_resume: AutoResume::Never,
        materialization: MaterializationPolicy {
            prefer_reflink: false,
            allow_hardlink: true,
            allow_copy: false,
        },
        ..default_policy()
    };
    let harness = Harness::with_policy(policy).await;
    harness
        .client
        .set_on_disk(harness.info_hash, DataCompleteness::Partial { ratio: 0.5 });

    let job = harness.discover().await;
    harness
        .store
        .record_progress(
            job.id,
            JobPatch {
                resume_approved: Some(true),
                ..JobPatch::default()
            },
        )
        .await
        .expect("pre-approve the job before it ever parks");

    let parked = harness
        .run_until(40, |job| job.state == RepairState::AwaitingReview)
        .await;

    assert_eq!(
        parked.review_reason,
        Some(ReviewReason::AliasedIncompleteData)
    );
    assert_eq!(
        harness.client.resume_count(),
        0,
        "an approval must never override the incomplete-and-aliased safety floor"
    );
}

/// Approval is per job, recorded as an ordinary operator transition — it must
/// never look like it changed anything global. A second, unrelated job
/// discovered afterwards still parks exactly as before.
#[tokio::test]
async fn approving_one_jobs_resume_leaves_the_global_policy_untouched() {
    let harness = Harness::with_policy(policy_that_parks_on_auto_resume()).await;
    let job = harness.discover().await;

    let parked = harness
        .run_until(40, |job| job.state == RepairState::AwaitingReview)
        .await;
    approve_resume(&harness, &parked).await;
    harness
        .run_until(10, |job| job.state == RepairState::Seeding)
        .await;

    assert_eq!(
        harness.deps.policy.auto_resume,
        AutoResume::Never,
        "approving a job must never mutate the policy it was checked against"
    );
}
