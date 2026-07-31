//! Scale characteristics: a torrent with many files and a library with many
//! more candidates than any of them.
//!
//! Not run by default — see `#[ignore]`. The acceptance criteria in
//! `docs/todos/0013-end-to-end-testing.md` ask for a recorded runtime so a
//! regression is visible, not a hard threshold tuned to one machine; run it
//! explicitly with `cargo test --test scale -- --ignored --nocapture` to see
//! the timing.

mod support;

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use chrono::Utc;
use seedmedic::{
    clock::{Clock, TestClock},
    database,
    diagnostics::Diagnostics,
    events::EventBus,
    library::{
        Candidate, CandidateError, CandidateQuery, CandidateSource,
        adapters::filesystem::FilesystemCandidateSource,
    },
    notify::adapters::noop::NoopNotifier,
    repair::{
        RepairDeps, RepairState, adapters::sqlite::SqliteRepairStore,
        application::discover_hit_and_runs, worker::WorkerHealth,
    },
    seeding::adapters::fake::FakeTorrentClient,
    staging::{StagingRoot, adapters::local::LocalStaging},
    torrent::{
        InfoHash, SafeRelativePath, TorrentFile, TorrentMetadata, adapters::fake::FakeInspector,
    },
    tracker::{
        HitAndRun, TrackerId, TrackerTorrentId,
        adapters::fake::{FakeTorrent, FakeTracker},
    },
};
use support::{default_policy, worker_for};

/// Files actually in the torrent — the doc's "a thousand-file torrent" target,
/// doubled for margin.
const TORRENT_FILES: usize = 2000;
/// Extra library files that are candidates for nothing, so matching has to
/// filter a real haystack rather than a library that happens to be exactly
/// the right size.
const NOISE_FILES: usize = 50_000 - TORRENT_FILES;

/// Wraps a real [`FilesystemCandidateSource`] to count how many times it is
/// asked — the doc's "the filesystem walk should happen once per job", made
/// checkable instead of assumed.
struct CountingCandidateSource {
    inner: FilesystemCandidateSource,
    calls: AtomicUsize,
}

#[async_trait]
impl CandidateSource for CountingCandidateSource {
    fn label(&self) -> &str {
        self.inner.label()
    }

    async fn find_candidates(
        &self,
        query: &CandidateQuery<'_>,
    ) -> Result<Vec<Candidate>, CandidateError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.find_candidates(query).await
    }
}

#[tokio::test]
#[ignore = "slow: writes 50,000 files to disk; run explicitly with `cargo test --test scale -- --ignored --nocapture`"]
async fn a_two_thousand_file_torrent_completes_against_a_fifty_thousand_file_library() {
    let library = tempfile::tempdir().expect("library tempdir");
    let staging = tempfile::tempdir().expect("staging tempdir");
    let db_dir = tempfile::tempdir().expect("db tempdir");

    std::fs::create_dir_all(library.path().join("Show")).expect("show dir");

    // Distinct lengths in a range no noise file uses, so every torrent file
    // has exactly one real candidate and matching never has to break a tie.
    let mut torrent_files = Vec::with_capacity(TORRENT_FILES);
    for index in 0..TORRENT_FILES {
        let length = 100_000 + index as u64;
        let name = format!("Show/episode-{index:05}.mkv");
        std::fs::write(
            library.path().join(&name),
            vec![index as u8; length as usize],
        )
        .expect("torrent library file");
        torrent_files.push(TorrentFile {
            path: SafeRelativePath::parse(&name).expect("valid torrent file path"),
            length,
        });
    }

    // Noise files: identical size to each other, and nowhere near the
    // 100,000+ range the torrent's files use, so candidate discovery's
    // size-based filter has real work to skip past rather than nothing.
    std::fs::create_dir_all(library.path().join("Noise")).expect("noise dir");
    for index in 0..NOISE_FILES {
        std::fs::write(
            library.path().join(format!("Noise/file-{index:05}.bin")),
            b"noise",
        )
        .expect("noise library file");
    }

    let metadata = TorrentMetadata {
        info_hash: InfoHash::from_bytes([7; 20]),
        name: SafeRelativePath::parse("Show").expect("valid torrent name"),
        piece_length: 1 << 20,
        files: torrent_files,
        pieces: Vec::new(),
    };

    let tracker_id = TrackerId::new("scale-tracker");
    let torrent_id = TrackerTorrentId::new("scale-1");
    let tracker = Arc::new(FakeTracker::new(
        tracker_id.clone(),
        vec![FakeTorrent {
            hit_and_run: HitAndRun {
                tracker: tracker_id.clone(),
                torrent_id: torrent_id.clone(),
                torrent_name: "Show".to_owned(),
                info_hash: Some(metadata.info_hash),
                size_bytes: metadata.total_length(),
                deadline: None,
                observed_at: Utc::now(),
            },
            bytes: FakeInspector::encode(&metadata),
        }],
    ));

    let clock = Arc::new(TestClock::default());
    let pool = database::connect(&db_dir.path().join("scale.sqlite3"))
        .await
        .expect("file-backed test database");
    let store = Arc::new(SqliteRepairStore::new(
        pool,
        clock.clone() as Arc<dyn Clock>,
    ));
    let client = Arc::new(FakeTorrentClient::new());

    let staging_root = StagingRoot::new(
        staging.path().to_path_buf(),
        &[library.path().to_path_buf()],
    )
    .expect("staging root");

    let counting_source = Arc::new(CountingCandidateSource {
        inner: FilesystemCandidateSource::new(library.path().to_path_buf()),
        calls: AtomicUsize::new(0),
    });

    let deps = Arc::new(RepairDeps {
        store,
        trackers: [(tracker_id, tracker.clone() as Arc<_>)]
            .into_iter()
            .collect(),
        inspector: Arc::new(FakeInspector),
        candidate_sources: vec![counting_source.clone() as Arc<dyn CandidateSource>],
        staging: Arc::new(LocalStaging::new(staging_root, 0)),
        client: client.clone(),
        clock: clock.clone(),
        policy: default_policy(),
        category: Some("seedmedic".to_owned()),
        worker_health: Arc::new(WorkerHealth::default()),
        diagnostics: Arc::new(Diagnostics::new(std::iter::empty())),
        events: Arc::new(EventBus::default()),
        client_is_stub: true,
        #[cfg(feature = "metrics")]
        metrics: Arc::new(seedmedic::metrics::Metrics::default()),
        notifier: Arc::new(NoopNotifier),
        tracker_unreachable_threshold: Duration::from_secs(1800),
    });

    discover_hit_and_runs(&deps).await;
    tracker.clear_hit_and_run(&torrent_id);

    let start = Instant::now();
    let worker = worker_for(deps.clone());
    let mut job = deps
        .store
        .jobs(1)
        .await
        .expect("jobs")
        .into_iter()
        .next()
        .expect("the discovered job");

    for _ in 0..200 {
        if job.state == RepairState::Completed {
            break;
        }
        worker.tick().await;
        clock.advance(chrono::Duration::seconds(30));
        job = deps
            .store
            .job(job.id)
            .await
            .expect("job lookup")
            .expect("job")
    }
    let elapsed = start.elapsed();

    println!(
        "scale: {TORRENT_FILES}-file torrent against a {}-file library completed in {elapsed:?}",
        TORRENT_FILES + NOISE_FILES
    );

    assert_eq!(
        job.state,
        RepairState::Completed,
        "stuck in {} (review: {:?})",
        job.state,
        job.review_reason
    );
    assert_eq!(
        counting_source.calls.load(Ordering::SeqCst),
        1,
        "the library must be walked once per job, not once per torrent file"
    );

    // Not a strict regression gate — CI hardware varies — but a two-order-
    // of-magnitude blowup from an accidental quadratic path should still fail
    // loudly rather than just quietly taking longer in CI logs nobody reads.
    assert!(
        elapsed < Duration::from_secs(180),
        "a {TORRENT_FILES}-file repair against a {}-file library took {elapsed:?}; \
         that is far enough outside historical norms to suspect an accidental \
         quadratic path in matching or staging",
        TORRENT_FILES + NOISE_FILES
    );
}
