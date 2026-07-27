//! End-to-end proof that piece verification reaches `Exact` and that
//! `min_match_confidence = "exact"` is a usable setting rather than a way to
//! send every repair to review — see docs/todos/0005-media-matching.md.

mod support;

use seedmedic::{
    library::MatchConfidence,
    repair::{AutoResume, RepairState, RepairStore, SafetyPolicy},
    torrent::{InfoHash, PieceHash, TorrentFile, TorrentMetadata},
};
use sha1::{Digest, Sha1};
use support::{Harness, default_policy, path};

fn piece_hash(bytes: &[u8]) -> PieceHash {
    PieceHash::from_bytes(Sha1::digest(bytes).into())
}

/// Two library files share the torrent's file size; only one has the right
/// bytes, and it is not the one whose name matches. Size and name alone would
/// stage the wrong file — piece verification is what tells them apart.
#[tokio::test]
async fn a_correct_candidate_among_same_size_files_is_matched_exactly_and_completes() {
    let correct = vec![b'a'; 100];
    let wrong = vec![b'b'; 100];

    let metadata = TorrentMetadata {
        info_hash: InfoHash::from_bytes([42; 20]),
        name: path("Movie"),
        piece_length: 100,
        files: vec![TorrentFile {
            path: path("Movie/movie.mkv"),
            length: 100,
        }],
        pieces: vec![piece_hash(&correct)],
    };

    let policy = SafetyPolicy {
        auto_resume: AutoResume::WhenVerifiedComplete,
        min_match_confidence: MatchConfidence::Exact,
        ..default_policy()
    };

    let harness = Harness::with_policy_and_metadata(
        policy,
        metadata,
        &[
            ("movie.mkv", wrong),     // named the way the torrent wants, wrong bytes
            ("renamed.mkv", correct), // the right bytes, under an unrelated name
        ],
    )
    .await;

    harness.discover().await;
    let job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    // Never sat in AwaitingReview: an "exact" floor was met on the first pass.
    assert_eq!(job.state, RepairState::Seeding);

    let files = harness.store.planned_files(job.id).await.expect("files");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].confidence, Some(MatchConfidence::Exact));
    assert_eq!(files[0].evidence.map(|e| e.piece_verified), Some(true));
    assert!(
        files[0]
            .source
            .as_ref()
            .is_some_and(|source| source.ends_with("renamed.mkv"))
    );

    harness.tracker.clear_hit_and_run(&harness.torrent_id);
    let job = harness
        .run_until(10, |job| job.state == RepairState::Completed)
        .await;
    assert_eq!(job.state, RepairState::Completed);
}
