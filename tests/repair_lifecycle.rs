//! The whole repair, end to end, over a real store and a real staging area.

mod support;

use seedmedic::{
    library::MatchConfidence,
    repair::{RepairState, RepairStore},
    seeding::DataCompleteness,
    staging::MaterializationStrategy,
};
use support::Harness;

#[tokio::test]
async fn a_repair_runs_from_discovery_to_seeding_and_waits_for_the_tracker() {
    let harness = Harness::new().await;

    let job = harness.discover().await;
    assert_eq!(job.state, RepairState::Discovered);

    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    // Everything the repair learned is on the job, not in the worker's head.
    assert_eq!(job.info_hash, Some(harness.info_hash));
    assert_eq!(job.total_bytes, Some(3000));
    assert_eq!(
        job.staging_dir.as_ref().map(|dir| dir.as_str()),
        Some("job-1")
    );

    // Reflink is unimplemented, so the policy's next choice was used.
    assert_eq!(job.materialization, Some(MaterializationStrategy::Copy));

    // The files really are on disk, in the torrent's layout.
    let staged = harness.staging_root.join("job-1/Demo.Show.S01");
    assert_eq!(
        std::fs::metadata(staged.join("e01.mkv")).unwrap().len(),
        1000
    );
    assert_eq!(
        std::fs::metadata(staged.join("e02.mkv")).unwrap().len(),
        2000
    );

    assert_eq!(harness.client.add_count(), 1);
    assert_eq!(harness.client.recheck_count(), 1);
    assert_eq!(harness.client.resume_count(), 1);

    // The tracker has not cleared the warning, so the repair is not finished —
    // however happy the download client is.
    harness.tick().await;
    assert_eq!(
        harness.job(job.id).await.state,
        RepairState::Seeding,
        "a seeding torrent must not be mistaken for a cleared hit-and-run"
    );
}

#[tokio::test]
async fn the_tracker_clearing_the_warning_is_what_completes_the_repair() {
    let harness = Harness::new().await;
    harness.discover().await;
    harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    harness.tracker.clear_hit_and_run(&harness.torrent_id);

    let job = harness
        .run_until(10, |job| job.state == RepairState::Completed)
        .await;
    assert_eq!(job.state, RepairState::Completed);
}

#[tokio::test]
async fn the_file_plan_records_what_was_matched_and_how_it_was_staged() {
    let harness = Harness::new().await;
    let job = harness.discover().await;
    harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    let files = harness.store.planned_files(job.id).await.expect("files");
    assert_eq!(files.len(), 2);

    for file in &files {
        assert_eq!(file.confidence, Some(MatchConfidence::Probable));
        assert_eq!(file.materialized_as, Some(MaterializationStrategy::Copy));
        assert!(file.source.as_ref().is_some_and(|path| {
            path.ends_with(file.torrent_path.as_str().rsplit('/').next().unwrap())
        }));
        // Size alone is never proof, so nothing claims to be verified.
        assert_eq!(file.evidence.map(|e| e.piece_verified), Some(false));
    }
}

#[tokio::test]
async fn every_decision_is_explained_in_the_audit_trail() {
    let harness = Harness::new().await;
    let job = harness.discover().await;
    harness.tracker.clear_hit_and_run(&harness.torrent_id);
    harness
        .run_until(40, |job| job.state == RepairState::Completed)
        .await;

    let history = harness.store.history(job.id).await.expect("history");
    let transitions: Vec<_> = history
        .iter()
        .map(|record| (record.from, record.to))
        .collect();

    // Discovery, then one row per lifecycle step. Nothing skipped, nothing
    // duplicated.
    let mut expected = vec![(RepairState::Discovered, RepairState::Discovered)];
    expected.extend(
        RepairState::PROGRESSION
            .windows(2)
            .map(|pair| (pair[0], pair[1])),
    );
    assert_eq!(transitions, expected);

    let matched = history
        .iter()
        .find(|record| record.to == RepairState::Matched)
        .expect("the matching step is recorded");
    assert!(
        matched.detail.is_some(),
        "an automated decision must record the evidence behind it"
    );
}

#[tokio::test]
async fn incomplete_data_parks_the_repair_instead_of_seeding_it() {
    let harness = Harness::new().await;
    // The recheck will find most, but not all, of the data.
    harness
        .client
        .set_on_disk(harness.info_hash, DataCompleteness::Partial { ratio: 0.98 });

    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::AwaitingReview)
        .await;

    assert_eq!(
        job.review_reason,
        Some(seedmedic::repair::ReviewReason::IncompleteData)
    );
    assert_eq!(job.review_from_state, Some(RepairState::Rechecking));
    assert_eq!(
        harness.client.resume_count(),
        0,
        "nothing may be resumed on incomplete data"
    );
}
