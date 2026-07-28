//! The full repair workflow against a real qBittorrent — no fake for the
//! download client. Every other integration test in this suite proves the
//! workflow against `FakeTorrentClient`; this one proves the port contract
//! `FakeTorrentClient` models is actually what qBittorrent does.
//!
//! Opt-in twice over: `#[ignore]`, so a bare `cargo test` never touches the
//! network, and gated at runtime on `SEEDMEDIC_QBITTORRENT_URL`, so running
//! with `--ignored` by accident skips cleanly instead of failing to connect.
//! See `docker-compose.test.yml` for a disposable instance to point this at.
//!
//! The tracker stays fake — pointing a test suite at a real private tracker
//! is out of scope (see the "Out of scope" section of
//! `docs/todos/0013-end-to-end-testing.md`) — but the `.torrent` bytes are
//! real bencode, hand-built here, because the point of this test is that a
//! real qBittorrent accepts and hash-checks them, which JSON from
//! `FakeInspector` never could.

mod support;

use std::{path::PathBuf, sync::Arc, time::Duration};

use chrono::Utc;
use seedmedic::{
    clock::SystemClock,
    config::Secret,
    database,
    diagnostics::Diagnostics,
    library::{CandidateSource, adapters::filesystem::FilesystemCandidateSource},
    notify::adapters::noop::NoopNotifier,
    repair::{
        RepairDeps, RepairState, adapters::sqlite::SqliteRepairStore,
        application::discover_hit_and_runs, worker::WorkerHealth,
    },
    seeding::{TorrentClient, adapters::qbittorrent::QBittorrentClient},
    staging::{StagingRoot, adapters::local::LocalStaging},
    torrent::{InfoHash, adapters::bencode::BencodeInspector},
    tracker::{
        HitAndRun, TrackerId, TrackerTorrentId,
        adapters::fake::{FakeTorrent, FakeTracker},
    },
};
use sha1::{Digest, Sha1};
use support::default_policy;
use url::Url;

/// What the test needs from the environment; absent means "skip", since this
/// test must never run by accident against nothing.
struct LiveConfig {
    url: Url,
    username: String,
    password: Secret,
    staging_dir: PathBuf,
}

impl LiveConfig {
    fn from_env() -> Option<Self> {
        let url = std::env::var("SEEDMEDIC_QBITTORRENT_URL").ok()?;
        let username =
            std::env::var("SEEDMEDIC_QBITTORRENT_USERNAME").unwrap_or_else(|_| "admin".to_owned());
        let password = std::env::var("SEEDMEDIC_QBITTORRENT_PASSWORD").expect(
            "SEEDMEDIC_QBITTORRENT_PASSWORD must be set alongside SEEDMEDIC_QBITTORRENT_URL",
        );
        // Must be the exact path docker-compose.test.yml bind-mounts into the
        // qBittorrent container: SeedMedic writes staged files from the host
        // side, and qBittorrent reads them from the container side, so the
        // two only agree on a save_path if it is the same absolute string in
        // both places.
        let staging_dir = std::env::var("SEEDMEDIC_QBITTORRENT_STAGING_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp/seedmedic-live-qbittorrent-staging"));

        Some(Self {
            url: Url::parse(&url).expect("SEEDMEDIC_QBITTORRENT_URL must be a valid URL"),
            username,
            password: Secret::new(password),
            staging_dir,
        })
    }
}

/// A minimal, real, single-file, single-piece `.torrent`. Hand-bencoded
/// rather than produced by any SeedMedic code, so this test does not just
/// check SeedMedic against itself.
fn build_torrent(name: &str, content: &[u8]) -> (Vec<u8>, InfoHash) {
    fn string(bytes: &[u8], out: &mut Vec<u8>) {
        out.extend(bytes.len().to_string().as_bytes());
        out.push(b':');
        out.extend(bytes);
    }
    fn int(value: i64, out: &mut Vec<u8>) {
        out.push(b'i');
        out.extend(value.to_string().as_bytes());
        out.push(b'e');
    }

    // One piece, no bigger than the whole (small) file, so the piece hash is
    // simply the file's own SHA-1 with no padding to reason about.
    let piece_length: i64 = (content.len() as i64).max(1);
    let piece_hash: [u8; 20] = Sha1::digest(content).into();

    let mut info = vec![b'd'];
    string(b"length", &mut info);
    int(content.len() as i64, &mut info);
    string(b"name", &mut info);
    string(name.as_bytes(), &mut info);
    string(b"piece length", &mut info);
    int(piece_length, &mut info);
    string(b"pieces", &mut info);
    string(&piece_hash, &mut info);
    info.push(b'e');

    let info_hash = InfoHash::from_bytes(Sha1::digest(&info).into());

    let mut torrent = vec![b'd'];
    string(b"announce", &mut torrent);
    // Port 1 on loopback: nothing listens there, so an announce attempt fails
    // instantly instead of hanging on a real network timeout.
    string(b"http://127.0.0.1:1/announce", &mut torrent);
    string(b"info", &mut torrent);
    torrent.extend(info);
    torrent.push(b'e');

    (torrent, info_hash)
}

#[tokio::test]
#[ignore = "opt-in: needs a real qBittorrent — see docker-compose.test.yml"]
async fn the_full_workflow_completes_against_a_real_qbittorrent() {
    // Quiet by default; set RUST_LOG=debug (with --nocapture) to see the
    // worker's own steps and every HTTP call this makes against qBittorrent.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    let Some(config) = LiveConfig::from_env() else {
        eprintln!(
            "skipping: SEEDMEDIC_QBITTORRENT_URL is not set — bring up docker-compose.test.yml \
             and see its comments for the rest of the environment this test needs"
        );
        return;
    };

    let library = tempfile::tempdir().expect("library tempdir");
    std::fs::create_dir_all(&config.staging_dir).expect(
        "the staging directory must already exist and be writable — docker-compose.test.yml \
         bind-mounts it into the qBittorrent container",
    );

    let content = b"seedmedic live qbittorrent integration test payload";
    let file_name = "seedmedic-live-test.bin";
    std::fs::write(library.path().join(file_name), content).expect("library file");
    let (torrent_bytes, info_hash) = build_torrent(file_name, content);

    let tracker_id = TrackerId::new("live-test-tracker");
    let torrent_id = TrackerTorrentId::new("live-1");
    let tracker = Arc::new(FakeTracker::new(
        tracker_id.clone(),
        vec![FakeTorrent {
            hit_and_run: HitAndRun {
                tracker: tracker_id.clone(),
                torrent_id: torrent_id.clone(),
                torrent_name: file_name.to_owned(),
                info_hash: Some(info_hash),
                size_bytes: content.len() as u64,
                deadline: None,
                observed_at: Utc::now(),
            },
            bytes: torrent_bytes,
        }],
    ));

    let client = Arc::new(QBittorrentClient::new(
        config.url,
        config.username,
        config.password,
        reqwest::Client::new(),
    ));

    // Clean up any leftover torrent of the same hash from a previous run of
    // this test against a qBittorrent instance whose config volume persists
    // between `docker compose up`s.
    let _ = client.remove(info_hash, false).await;

    let staging_root =
        StagingRoot::new(config.staging_dir.clone(), &[library.path().to_path_buf()])
            .expect("staging root");
    let candidate_sources: Vec<Arc<dyn CandidateSource>> = vec![Arc::new(
        FilesystemCandidateSource::new(library.path().to_path_buf()),
    )];

    let deps = Arc::new(RepairDeps {
        store: Arc::new(SqliteRepairStore::new(
            database::test_pool().await,
            Arc::new(SystemClock),
        )),
        trackers: [(tracker_id, tracker.clone() as Arc<_>)]
            .into_iter()
            .collect(),
        inspector: Arc::new(BencodeInspector),
        candidate_sources,
        staging: Arc::new(LocalStaging::new(staging_root, 0)),
        client: client.clone() as Arc<dyn TorrentClient>,
        clock: Arc::new(SystemClock),
        policy: default_policy(),
        category: None,
        worker_health: Arc::new(WorkerHealth::default()),
        diagnostics: Arc::new(Diagnostics::new(std::iter::empty())),
        client_is_stub: false,
        #[cfg(feature = "metrics")]
        metrics: Arc::new(seedmedic::metrics::Metrics::default()),
        notifier: Arc::new(NoopNotifier),
        tracker_unreachable_threshold: Duration::from_secs(1800),
    });

    discover_hit_and_runs(&deps).await;
    tracker.clear_hit_and_run(&torrent_id);

    let worker = support::worker_for(deps.clone());
    let mut job = deps
        .store
        .jobs(1)
        .await
        .expect("jobs")
        .into_iter()
        .next()
        .expect("the discovered job");

    // Real time, not `TestClock`: nothing here can fast-forward a real
    // qBittorrent's own hash check.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while job.state != RepairState::Completed && std::time::Instant::now() < deadline {
        worker.tick().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        job = deps
            .store
            .job(job.id)
            .await
            .expect("job lookup")
            .expect("job");
    }

    let cleanup = client.remove(info_hash, false).await;

    assert_eq!(
        job.state,
        RepairState::Completed,
        "stuck in {} (review: {:?}) against the real qBittorrent at the configured URL",
        job.state,
        job.review_reason
    );
    cleanup.expect("cleaning up the test torrent after a successful run");
}
