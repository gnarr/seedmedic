//! `GET /api/v1/dashboard` and `GET /api/v1/diagnostics`.
//!
//! Split because they cost different amounts. The dashboard is bounded queries
//! only, so it is safe to refetch on every event; diagnostics probes the download
//! client and walks the staging filesystem, so it is a page you open.

use axum::{Json, extract::State};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::{
    events::Activity,
    repair::{JobId, RepairState, ReviewReason},
    web::AppState,
};

use super::error::ApiError;

/// A job left this long in an actionable state, with the worker still ticking,
/// is not merely slow. Deliberately generous: a legitimate recheck can run long,
/// and a false positive here is noise, not a wrong decision.
const STUCK_TIME_THRESHOLD: chrono::Duration = chrono::Duration::hours(1);

/// More rewinds than this means a job is oscillating rather than progressing.
const STUCK_REWIND_THRESHOLD: i64 = 3;

#[derive(Serialize)]
pub struct Dashboard {
    generated_at: DateTime<Utc>,
    counts: Counts,
    attention: Attention,
    worker: Worker,
    trackers: Vec<Tracker>,
    setup: Setup,
}

#[derive(Serialize)]
struct Counts {
    total: i64,
    by_state: Vec<StateCount>,
    by_review_reason: Vec<ReasonCount>,
}

#[derive(Serialize)]
struct StateCount {
    state: RepairState,
    count: i64,
}

#[derive(Serialize)]
struct ReasonCount {
    /// `null` for a job parked before a reason was recorded.
    reason: Option<ReviewReason>,
    /// The operator prose for it, from `ReviewReason::description` — so the
    /// client never holds a copy that can go stale.
    description: Option<&'static str>,
    count: i64,
}

#[derive(Serialize)]
struct Attention {
    review: i64,
    failed: i64,
    stuck: Vec<Stuck>,
}

#[derive(Serialize)]
struct Stuck {
    job: JobId,
    torrent_name: String,
    /// `"time_in_state"` or `"oscillating"`.
    reason: &'static str,
    detail: String,
}

#[derive(Serialize)]
struct Worker {
    last_tick: Option<DateTime<Utc>>,
    /// Whether the worker has gone quiet for longer than `/health` tolerates.
    /// A *separate* fact from whether the client is connected, and the UI must
    /// never conflate the two.
    stale: bool,
    threshold_seconds: u64,
    last_tick_summary: Option<ActivitySummary>,
    last_discovery: Option<ActivitySummary>,
    last_reconcile: Option<ActivitySummary>,
}

/// One of the three summaries the worker used to return, log, and throw away.
#[derive(Serialize)]
struct ActivitySummary {
    at: Option<DateTime<Utc>>,
    claimed: usize,
    advanced: usize,
    parked: usize,
    retrying: usize,
    rewound: usize,
    jobs_created: usize,
    trackers_failed: usize,
}

impl From<Activity> for ActivitySummary {
    fn from(activity: Activity) -> Self {
        Self {
            at: activity.at,
            claimed: activity.claimed,
            advanced: activity.advanced,
            parked: activity.parked,
            retrying: activity.retrying,
            rewound: activity.rewound,
            jobs_created: activity.jobs_created,
            trackers_failed: activity.trackers_failed,
        }
    }
}

#[derive(Serialize)]
struct Tracker {
    id: String,
    /// `"fake"` or `"unit3d"`.
    adapter: &'static str,
    stub: bool,
    last_success: Option<DateTime<Utc>>,
    last_error: Option<TrackerError>,
    unfinished_jobs: i64,
}

#[derive(Serialize)]
struct TrackerError {
    at: DateTime<Utc>,
    message: String,
}

/// What the maud UI's `Chrome` carried, as data.
#[derive(Serialize)]
struct Setup {
    config_path: String,
    /// Every unmet-setting warning `Config::problems()` found, verbatim. Empty
    /// once a deployment has nothing left to configure.
    warnings: Vec<String>,
    /// Three states, not a boolean: `"set"` shows a sign-out control, `"unset"`
    /// shows the no-token warning, `"unknown"` shows neither. Collapsing the
    /// last two into `false` would make a page that cannot know claim the port
    /// is unauthenticated.
    auth: &'static str,
}

pub async fn dashboard(State(state): State<AppState>) -> Result<Json<Dashboard>, ApiError> {
    let runtime = state.runtime.current();
    let now = runtime.deps.clock.now();

    // Four bounded queries, plus one grouped query for the stuck check. No
    // per-job history read, no filesystem walk.
    let counts = runtime.deps.store.counts().await?;
    let unfinished = runtime.deps.store.unfinished().await?;
    let by_tracker = runtime.deps.store.unfinished_by_tracker().await?;
    let oscillating = runtime
        .deps
        .store
        .rewind_counts(STUCK_REWIND_THRESHOLD + 1)
        .await?;

    let count_of = |wanted: RepairState| {
        counts
            .by_state
            .iter()
            .find(|(state, _)| *state == wanted)
            .map_or(0, |(_, count)| *count)
    };

    let mut stuck = Vec::new();
    for job in &unfinished {
        if now - job.updated_at > STUCK_TIME_THRESHOLD {
            stuck.push(Stuck {
                job: job.id,
                torrent_name: job.torrent_name.clone(),
                reason: "time_in_state",
                detail: "No progress for over an hour.".to_owned(),
            });
        } else if let Some((_, rewinds)) = oscillating.iter().find(|(id, _)| *id == job.id) {
            stuck.push(Stuck {
                job: job.id,
                torrent_name: job.torrent_name.clone(),
                reason: "oscillating",
                detail: format!("Rewound {rewinds} times; may be oscillating."),
            });
        }
    }

    let activity = runtime.deps.events.latest_activity();
    let last_tick = activity.tick.and_then(|tick| tick.at);

    let trackers = runtime
        .deps
        .trackers
        .keys()
        .map(|id| {
            let health = runtime.deps.diagnostics.tracker_health(id);
            Tracker {
                id: id.as_str().to_owned(),
                adapter: if health.stub { "fake" } else { "unit3d" },
                stub: health.stub,
                last_success: health.last_success,
                last_error: health
                    .last_error
                    .map(|(at, message)| TrackerError { at, message }),
                unfinished_jobs: by_tracker
                    .iter()
                    .find(|(tracker, _)| tracker == id)
                    .map_or(0, |(_, count)| *count),
            }
        })
        .collect::<Vec<_>>();
    let mut trackers = trackers;
    trackers.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(Json(Dashboard {
        generated_at: now,
        counts: Counts {
            total: counts.total,
            by_state: ordered_states(&counts.by_state),
            by_review_reason: counts
                .by_review_reason
                .iter()
                .map(|(reason, count)| ReasonCount {
                    reason: *reason,
                    description: reason.map(ReviewReason::description),
                    count: *count,
                })
                .collect(),
        },
        attention: Attention {
            review: count_of(RepairState::AwaitingReview),
            failed: count_of(RepairState::Failed),
            stuck,
        },
        worker: Worker {
            last_tick: last_tick.or_else(|| runtime.deps.worker_health.last_tick()),
            stale: match last_tick.or_else(|| runtime.deps.worker_health.last_tick()) {
                Some(at) => {
                    now - at
                        > chrono::Duration::from_std(runtime.health_threshold)
                            .unwrap_or_else(|_| chrono::Duration::seconds(90))
                }
                // Never ticked yet is not stale — a process that just started
                // has not had the chance, and calling that unhealthy would make
                // every cold start look broken.
                None => false,
            },
            threshold_seconds: runtime.health_threshold.as_secs(),
            last_tick_summary: activity.tick.map(ActivitySummary::from),
            last_discovery: activity.discovery.map(ActivitySummary::from),
            last_reconcile: activity.reconcile.map(ActivitySummary::from),
        },
        trackers,
        setup: Setup {
            config_path: state.runtime.config_path().display().to_string(),
            warnings: runtime.chrome.warnings().to_vec(),
            auth: runtime.chrome.auth(),
        },
    }))
}

/// Lifecycle order rather than the store's alphabetical order — a summary an
/// operator reads top to bottom should follow the pipeline.
fn ordered_states(counts: &[(RepairState, i64)]) -> Vec<StateCount> {
    RepairState::PROGRESSION
        .into_iter()
        .chain([RepairState::AwaitingReview, RepairState::Failed])
        .filter_map(|state| {
            counts
                .iter()
                .find(|(counted, _)| *counted == state)
                .map(|(_, count)| StateCount {
                    state,
                    count: *count,
                })
        })
        .collect()
}

#[derive(Serialize)]
pub struct Diagnostics {
    generated_at: DateTime<Utc>,
    download_client: DownloadClient,
    staging: Staging,
    /// The effective configuration with every secret reduced to `set`/`unset` —
    /// `Config::redacted_summary`, which is already the only thing the status
    /// page was ever allowed to print.
    policy_summary: String,
    ready: bool,
}

#[derive(Serialize)]
struct DownloadClient {
    adapter: &'static str,
    stub: bool,
    reachable: bool,
    torrent_count: Option<usize>,
    error: Option<String>,
}

#[derive(Serialize)]
struct Staging {
    configured: bool,
    root: Option<String>,
    free_bytes: Option<u64>,
    /// Measured — the real filesystem walk, once per job with a staging
    /// directory. This is why diagnostics is its own endpoint rather than part of
    /// the dashboard.
    held_bytes: u64,
    /// What the store believes is staged, from `total_bytes`. Shown alongside
    /// the measurement because a disagreement between them is itself a signal.
    declared_bytes: u64,
}

pub async fn diagnostics(State(state): State<AppState>) -> Result<Json<Diagnostics>, ApiError> {
    let runtime = state.runtime.current();

    let client_summary = runtime.deps.client.summary().await;
    let root = runtime.deps.staging.root_path().to_path_buf();
    let configured = !root.as_os_str().is_empty();

    let mut held_bytes = 0_u64;
    for job in runtime.deps.store.jobs(i64::MAX).await? {
        if let Some(dir) = &job.staging_dir {
            held_bytes += runtime.deps.staging.usage(dir).await.unwrap_or_default();
        }
    }

    Ok(Json(Diagnostics {
        generated_at: runtime.deps.clock.now(),
        download_client: DownloadClient {
            adapter: if runtime.deps.client_is_stub {
                "fake"
            } else {
                "qbittorrent"
            },
            stub: runtime.deps.client_is_stub,
            reachable: client_summary.is_ok(),
            torrent_count: client_summary
                .as_ref()
                .ok()
                .map(|summary| summary.torrent_count),
            error: client_summary.as_ref().err().map(ToString::to_string),
        },
        staging: Staging {
            configured,
            root: configured.then(|| super::view::path_text(&root)),
            free_bytes: runtime.deps.staging.free_bytes().await.ok(),
            held_bytes,
            declared_bytes: runtime.deps.store.staged_bytes_declared().await?,
        },
        policy_summary: runtime.config_summary.to_string(),
        ready: runtime.deps.store.ping().await.is_ok(),
    }))
}
