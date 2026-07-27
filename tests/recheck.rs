//! `Rechecking` behaviour beyond a single completeness ratio: per-file detail,
//! adaptive polling, the recheck ceiling, and `Errored` mid-check. See
//! docs/todos/0008-recheck-and-resume.md.

mod support;

use std::collections::HashMap;

use seedmedic::{
    repair::{MaterializationPolicy, RepairStore, ReviewReason, SafetyPolicy},
    seeding::DataCompleteness,
    torrent::{InfoHash, TorrentFile, TorrentMetadata},
};
use support::{Harness, default_policy, path};

/// Four files, one of which is a different encode: the case the whole feature
/// exists for. A single ratio can only say "75% of this torrent"; the review
/// page needs to say which quarter.
fn season_pack() -> TorrentMetadata {
    TorrentMetadata {
        info_hash: InfoHash::from_bytes([7; 20]),
        name: path("Season.Pack"),
        piece_length: 1 << 20,
        files: vec![
            TorrentFile {
                path: path("Season.Pack/e01.mkv"),
                length: 1000,
            },
            TorrentFile {
                path: path("Season.Pack/e02.mkv"),
                length: 1000,
            },
            TorrentFile {
                path: path("Season.Pack/e03.mkv"),
                length: 1000,
            },
            TorrentFile {
                path: path("Season.Pack/e04.mkv"),
                length: 1000,
            },
        ],
        pieces: Vec::new(),
    }
}

fn season_pack_library() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("e01.mkv", vec![b'a'; 1000]),
        ("e02.mkv", vec![b'b'; 1000]),
        ("e03.mkv", vec![b'c'; 1000]),
        ("e04.mkv", vec![b'd'; 1000]),
    ]
}

#[tokio::test]
async fn a_partial_recheck_records_exactly_which_file_is_the_mismatch() {
    let library = season_pack_library();
    let harness =
        Harness::with_policy_and_metadata(default_policy(), season_pack(), &library).await;

    // Three quarters of the torrent, and the client can say which quarter.
    harness
        .client
        .set_on_disk(harness.info_hash, DataCompleteness::Partial { ratio: 0.75 });
    harness.client.set_file_progress(
        harness.info_hash,
        vec![
            seedmedic::seeding::FileProgress {
                torrent_path: path("Season.Pack/e01.mkv"),
                completeness: DataCompleteness::Complete,
            },
            seedmedic::seeding::FileProgress {
                torrent_path: path("Season.Pack/e02.mkv"),
                completeness: DataCompleteness::Complete,
            },
            seedmedic::seeding::FileProgress {
                torrent_path: path("Season.Pack/e03.mkv"),
                completeness: DataCompleteness::Complete,
            },
            seedmedic::seeding::FileProgress {
                torrent_path: path("Season.Pack/e04.mkv"),
                completeness: DataCompleteness::Partial { ratio: 0.0 },
            },
        ],
    );

    harness.discover().await;
    let job = harness
        .run_until(40, |job| {
            job.state == seedmedic::repair::RepairState::AwaitingReview
        })
        .await;

    assert_eq!(job.review_reason, Some(ReviewReason::IncompleteData));

    let files = harness.store.planned_files(job.id).await.expect("files");
    let mut by_path: HashMap<String, Option<f64>> = files
        .into_iter()
        .map(|file| (file.torrent_path.as_str().to_owned(), file.recheck_progress))
        .collect();

    assert_eq!(by_path.remove("Season.Pack/e01.mkv"), Some(Some(1.0)));
    assert_eq!(by_path.remove("Season.Pack/e02.mkv"), Some(Some(1.0)));
    assert_eq!(by_path.remove("Season.Pack/e03.mkv"), Some(Some(1.0)));
    assert_eq!(
        by_path.remove("Season.Pack/e04.mkv"),
        Some(Some(0.0)),
        "the mismatched file is the one the review page must name"
    );
    assert!(by_path.is_empty(), "no other files were planned");
}

fn hardlink_only_policy() -> SafetyPolicy {
    SafetyPolicy {
        materialization: MaterializationPolicy {
            prefer_reflink: false,
            allow_hardlink: true,
            allow_copy: false,
        },
        ..default_policy()
    }
}

#[tokio::test]
async fn hardlinked_incomplete_data_still_parks_when_per_file_detail_is_present() {
    let harness = Harness::with_policy(hardlink_only_policy()).await;
    harness
        .client
        .set_on_disk(harness.info_hash, DataCompleteness::Partial { ratio: 0.5 });
    harness.client.set_file_progress(
        harness.info_hash,
        vec![
            seedmedic::seeding::FileProgress {
                torrent_path: path("Demo.Show.S01/e01.mkv"),
                completeness: DataCompleteness::Complete,
            },
            seedmedic::seeding::FileProgress {
                torrent_path: path("Demo.Show.S01/e02.mkv"),
                completeness: DataCompleteness::Partial { ratio: 0.0 },
            },
        ],
    );

    harness.discover().await;
    let job = harness
        .run_until(40, |job| {
            job.state == seedmedic::repair::RepairState::AwaitingReview
        })
        .await;

    assert_eq!(job.review_reason, Some(ReviewReason::AliasedIncompleteData));
    assert_eq!(
        harness.client.resume_count(),
        0,
        "per-file detail must never make the gate more permissive"
    );
}

#[tokio::test]
async fn hardlinked_incomplete_data_still_parks_when_per_file_detail_is_absent() {
    let harness = Harness::with_policy(hardlink_only_policy()).await;
    harness
        .client
        .set_on_disk(harness.info_hash, DataCompleteness::Partial { ratio: 0.5 });

    harness.discover().await;
    let job = harness
        .run_until(40, |job| {
            job.state == seedmedic::repair::RepairState::AwaitingReview
        })
        .await;

    assert_eq!(job.review_reason, Some(ReviewReason::AliasedIncompleteData));
    assert_eq!(harness.client.resume_count(), 0);
}
