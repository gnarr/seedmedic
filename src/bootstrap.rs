//! Wiring. The only place that knows which adapter implements which port.
//!
//! Everything else in SeedMedic depends on ports; this module reads the config
//! and picks the implementations once, at startup.

use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use anyhow::{Context, Result};

use crate::{
    clock::{Clock, SystemClock},
    config::{ArrKind, Config, DownloadClientKind, TrackerConfig, TrackerKind},
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
        RepairDeps, WorkerConfig, WorkerHealth, adapters::sqlite::SqliteRepairStore,
        worker::RepairWorker,
    },
    seeding::{TorrentClient, adapters::qbittorrent::QBittorrentClient},
    staging::{
        StagingFilesystem, StagingRoot,
        adapters::{local::LocalStaging, unconfigured::UnconfiguredStaging},
    },
    torrent::{TorrentInspector, adapters::bencode::BencodeInspector},
    tracker::{TrackerClient, TrackerId, adapters::unit3d::Unit3dTracker},
};

/// A fully wired SeedMedic, ready to serve and to work.
pub struct App {
    pub deps: Arc<RepairDeps>,
    pub worker_config: WorkerConfig,
    pub bind_address: SocketAddr,
    pub auth_token: Option<String>,
    /// How long `/health` tolerates the worker having gone quiet before
    /// reporting unready. Derived from `worker.poll_interval` with margin
    /// rather than hard-coded, so a slower configured interval does not
    /// immediately look unhealthy.
    pub health_threshold: Duration,
    /// The effective configuration, secrets redacted, for the `/status` page.
    pub config_summary: String,
    /// Whether `/metrics` should serve anything. Harmless without the
    /// `metrics` feature — see `crate::metrics`.
    pub metrics_enabled: bool,
}

impl App {
    pub fn worker(&self) -> RepairWorker {
        RepairWorker::new(self.deps.clone(), self.worker_config.clone())
    }
}

pub async fn build(config: Config) -> Result<App> {
    config.validate()?;

    let bind_address: SocketAddr = config
        .server
        .bind_address
        .parse()
        .with_context(|| format!("invalid bind address {}", config.server.bind_address))?;

    let pool = database::connect(&config.database.path).await?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let store = Arc::new(SqliteRepairStore::new(pool, clock.clone()));

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
    let client = build_client(&config)?;
    let candidate_sources = build_candidate_sources(&config)?;
    let worker_config = config.worker.to_worker_config();

    let stub_trackers = config
        .trackers
        .iter()
        .filter(|tracker| tracker.kind == TrackerKind::Fake)
        .map(|tracker| TrackerId::new(&tracker.id));
    let client_is_stub = config
        .download_client
        .as_ref()
        .is_some_and(|download_client| download_client.kind == DownloadClientKind::Fake);
    let notifier: Arc<dyn Notifier> = match &config.notifications.webhook_url {
        Some(url) => Arc::new(WebhookNotifier::new(url.clone(), build_http_client()?)),
        None => Arc::new(NoopNotifier),
    };

    Ok(App {
        deps: Arc::new(RepairDeps {
            store,
            trackers,
            inspector,
            candidate_sources,
            staging,
            client,
            clock,
            policy: config.policy.to_policy(),
            category: config
                .download_client
                .as_ref()
                .and_then(|download_client| download_client.category.clone()),
            worker_health: Arc::new(WorkerHealth::default()),
            diagnostics: Arc::new(Diagnostics::new(stub_trackers)),
            client_is_stub,
            #[cfg(feature = "metrics")]
            metrics: Arc::new(crate::metrics::Metrics::default()),
            notifier,
            tracker_unreachable_threshold: Duration::from_secs(
                config.notifications.tracker_unreachable_after_seconds,
            ),
        }),
        health_threshold: worker_config.poll_interval * 3 + Duration::from_secs(30),
        worker_config,
        bind_address,
        auth_token: (!config.server.auth_token.is_empty())
            .then(|| config.server.auth_token.expose().to_owned()),
        config_summary: config.redacted_summary(),
        metrics_enabled: config.metrics.enabled,
    })
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
