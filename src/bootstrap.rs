//! Wiring. The only place that knows which adapter implements which port.
//!
//! Split in two, so a configuration reload (see `src/runtime.rs`) can be "the
//! existing startup sequence, run again" without reopening the database:
//!
//! - [`open`] runs once per process. It is the only place that does network or
//!   database I/O, and everything it produces — [`Persistent`] — outlives
//!   every reload.
//! - [`build`] wires one generation from a [`Config`] and a [`Persistent`]. It
//!   is synchronous: every adapter it constructs is plain Rust construction,
//!   so a reload cannot hang or need a timeout.

use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};

use crate::{
    clock::{Clock, SystemClock},
    config::{ArrKind, Config, DownloadClientKind, Secret, Severity, TrackerConfig, TrackerKind},
    database,
    diagnostics::Diagnostics,
    library::{
        CandidateSource,
        adapters::{
            arr::{ArrCandidateSource, ArrKind as AdapterArrKind, PathMapping},
            filesystem::FilesystemCandidateSource,
        },
    },
    notify::{
        Notifier,
        adapters::{noop::NoopNotifier, webhook::WebhookNotifier},
    },
    repair::{
        RepairDeps, RepairStore, WorkerConfig, WorkerHealth, adapters::sqlite::SqliteRepairStore,
    },
    seeding::{TorrentClient, adapters::qbittorrent::QBittorrentClient},
    staging::{
        StagingFilesystem, StagingRoot,
        adapters::{local::LocalStaging, unconfigured::UnconfiguredStaging},
    },
    torrent::{TorrentInspector, adapters::bencode::BencodeInspector},
    tracker::{TrackerClient, TrackerId, adapters::unit3d::Unit3dTracker},
};

/// Opened once per process. Everything here outlives every reload: the
/// database connection, so `database.path` can never change without a
/// restart; the clock; and the two pieces of operational state a reload must
/// never reset — [`WorkerHealth`] (or `/health` dips on every settings save)
/// and [`Diagnostics`] (or an operator loses the error history they were
/// looking at when they changed a setting).
pub struct Persistent {
    pub store: Arc<dyn RepairStore>,
    pub clock: Arc<dyn Clock>,
    pub worker_health: Arc<WorkerHealth>,
    pub diagnostics: Arc<Diagnostics>,
    #[cfg(feature = "metrics")]
    pub metrics: Arc<crate::metrics::Metrics>,
}

/// Open the database and create the state that survives every reload.
pub async fn open(config: &Config) -> Result<Persistent> {
    let pool = database::connect(&config.database.path).await?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let store = Arc::new(SqliteRepairStore::new(pool, clock.clone()));

    Ok(Persistent {
        store,
        clock,
        worker_health: Arc::new(WorkerHealth::default()),
        diagnostics: Arc::new(Diagnostics::default()),
        #[cfg(feature = "metrics")]
        metrics: Arc::new(crate::metrics::Metrics::default()),
    })
}

/// Everything one configuration produces. Replaced wholesale on a reload;
/// never mutated, so a request that started against generation N finishes
/// against generation N even if N+1 lands mid-request.
pub struct Runtime {
    pub deps: Arc<RepairDeps>,
    /// How long `/health` tolerates the worker having gone quiet before
    /// reporting unready. Derived from `worker.poll_interval` with margin
    /// rather than hard-coded, so a slower configured interval does not
    /// immediately look unhealthy.
    pub health_threshold: Duration,
    pub auth_token: Option<Secret>,
    /// The effective configuration, secrets redacted, for the `/status` page.
    pub config_summary: Arc<str>,
    /// Whether `/metrics` should serve anything. Harmless without the
    /// `metrics` feature — see `crate::metrics`.
    pub metrics_enabled: bool,
    /// The setup banner every page shows until nothing is left to configure.
    pub chrome: crate::web::Chrome,
    /// The configuration this generation was built from, kept so the next
    /// reload can tell what changed — see `RuntimeHandle::reload`'s refusal
    /// checks and its `Applied::restart_needed` report. Never rendered raw;
    /// `config_summary` is what templates use.
    pub config: Arc<Config>,
}

/// Wire one generation. Synchronous: nothing here does network or database
/// I/O — `database::connect` was `bootstrap::build`'s only `await`, and
/// everything else (the HTTP client, the trackers, the client, the candidate
/// sources, `StagingRoot::new`) is already sync. No timeout question, no
/// cancellation question, so a reload cannot hang.
///
/// `config_path` is only used to render the setup-banner's displayed path; it
/// does no I/O of its own beyond `std::path::absolute`, which never touches
/// the filesystem.
pub fn build(
    config: &Config,
    persistent: &Persistent,
    config_path: &Path,
) -> Result<(Runtime, WorkerConfig)> {
    config.validate()?;

    // Validated here rather than trusted later: this is what guarantees no
    // repair can ever write inside the media library. An empty
    // `staging.root` is a fresh install, not a misconfiguration — wire an
    // adapter that parks any repair reaching it for review instead of
    // guessing a path.
    let staging: Arc<dyn StagingFilesystem> = if config.staging.root.as_os_str().is_empty() {
        Arc::new(UnconfiguredStaging)
    } else {
        let staging_root = StagingRoot::new(config.staging.root.clone(), &config.library.roots)
            .context("staging root is not usable")?;
        Arc::new(LocalStaging::new(
            staging_root,
            config.staging.min_free_bytes,
        ))
    };

    let trackers = build_trackers(&config.trackers)?;
    let inspector = build_inspector(&config.trackers);
    let client = build_client(config)?;
    let candidate_sources = build_candidate_sources(config)?;
    let worker_config = config.worker.to_worker_config();

    persistent
        .diagnostics
        .reseed(config.trackers.iter().map(|tracker| {
            (
                TrackerId::new(&tracker.id),
                tracker.kind == TrackerKind::Fake,
            )
        }));

    let client_is_stub = config
        .download_client
        .as_ref()
        .is_some_and(|download_client| download_client.kind == DownloadClientKind::Fake);
    let notifier: Arc<dyn Notifier> = match &config.notifications.webhook_url {
        Some(url) => Arc::new(WebhookNotifier::new(url.clone(), build_http_client()?)),
        None => Arc::new(NoopNotifier),
    };

    let setup_warnings: Vec<String> = config
        .problems()
        .into_iter()
        .filter(|problem| problem.severity == Severity::Warning)
        .map(|problem| problem.message)
        .collect();
    let displayed_config_path = std::path::absolute(config_path)
        .unwrap_or_else(|_| config_path.to_path_buf())
        .display()
        .to_string();
    let auth_token_set = !config.server.auth_token.is_empty();
    let chrome = crate::web::Chrome::new(displayed_config_path, setup_warnings, auth_token_set);

    let deps = Arc::new(RepairDeps {
        store: persistent.store.clone(),
        trackers,
        inspector,
        candidate_sources,
        staging,
        client,
        clock: persistent.clock.clone(),
        policy: config.policy.to_policy(),
        category: config
            .download_client
            .as_ref()
            .and_then(|download_client| download_client.category.clone()),
        worker_health: persistent.worker_health.clone(),
        diagnostics: persistent.diagnostics.clone(),
        client_is_stub,
        #[cfg(feature = "metrics")]
        metrics: persistent.metrics.clone(),
        notifier,
        tracker_unreachable_threshold: Duration::from_secs(
            config.notifications.tracker_unreachable_after_seconds,
        ),
    });

    let runtime = Runtime {
        health_threshold: worker_config.poll_interval * 3 + Duration::from_secs(30),
        auth_token: (!config.server.auth_token.is_empty())
            .then(|| config.server.auth_token.clone()),
        config_summary: Arc::from(config.redacted_summary()),
        metrics_enabled: config.metrics.enabled,
        chrome,
        deps,
        config: Arc::new(config.clone()),
    };

    Ok((runtime, worker_config))
}

/// Shared by every HTTP-backed adapter so trackers are identifiable in access
/// logs and nobody pays for a fresh connection pool per adapter.
fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("seedmedic/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building shared HTTP client")
}

fn build_trackers(
    configured: &[TrackerConfig],
) -> Result<HashMap<TrackerId, Arc<dyn TrackerClient>>> {
    let mut trackers: HashMap<TrackerId, Arc<dyn TrackerClient>> = HashMap::new();
    let http = build_http_client()?;

    for tracker in configured {
        let id = TrackerId::new(&tracker.id);
        let adapter: Arc<dyn TrackerClient> = match tracker.kind {
            TrackerKind::Unit3d => Arc::new(Unit3dTracker::new(
                id.clone(),
                tracker.base_url.clone(),
                tracker.api_key.clone(),
                tracker.token_placement,
                http.clone(),
            )),
            #[cfg(feature = "fakes")]
            TrackerKind::Fake => Arc::new(crate::tracker::adapters::fake::FakeTracker::new(
                id.clone(),
                demo_torrents(&id),
            )),
            #[cfg(not(feature = "fakes"))]
            TrackerKind::Fake => unreachable!(
                "config.validate() rejects a `fake` tracker in a build without the `fakes` feature"
            ),
        };
        trackers.insert(id, adapter);
    }

    Ok(trackers)
}

/// The fake tracker serves JSON rather than bencode, so it needs the matching
/// inspector. Mixing a fake tracker with a real one is not supported; the real
/// decoder wins, and the fake tracker's torrents will fail to parse.
fn build_inspector(trackers: &[TrackerConfig]) -> Arc<dyn TorrentInspector> {
    #[cfg(feature = "fakes")]
    if !trackers.is_empty() && trackers.iter().all(|t| t.kind == TrackerKind::Fake) {
        return Arc::new(crate::torrent::adapters::fake::FakeInspector);
    }
    let _ = trackers;
    Arc::new(BencodeInspector)
}

fn build_client(config: &Config) -> Result<Arc<dyn TorrentClient>> {
    let Some(download_client) = &config.download_client else {
        return Ok(Arc::new(
            crate::seeding::adapters::unconfigured::UnconfiguredClient,
        ));
    };

    Ok(match download_client.kind {
        DownloadClientKind::QBittorrent => Arc::new(QBittorrentClient::new(
            download_client.base_url.clone(),
            download_client.username.clone(),
            download_client.password.clone(),
            build_http_client()?,
        )),
        #[cfg(feature = "fakes")]
        DownloadClientKind::Fake => {
            Arc::new(crate::seeding::adapters::fake::FakeTorrentClient::new())
        }
        #[cfg(not(feature = "fakes"))]
        DownloadClientKind::Fake => unreachable!(
            "config.validate() rejects download_client = \"fake\" in a build without the \
             `fakes` feature"
        ),
    })
}

fn build_candidate_sources(config: &Config) -> Result<Vec<Arc<dyn CandidateSource>>> {
    let mut sources: Vec<Arc<dyn CandidateSource>> = Vec::new();

    if !config.arr.is_empty() {
        let http = build_http_client()?;
        for arr in &config.arr {
            let kind = match arr.kind {
                ArrKind::Sonarr => AdapterArrKind::Sonarr,
                ArrKind::Radarr => AdapterArrKind::Radarr,
            };
            let path_mappings = arr
                .path_mappings
                .iter()
                .map(|mapping| PathMapping {
                    from: mapping.from.clone(),
                    to: mapping.to.clone(),
                })
                .collect();
            sources.push(Arc::new(ArrCandidateSource::new(
                kind,
                &arr.name,
                arr.base_url.clone(),
                arr.api_key.clone(),
                http.clone(),
                path_mappings,
            )));
        }
    }

    for root in &config.library.roots {
        sources.push(Arc::new(FilesystemCandidateSource::new(root.clone())));
    }

    Ok(sources)
}

/// Two warnings for the fake tracker: enough to see discovery, the state
/// machine, and the review queue working. Their content is not in anybody's
/// library, so both park for review — which is the correct, visible outcome
/// rather than a pretend success.
#[cfg(feature = "fakes")]
fn demo_torrents(tracker: &TrackerId) -> Vec<crate::tracker::adapters::fake::FakeTorrent> {
    use chrono::Utc;

    use crate::{
        torrent::{
            InfoHash, SafeRelativePath, TorrentFile, TorrentMetadata, adapters::fake::FakeInspector,
        },
        tracker::{HitAndRun, TrackerTorrentId, adapters::fake::FakeTorrent},
    };

    let build = |index: u8, name: &str, files: Vec<(&str, u64)>| {
        let metadata = TorrentMetadata {
            info_hash: InfoHash::from_bytes([index; 20]),
            name: SafeRelativePath::parse(name).expect("demo torrent name is a valid component"),
            piece_length: 1 << 20,
            files: files
                .into_iter()
                .map(|(path, length)| TorrentFile {
                    path: SafeRelativePath::parse(path).expect("demo path is valid"),
                    length,
                })
                .collect(),
            pieces: Vec::new(),
        };

        FakeTorrent {
            hit_and_run: HitAndRun {
                tracker: tracker.clone(),
                torrent_id: TrackerTorrentId::new(format!("demo-{index}")),
                torrent_name: name.to_owned(),
                info_hash: Some(metadata.info_hash),
                size_bytes: metadata.total_length(),
                deadline: None,
                observed_at: Utc::now(),
            },
            bytes: FakeInspector::encode(&metadata),
        }
    };

    // Sizes are small and round so the demo can be completed for real — see
    // the recipe in config.example.toml.
    vec![
        build(
            1,
            "Demo.Movie.2024.1080p",
            vec![("Demo.Movie.2024.1080p/movie.mkv", 1 << 20)],
        ),
        build(
            2,
            "Demo.Show.S01.1080p",
            vec![
                ("Demo.Show.S01.1080p/S01E01.mkv", 2 << 20),
                ("Demo.Show.S01.1080p/S01E02.mkv", 3 << 20),
            ],
        ),
    ]
}
