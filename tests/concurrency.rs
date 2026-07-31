//! Many jobs, contended for by more than one claimant.
//!
//! Every other integration test drives exactly one job, so nothing else in
//! this suite proves that `RepairStore::claim` is actually exclusive under
//! real concurrency rather than under the accidentally-serial execution a
//! single `#[tokio::test]` task gives you for free. This file drives twenty
//! independent repairs — different info-hashes, different library files, so
//! nothing about matching collides between them — through two workers with
//! different owners racing over the same real SQLite file, on a
//! multi-threaded runtime so the race is genuine.

mod support;

use std::{sync::Arc, time::Duration};

use chrono::Utc;
use seedmedic::{
    clock::{Clock, TestClock},
    database,
    diagnostics::Diagnostics,
    events::EventBus,
    library::{CandidateSource, adapters::filesystem::FilesystemCandidateSource},
    notify::adapters::noop::NoopNotifier,
    repair::{
        RepairDeps, RepairJob, RepairState, WorkerConfig,
        adapters::sqlite::SqliteRepairStore,
        application::discover_hit_and_runs,
        worker::{RepairWorker, WorkerHealth},
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
use support::default_policy;

const JOB_COUNT: usize = 20;

/// A library and a tracker with `JOB_COUNT` independent, single-file
/// torrents, backed by a real SQLite file so multiple real connections can
/// contend for it.
struct Fleet {
    deps: Arc<RepairDeps>,
    clock: Arc<TestClock>,
    client: Arc<FakeTorrentClient>,
    tracker: Arc<FakeTracker>,
    _library: tempfile::TempDir,
    _staging: tempfile::TempDir,
    _db: tempfile::TempDir,
}

impl Fleet {
    async fn build(count: usize) -> Self {
        let library = tempfile::tempdir().expect("library tempdir");
        let staging = tempfile::tempdir().expect("staging tempdir");
        let db_dir = tempfile::tempdir().expect("db tempdir");

        let tracker_id = TrackerId::new("concurrency-tracker");
        let mut torrents = Vec::with_capacity(count);

        for index in 0..count {
            // A distinct length per job, not just a distinct name: candidate
            // matching filters by exact size before anything else, so a
            // shared size would let one job's library file get offered as a
            // candidate for another's torrent.
            let length = 1000 + index as u64;
            let show = format!("Show.{index:03}");
            let file_path = format!("{show}/episode.mkv");

            std::fs::create_dir_all(library.path().join(&show)).expect("library show dir");
            std::fs::write(
                library.path().join(&file_path),
                vec![index as u8; length as usize],
            )
            .expect("library file");

            let metadata = TorrentMetadata {
                info_hash: InfoHash::from_bytes([index as u8 + 1; 20]),
                name: SafeRelativePath::parse(&show).expect("valid torrent name"),
                piece_length: 1 << 16,
                files: vec![TorrentFile {
                    path: SafeRelativePath::parse(&file_path).expect("valid torrent file path"),
                    length,
                }],
                pieces: Vec::new(),
            };

            torrents.push(FakeTorrent {
                hit_and_run: HitAndRun {
                    tracker: tracker_id.clone(),
                    torrent_id: TrackerTorrentId::new(format!("t-{index}")),
                    torrent_name: show,
                    info_hash: Some(metadata.info_hash),
                    size_bytes: metadata.total_length(),
                    deadline: None,
                    observed_at: Utc::now(),
                },
                bytes: FakeInspector::encode(&metadata),
            });
        }

        let tracker = Arc::new(FakeTracker::new(tracker_id.clone(), torrents));
        let clock = Arc::new(TestClock::default());
        // A real file, not `:memory:`: two workers each need their own
        // connection to genuinely race, and an in-memory database only ever
        // has the one connection behind it.
        let pool = database::connect(&db_dir.path().join("concurrency.sqlite3"))
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
        let candidate_sources: Vec<Arc<dyn CandidateSource>> = vec![Arc::new(
            FilesystemCandidateSource::new(library.path().to_path_buf()),
        )];

        let deps = Arc::new(RepairDeps {
            store,
            trackers: [(tracker_id, tracker.clone() as Arc<_>)]
                .into_iter()
                .collect(),
            inspector: Arc::new(FakeInspector),
            candidate_sources,
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

        Self {
            deps,
            clock,
            client,
            tracker,
            _library: library,
            _staging: staging,
            _db: db_dir,
        }
    }

    /// A fresh worker over the shared deps. Built anew per call — cheap,
    /// since it is just an `Arc` clone and a small config struct — so a
    /// caller can hand one to `tokio::spawn`, which needs an owned, `'static`
    /// future rather than one borrowing a shared worker.
    fn worker(&self, owner: &str, batch_size: i64) -> RepairWorker {
        RepairWorker::new(
            self.deps.clone(),
            WorkerConfig {
                owner: owner.to_owned(),
                lease: Duration::from_secs(300),
                batch_size,
                poll_interval: Duration::from_secs(1),
                discovery_interval: Duration::from_secs(1),
            },
        )
    }

    async fn jobs(&self) -> Vec<RepairJob> {
        self.deps
            .store
            .jobs((JOB_COUNT * 2) as i64)
            .await
            .expect("jobs")
    }

    fn all_completed(jobs: &[RepairJob]) -> bool {
        jobs.len() == JOB_COUNT && jobs.iter().all(|job| job.state == RepairState::Completed)
    }
}

/// Drive workers built from `configs` (`(owner, batch_size)` pairs)
/// concurrently, advancing the clock between rounds, until every job is
/// `Completed` or `max_rounds` is spent.
///
/// Each round spawns a fresh, owned `RepairWorker` per config onto its own
/// task — genuinely concurrent under the multi-threaded runtime, which is the
/// only way two claimants can actually race for the same job rather than
/// merely interleave.
async fn drive_to_completion(fleet: &Fleet, configs: &[(&str, i64)], max_rounds: usize) {
    for _ in 0..max_rounds {
        let jobs = fleet.jobs().await;
        if Fleet::all_completed(&jobs) {
            return;
        }

        let handles: Vec<_> = configs
            .iter()
            .map(|(owner, batch_size)| {
                let worker = fleet.worker(owner, *batch_size);
                tokio::spawn(async move { worker.tick().await })
            })
            .collect();
        for handle in handles {
            handle.await.expect("worker tick task panicked");
        }

        fleet.clock.advance(chrono::Duration::seconds(30));
    }

    let jobs = fleet.jobs().await;
    assert!(
        Fleet::all_completed(&jobs),
        "not every job reached Completed within {max_rounds} rounds: {:?}",
        jobs.iter()
            .map(|job| (job.id, job.state, job.review_reason))
            .collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_complete_twenty_jobs_exactly_once_each_with_no_duplicated_adds() {
    let fleet = Fleet::build(JOB_COUNT).await;
    discover_hit_and_runs(&fleet.deps).await;

    // The invariant under test is exclusive claiming and non-duplicated side
    // effects, not tracker timing — clear every warning up front so nothing
    // sits in `Seeding` waiting on the tracker.
    for job in fleet.jobs().await {
        fleet.tracker.clear_hit_and_run(&job.torrent_id);
    }

    let configs = [("worker-a", 4), ("worker-b", 4)];
    drive_to_completion(&fleet, &configs, 60).await;

    let jobs = fleet.jobs().await;
    assert_eq!(jobs.len(), JOB_COUNT);
    assert!(
        jobs.iter().all(|job| job.state == RepairState::Completed),
        "every job must complete exactly once"
    );
    assert_eq!(
        fleet.client.add_count(),
        JOB_COUNT,
        "no job's torrent was added more than once, however the two workers split the claims"
    );
    assert_eq!(fleet.client.recheck_count(), JOB_COUNT);
    assert_eq!(fleet.client.resume_count(), JOB_COUNT);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_worker_with_a_larger_batch_completes_twenty_jobs_exactly_once_each() {
    let fleet = Fleet::build(JOB_COUNT).await;
    discover_hit_and_runs(&fleet.deps).await;

    for job in fleet.jobs().await {
        fleet.tracker.clear_hit_and_run(&job.torrent_id);
    }

    let configs = [("solo-worker", 8)];
    drive_to_completion(&fleet, &configs, 60).await;

    let jobs = fleet.jobs().await;
    assert_eq!(jobs.len(), JOB_COUNT);
    assert!(
        jobs.iter().all(|job| job.state == RepairState::Completed),
        "every job must complete exactly once"
    );
    assert_eq!(fleet.client.add_count(), JOB_COUNT);
    assert_eq!(fleet.client.recheck_count(), JOB_COUNT);
    assert_eq!(fleet.client.resume_count(), JOB_COUNT);
}
