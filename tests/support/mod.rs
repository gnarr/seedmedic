// Each integration test file compiles this module separately, so anything only
// one of them uses looks dead to the others.
#![allow(dead_code)]

//! Shared harness for the integration tests.
//!
//! Real SQLite store, real local staging over temporary directories, real
//! filesystem candidate discovery — fakes only where the outside world would
//! be. That mix is deliberate: the parts most likely to be wrong (persistence,
//! path handling, materialisation) are exercised for real.

pub mod fail_at;

use std::{sync::Arc, time::Duration};

use chrono::Utc;
use seedmedic::{
    clock::{Clock, TestClock},
    database,
    diagnostics::Diagnostics,
    library::{CandidateSource, MatchConfidence, adapters::filesystem::FilesystemCandidateSource},
    notify::adapters::noop::NoopNotifier,
    repair::{
        AutoResume, JobId, MaterializationPolicy, RepairDeps, RepairJob, RepairStore, SafetyPolicy,
        WorkerConfig,
        adapters::sqlite::SqliteRepairStore,
        worker::{RepairWorker, TickSummary, WorkerHealth},
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
use tempfile::TempDir;

pub const OWNER: &str = "test-worker";

/// A generous `/health` staleness threshold for tests that don't care about
/// exercising it.
pub const HEALTH_THRESHOLD: Duration = Duration::from_secs(3600);

/// [`seedmedic::web::router`] with defaults for the arguments most tests
/// don't care about.
pub fn router(
    deps: Arc<seedmedic::repair::RepairDeps>,
    auth_token: Option<String>,
) -> axum::Router {
    seedmedic::web::router(deps, auth_token, HEALTH_THRESHOLD, String::new(), false)
}

/// The torrent every test repairs: two episodes, both present in the library
/// under matching names, so matching reaches `Probable` without help.
pub struct Harness {
    pub deps: Arc<RepairDeps>,
    pub store: Arc<SqliteRepairStore>,
    pub tracker: Arc<FakeTracker>,
    pub client: Arc<FakeTorrentClient>,
    pub clock: Arc<TestClock>,
    pub torrent_id: TrackerTorrentId,
    pub info_hash: InfoHash,
    pub staging_root: std::path::PathBuf,
    _library: TempDir,
    _staging: TempDir,
}

impl Harness {
    pub async fn new() -> Self {
        Self::with_policy(default_policy()).await
    }

    pub async fn with_policy(policy: SafetyPolicy) -> Self {
        Self::with_policy_and_metadata(
            policy,
            torrent_metadata(),
            &[("e01.mkv", vec![b'a'; 1000]), ("e02.mkv", vec![b'b'; 2000])],
        )
        .await
    }

    /// Like [`Harness::with_policy`], but with a caller-chosen torrent and
    /// library contents — for scenarios `torrent_metadata`'s fixed two files
    /// cannot express, such as piece verification.
    pub async fn with_policy_and_metadata(
        policy: SafetyPolicy,
        metadata: TorrentMetadata,
        library_files: &[(&str, Vec<u8>)],
    ) -> Self {
        Self::with_policy_metadata_and_deadline(policy, metadata, library_files, None).await
    }

    /// Like [`Harness::with_policy_and_metadata`], but with the tracker's
    /// hit-and-run deadline set from discovery — for the deadline-awareness
    /// scenarios in `seeding_monitoring.rs`.
    pub async fn with_policy_metadata_and_deadline(
        policy: SafetyPolicy,
        metadata: TorrentMetadata,
        library_files: &[(&str, Vec<u8>)],
        deadline: Option<chrono::DateTime<Utc>>,
    ) -> Self {
        let library = tempfile::tempdir().expect("library tempdir");
        let staging = tempfile::tempdir().expect("staging tempdir");

        for (name, content) in library_files {
            std::fs::write(library.path().join(name), content).expect("library file");
        }

        let info_hash = metadata.info_hash;
        let tracker_id = TrackerId::new("test-tracker");
        let torrent_id = TrackerTorrentId::new("t-1");

        let tracker = Arc::new(FakeTracker::new(
            tracker_id.clone(),
            vec![FakeTorrent {
                hit_and_run: HitAndRun {
                    tracker: tracker_id.clone(),
                    torrent_id: torrent_id.clone(),
                    torrent_name: "Demo.Show.S01".to_owned(),
                    info_hash: Some(info_hash),
                    size_bytes: metadata.total_length(),
                    deadline,
                    observed_at: Utc::now(),
                },
                bytes: FakeInspector::encode(&metadata),
            }],
        ));

        let clock = Arc::new(TestClock::default());
        let store = Arc::new(SqliteRepairStore::new(
            database::test_pool().await,
            clock.clone() as Arc<dyn Clock>,
        ));
        let client = Arc::new(FakeTorrentClient::new());

        let staging_root = StagingRoot::new(
            staging.path().to_path_buf(),
            &[library.path().to_path_buf()],
        )
        .expect("staging root");
        let staging_path = staging_root.path().to_path_buf();

        let candidate_sources: Vec<Arc<dyn CandidateSource>> = vec![Arc::new(
            FilesystemCandidateSource::new(library.path().to_path_buf()),
        )];

        let deps = Arc::new(RepairDeps {
            store: store.clone(),
            trackers: [(tracker_id, tracker.clone() as Arc<_>)]
                .into_iter()
                .collect(),
            inspector: Arc::new(FakeInspector),
            candidate_sources,
            staging: Arc::new(LocalStaging::new(staging_root, 0)),
            client: client.clone(),
            clock: clock.clone(),
            policy,
            category: Some("seedmedic".to_owned()),
            worker_health: Arc::new(WorkerHealth::default()),
            diagnostics: Arc::new(Diagnostics::new(std::iter::empty())),
            client_is_stub: true,
            #[cfg(feature = "metrics")]
            metrics: Arc::new(seedmedic::metrics::Metrics::default()),
            notifier: Arc::new(NoopNotifier),
            tracker_unreachable_threshold: Duration::from_secs(1800),
        });

        Self {
            deps,
            store,
            tracker,
            client,
            clock,
            torrent_id,
            info_hash,
            staging_root: staging_path,
            _library: library,
            _staging: staging,
        }
    }

    pub fn worker(&self) -> RepairWorker {
        RepairWorker::new(self.deps.clone(), worker_config())
    }

    /// Discover the fake tracker's warning and return the job it created.
    pub async fn discover(&self) -> RepairJob {
        self.worker().discover().await;
        self.only_job().await
    }

    pub async fn only_job(&self) -> RepairJob {
        let jobs = self.store.jobs(10).await.expect("jobs");
        assert_eq!(jobs.len(), 1, "the harness only ever has one job");
        jobs.into_iter().next().expect("one job")
    }

    pub async fn job(&self, id: JobId) -> RepairJob {
        self.store.job(id).await.expect("job lookup").expect("job")
    }

    /// Run ticks, advancing the clock between them, until `done` or `max_ticks`.
    ///
    /// The clock has to move because `Wait` outcomes schedule the next attempt
    /// in the future — which is the behaviour under test, not an inconvenience.
    pub async fn run_until(
        &self,
        max_ticks: usize,
        done: impl Fn(&RepairJob) -> bool,
    ) -> RepairJob {
        self.run_until_with(&self.worker(), max_ticks, done).await
    }

    /// Like [`Harness::run_until`], but driven by a caller-supplied worker —
    /// for tests that need a worker over a decorated store or a different
    /// config, while still sharing the harness's clock and job lookups.
    pub async fn run_until_with(
        &self,
        worker: &RepairWorker,
        max_ticks: usize,
        done: impl Fn(&RepairJob) -> bool,
    ) -> RepairJob {
        let mut job = self.only_job().await;

        for _ in 0..max_ticks {
            if done(&job) {
                return job;
            }
            worker.tick().await;
            self.clock.advance(chrono::Duration::seconds(30));
            job = self.job(job.id).await;
        }

        assert!(
            done(&job),
            "job did not reach the expected state within {max_ticks} ticks (stuck in {}, review: {:?})",
            job.state,
            job.review_reason
        );
        job
    }

    pub async fn tick(&self) -> TickSummary {
        self.worker().tick().await
    }

    /// Tick once against a different policy, reusing the same store, tracker,
    /// and client — for tests where a job must reach a state under one policy
    /// (e.g. `auto_resume = when_verified_complete`, to get to `Seeding` at
    /// all) and then be driven under another.
    pub async fn tick_with_policy(&self, policy: SafetyPolicy) -> TickSummary {
        let deps = Arc::new(RepairDeps {
            store: self.deps.store.clone(),
            trackers: self.deps.trackers.clone(),
            inspector: self.deps.inspector.clone(),
            candidate_sources: self.deps.candidate_sources.clone(),
            staging: self.deps.staging.clone(),
            client: self.deps.client.clone(),
            clock: self.deps.clock.clone(),
            policy,
            category: self.deps.category.clone(),
            worker_health: self.deps.worker_health.clone(),
            diagnostics: self.deps.diagnostics.clone(),
            client_is_stub: self.deps.client_is_stub,
            #[cfg(feature = "metrics")]
            metrics: self.deps.metrics.clone(),
            notifier: self.deps.notifier.clone(),
            tracker_unreachable_threshold: self.deps.tracker_unreachable_threshold,
        });
        RepairWorker::new(deps, worker_config()).tick().await
    }

    /// A worker over the same store, tracker, and client, but with a
    /// caller-chosen notifier — for tests that need to observe what gets
    /// sent, since the harness otherwise wires up a no-op.
    pub fn worker_with_notifier(
        &self,
        notifier: Arc<dyn seedmedic::notify::Notifier>,
    ) -> RepairWorker {
        RepairWorker::new(
            self.deps_with_store(self.deps.store.clone(), notifier),
            worker_config(),
        )
    }

    /// A worker over a caller-chosen store — everything else unchanged — for
    /// tests that substitute a decorator like [`fail_at::FailAt`] in front of
    /// the real one.
    pub fn worker_with_store(&self, store: Arc<dyn RepairStore>) -> RepairWorker {
        RepairWorker::new(
            self.deps_with_store(store, self.deps.notifier.clone()),
            worker_config(),
        )
    }

    fn deps_with_store(
        &self,
        store: Arc<dyn RepairStore>,
        notifier: Arc<dyn seedmedic::notify::Notifier>,
    ) -> Arc<RepairDeps> {
        Arc::new(RepairDeps {
            store,
            trackers: self.deps.trackers.clone(),
            inspector: self.deps.inspector.clone(),
            candidate_sources: self.deps.candidate_sources.clone(),
            staging: self.deps.staging.clone(),
            client: self.deps.client.clone(),
            clock: self.deps.clock.clone(),
            policy: self.deps.policy,
            category: self.deps.category.clone(),
            worker_health: self.deps.worker_health.clone(),
            diagnostics: self.deps.diagnostics.clone(),
            client_is_stub: self.deps.client_is_stub,
            #[cfg(feature = "metrics")]
            metrics: self.deps.metrics.clone(),
            notifier,
            tracker_unreachable_threshold: self.deps.tracker_unreachable_threshold,
        })
    }
}

pub fn worker_config() -> WorkerConfig {
    WorkerConfig {
        owner: OWNER.to_owned(),
        lease: Duration::from_secs(300),
        batch_size: 4,
        poll_interval: Duration::from_secs(1),
        discovery_interval: Duration::from_secs(1),
    }
}

/// Permissive enough to reach `Completed` unattended, so the tests exercise the
/// whole lifecycle rather than stopping at the first safety gate.
pub fn default_policy() -> SafetyPolicy {
    SafetyPolicy {
        auto_resume: AutoResume::WhenVerifiedComplete,
        min_match_confidence: MatchConfidence::Probable,
        verification_pieces: 3,
        // Reflink first on purpose: it is unimplemented, so every run also
        // proves the fall-through to the next permitted strategy works.
        materialization: MaterializationPolicy {
            prefer_reflink: true,
            allow_hardlink: false,
            allow_copy: true,
        },
        max_attempts: 3,
        retry_base_delay: Duration::from_secs(1),
        retry_max_delay: Duration::from_secs(5),
        recheck_poll_interval: Duration::from_secs(1),
        recheck_poll_max_interval: Duration::from_secs(10),
        // Comfortably longer than the coarse 30-second ticks `run_until`
        // advances the clock by, so a recheck that finishes in a poll or two
        // is never mistaken for one that is stuck.
        recheck_timeout: Duration::from_secs(300),
        tracker_poll_interval: Duration::from_secs(1),
        tracker_poll_min_interval: Duration::from_millis(100),
        max_consecutive_unknown_tracker_status: 20,
    }
}

pub fn torrent_metadata() -> TorrentMetadata {
    TorrentMetadata {
        info_hash: InfoHash::from_bytes([9; 20]),
        name: path("Demo.Show.S01"),
        piece_length: 1 << 16,
        files: vec![
            TorrentFile {
                path: path("Demo.Show.S01/e01.mkv"),
                length: 1000,
            },
            TorrentFile {
                path: path("Demo.Show.S01/e02.mkv"),
                length: 2000,
            },
        ],
        // `piece_length` is larger than the whole torrent, so the single piece
        // spans both files and verification never applies to either — these
        // tests exercise the size/name path, not piece verification.
        pieces: Vec::new(),
    }
}

pub fn path(value: &str) -> SafeRelativePath {
    SafeRelativePath::parse(value).expect("test path is valid")
}

/// [`TestClock::default`]'s fixed starting instant, for tests that need to
/// express a hit-and-run deadline relative to it before the harness (and
/// therefore the clock) exists.
pub fn clock_epoch() -> chrono::DateTime<Utc> {
    chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
        .expect("valid fixed timestamp")
        .with_timezone(&Utc)
}
