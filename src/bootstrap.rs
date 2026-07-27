//! Wiring. The only place that knows which adapter implements which port.
//!
//! Everything else in SeedMedic depends on ports; this module reads the config
//! and picks the implementations once, at startup.

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use anyhow::{Context, Result};

use crate::{
    clock::{Clock, SystemClock},
    config::{ArrKind, Config, DownloadClientKind, TrackerConfig, TrackerKind},
    database,
    library::{
        CandidateSource,
        adapters::{
            arr::{ArrCandidateSource, ArrKind as AdapterArrKind, PathMapping},
            filesystem::FilesystemCandidateSource,
        },
    },
    repair::{RepairDeps, WorkerConfig, adapters::sqlite::SqliteRepairStore, worker::RepairWorker},
    seeding::{TorrentClient, adapters::qbittorrent::QBittorrentClient},
    staging::{StagingRoot, adapters::local::LocalStaging},
    torrent::{TorrentInspector, adapters::bencode::BencodeInspector},
    tracker::{TrackerClient, TrackerId, adapters::unit3d::Unit3dTracker},
};

/// A fully wired SeedMedic, ready to serve and to work.
pub struct App {
    pub deps: Arc<RepairDeps>,
    pub worker_config: WorkerConfig,
    pub bind_address: SocketAddr,
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
    // repair can ever write inside the media library.
    let staging_root = StagingRoot::new(config.staging.root.clone(), &config.library.roots)
        .context("staging root is not usable")?;
    let staging = Arc::new(LocalStaging::new(staging_root));

    let trackers = build_trackers(&config.trackers)?;
    let inspector = build_inspector(&config.trackers);
    let client = build_client(&config)?;
    let candidate_sources = build_candidate_sources(&config)?;

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
            category: config.download_client.category.clone(),
        }),
        worker_config: config.worker.to_worker_config(),
        bind_address,
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
            TrackerKind::Fake => anyhow::bail!(
                "tracker `{}` is configured as `fake`, but this build has the `fakes` feature disabled",
                tracker.id
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
    Ok(match config.download_client.kind {
        DownloadClientKind::QBittorrent => Arc::new(QBittorrentClient::new(
            config.download_client.base_url.clone(),
            config.download_client.category.clone(),
        )),
        #[cfg(feature = "fakes")]
        DownloadClientKind::Fake => {
            Arc::new(crate::seeding::adapters::fake::FakeTorrentClient::new())
        }
        #[cfg(not(feature = "fakes"))]
        DownloadClientKind::Fake => anyhow::bail!(
            "download_client is configured as `fake`, but this build has the `fakes` feature disabled"
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
