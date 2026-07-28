//! An operator resolving a match the worker could not decide on itself:
//! docs/todos/0010-manual-review.md.

mod support;

use seedmedic::{
    library::{MatchConfidence, UnmatchedReason},
    repair::{
        JobPatch, PlannedFile, RepairState, RepairStore, ReviewReason, TransitionReason,
        TransitionUpdate,
    },
    torrent::{InfoHash, TorrentFile, TorrentMetadata},
};
use support::{Harness, default_policy, path};

/// One file, two library files of the same size and neither name-matching it:
/// exactly the case matching cannot resolve on its own.
fn ambiguous_torrent() -> TorrentMetadata {
    TorrentMetadata {
        info_hash: InfoHash::from_bytes([11; 20]),
        name: path("Show.S01"),
        piece_length: 1 << 16,
        files: vec![TorrentFile {
            path: path("Show.S01/e01.mkv"),
            length: 100,
        }],
        pieces: Vec::new(),
    }
}

/// The rejected candidates for one torrent file, as recorded on the
/// transition that parked the job for review — reproduced here exactly as the
/// web review page reads them (`web::jobs::ambiguous_candidates`, not public
/// outside the crate), since there is no HTTP test harness.
async fn rejected_candidates(
    harness: &Harness,
    job_id: seedmedic::repair::JobId,
    torrent_path: &str,
) -> Vec<seedmedic::library::CandidateSummary> {
    let history = harness.store.history(job_id).await.expect("history");
    let detail = history
        .iter()
        .rev()
        .find(|record| record.reason == "review")
        .and_then(|record| record.detail.clone())
        .expect("a parked job has a review transition with detail");

    let unmatched = detail
        .get("unmatched")
        .and_then(|value| value.as_array())
        .expect("detail records the unmatched files");

    unmatched
        .iter()
        .find(|entry| entry.get("path").and_then(|v| v.as_str()) == Some(torrent_path))
        .and_then(|entry| entry.get("reason"))
        .and_then(|reason| serde_json::from_value::<UnmatchedReason>(reason.clone()).ok())
        .map(|reason| match reason {
            UnmatchedReason::Ambiguous { candidates } => candidates,
            UnmatchedReason::NoCandidate => Vec::new(),
        })
        .expect("the ambiguous file records its rejected candidates")
}

#[tokio::test]
async fn choosing_a_candidate_rewrites_the_file_plan_and_resumes_at_matched() {
    let harness = Harness::with_policy_and_metadata(
        default_policy(),
        ambiguous_torrent(),
        &[
            ("candidate-a.mkv", vec![b'a'; 100]),
            ("candidate-b.mkv", vec![b'b'; 100]),
        ],
    )
    .await;

    let job = harness.discover().await;
    let parked = harness
        .run_until(40, |job| job.state == RepairState::AwaitingReview)
        .await;
    assert_eq!(parked.review_reason, Some(ReviewReason::AmbiguousMatch));
    assert_eq!(parked.review_from_state, Some(RepairState::TorrentFetched));

    let candidates = rejected_candidates(&harness, job.id, "Show.S01/e01.mkv").await;
    assert_eq!(candidates.len(), 2, "both same-sized files were considered");
    let chosen = candidates[0].clone();

    // What `web::review::choose_candidate` does: rewrite the one file in the
    // plan the operator resolved, then complete matching in their stead.
    let mut files = harness.store.planned_files(job.id).await.expect("files");
    let file = files
        .iter_mut()
        .find(|file| file.torrent_path.as_str() == "Show.S01/e01.mkv")
        .expect("the file is in the plan");
    file.source = Some(chosen.path.clone());
    file.confidence = Some(MatchConfidence::Operator);

    let transition = parked
        .plan_transition(
            RepairState::Matched,
            TransitionReason::OperatorChooseCandidate,
        )
        .expect("choosing a candidate resumes exactly the matching step");
    harness
        .store
        .apply(
            job.id,
            transition,
            TransitionUpdate::default().patch(JobPatch {
                files: Some(files),
                ..JobPatch::default()
            }),
        )
        .await
        .expect("apply the operator's choice");

    let resumed = harness.job(job.id).await;
    assert_eq!(resumed.state, RepairState::Matched);

    let planned = harness.store.planned_files(job.id).await.expect("files");
    let file = planned
        .iter()
        .find(|file| file.torrent_path.as_str() == "Show.S01/e01.mkv")
        .expect("the file is in the plan");
    assert_eq!(file.source, Some(chosen.path));
    assert_eq!(file.confidence, Some(MatchConfidence::Operator));

    // The operator's choice is not trusted blindly: staging and a full recheck
    // still run, exactly as an automated match would go through them.
    let seeding = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;
    assert_eq!(seeding.state, RepairState::Seeding);
    assert_eq!(
        harness.client.recheck_count(),
        1,
        "an operator-chosen file must still be verified by the client, not trusted outright"
    );
}

#[tokio::test]
async fn a_plain_retry_on_an_ambiguous_match_cannot_resume_straight_to_matched() {
    let harness = Harness::with_policy_and_metadata(
        default_policy(),
        ambiguous_torrent(),
        &[
            ("candidate-a.mkv", vec![b'a'; 100]),
            ("candidate-b.mkv", vec![b'b'; 100]),
        ],
    )
    .await;

    harness.discover().await;
    let parked = harness
        .run_until(40, |job| job.state == RepairState::AwaitingReview)
        .await;

    assert!(
        parked
            .plan_transition(
                RepairState::Matched,
                TransitionReason::OperatorChooseCandidate
            )
            .is_ok(),
        "choosing a candidate is legal from where matching parked"
    );
    assert!(
        parked
            .plan_transition(RepairState::Matched, TransitionReason::OperatorRetry)
            .is_err(),
        "an ordinary retry must resume matching itself, not skip straight to matched"
    );
}
