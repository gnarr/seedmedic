//! The page somebody links in a bug report: what is SeedMedic doing, and can
//! it reach everything? Read-only, no actions — see
//! `docs/todos/0012-observability.md`.

use axum::response::{IntoResponse, Response};
use maud::{Markup, html};

use crate::repair::{JobId, RepairJob, RepairState, ReviewReason};

use super::{AppState, error::WebError, layout};

/// A job left this long in an actionable state, with the worker still
/// ticking (see `/health`), is not merely slow. Deliberately generous: a
/// legitimate recheck can run long, and a false positive here is just noise,
/// not a wrong decision — nothing acts on this besides showing it.
const STUCK_TIME_THRESHOLD: chrono::Duration = chrono::Duration::hours(1);

/// More rewinds than this within one job's history means it is oscillating
/// between rewind and advance rather than making progress — the same
/// condition `RepairWorker::drive` already warns about when it hits its own
/// iteration bound in a single tick.
const STUCK_REWIND_THRESHOLD: usize = 3;

enum StuckReason {
    TimeInState,
    Oscillating { rewinds: usize },
}

pub async fn page(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Response, WebError> {
    let jobs = state.deps.store.jobs(i64::MAX).await?;

    let mut tracker_ids: Vec<_> = state.deps.trackers.keys().cloned().collect();
    tracker_ids.sort();

    let client_summary = state.deps.client.summary().await;
    let staged_bytes = total_staged_bytes(&state, &jobs).await;
    let free_bytes = state.deps.staging.free_bytes().await;
    let stuck = stuck_jobs(&state, &jobs).await;

    let body = html! {
        @if !stuck.is_empty() {
            (stuck_notice(&stuck))
        }

        h2 { "Repairs" }
        (state_counts_table(&jobs))

        h2 { "Trackers" }
        (tracker_table(&state, &tracker_ids))

        h2 { "Download client" }
        dl {
            dt { "Adapter" } dd { @if state.deps.client_is_stub { "fake (stub)" } @else { "qbittorrent" } }
            dt { "Reachable" }
            dd {
                @match &client_summary {
                    Ok(_) => "yes",
                    Err(_) => "no",
                }
            }
            @if let Ok(summary) = &client_summary {
                dt { "Torrents held" } dd { (summary.torrent_count) }
            }
            @if let Err(error) = &client_summary {
                dt { "Last error" } dd { (error.to_string()) }
            }
        }

        h2 { "Staging" }
        dl {
            dt { "Path" }
            dd {
                @if state.deps.staging.root_path().as_os_str().is_empty() {
                    "not configured"
                } @else {
                    (state.deps.staging.root_path().display().to_string())
                }
            }
            dt { "Free space" }
            dd {
                @match free_bytes {
                    Ok(bytes) => (human_bytes(bytes)),
                    Err(_) => "unknown",
                }
            }
            dt { "Held by SeedMedic" } dd { (human_bytes(staged_bytes)) }
        }

        h2 { "Effective policy" }
        pre { (state.config_summary.as_ref()) }
    };

    Ok(layout::page(&state.chrome, "Status", body).into_response())
}

/// Jobs the worker still owns that look wedged: parked or completed jobs are
/// exempt, since a human or the tracker already has the last word on those.
async fn stuck_jobs(state: &AppState, jobs: &[RepairJob]) -> Vec<(JobId, String, StuckReason)> {
    let now = state.deps.clock.now();
    let mut stuck = Vec::new();

    for job in jobs.iter().filter(|job| job.state.is_actionable()) {
        if now - job.updated_at > STUCK_TIME_THRESHOLD {
            stuck.push((job.id, job.torrent_name.clone(), StuckReason::TimeInState));
            continue;
        }

        let rewinds = state
            .deps
            .store
            .history(job.id)
            .await
            .unwrap_or_default()
            .iter()
            .filter(|transition| transition.reason == "reconciliation")
            .count();
        if rewinds > STUCK_REWIND_THRESHOLD {
            stuck.push((
                job.id,
                job.torrent_name.clone(),
                StuckReason::Oscillating { rewinds },
            ));
        }
    }

    stuck
}

fn stuck_notice(stuck: &[(JobId, String, StuckReason)]) -> Markup {
    html! {
        div.notice.danger {
            strong { (stuck.len()) } " repair" @if stuck.len() != 1 { "s" } " may be stuck:"
            ul {
                @for (id, name, reason) in stuck {
                    li {
                        a href={ "/jobs/" (id) } { (name) }
                        " — "
                        @match reason {
                            StuckReason::TimeInState => "no progress for over an hour",
                            StuckReason::Oscillating { rewinds } => {
                                "rewound " (rewinds) " times; may be oscillating"
                            }
                        }
                    }
                }
            }
        }
    }
}

fn state_counts_table(jobs: &[RepairJob]) -> Markup {
    let mut counts: Vec<(RepairState, usize)> = RepairState::PROGRESSION
        .into_iter()
        .chain([RepairState::AwaitingReview, RepairState::Failed])
        .map(|state| (state, jobs.iter().filter(|job| job.state == state).count()))
        .collect();
    counts.retain(|(_, count)| *count > 0);

    let review_reasons = review_reason_counts(jobs);

    html! {
        @if jobs.is_empty() {
            p.empty { "No hit-and-runs discovered yet." }
        } @else {
            dl {
                @for (state, count) in &counts {
                    dt { (layout::state_chip(*state)) } dd { (count) }
                }
            }
            @if !review_reasons.is_empty() {
                h3 { "Awaiting review, by reason" }
                dl {
                    @for (reason, count) in &review_reasons {
                        dt { (reason.map_or("No reason recorded", ReviewReason::description)) }
                        dd { (count) }
                    }
                }
            }
        }
    }
}

/// Biggest group first — the same ordering the review queue uses, so a
/// dozen jobs blocked on one cause is impossible to miss here too.
fn review_reason_counts(jobs: &[RepairJob]) -> Vec<(Option<ReviewReason>, usize)> {
    let mut counts: Vec<(Option<ReviewReason>, usize)> = Vec::new();
    for job in jobs
        .iter()
        .filter(|job| job.state == RepairState::AwaitingReview)
    {
        match counts
            .iter_mut()
            .find(|(reason, _)| *reason == job.review_reason)
        {
            Some((_, count)) => *count += 1,
            None => counts.push((job.review_reason, 1)),
        }
    }
    counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    counts
}

fn tracker_table(state: &AppState, tracker_ids: &[crate::tracker::TrackerId]) -> Markup {
    html! {
        @if tracker_ids.is_empty() {
            p.empty { "No trackers configured." }
        } @else {
            table {
                thead { tr {
                    th { "Tracker" } th { "Adapter" } th { "Last successful poll" } th { "Last error" }
                } }
                tbody {
                    @for id in tracker_ids {
                        @let health = state.deps.diagnostics.tracker_health(id);
                        tr {
                            td { (id) }
                            td { @if health.stub { "fake (stub)" } @else { "unit3d" } }
                            td {
                                @match health.last_success {
                                    Some(at) => (at.format("%Y-%m-%d %H:%M").to_string()),
                                    None => "never",
                                }
                            }
                            td {
                                @match &health.last_error {
                                    Some((at, message)) => (format!("{} — {message}", at.format("%Y-%m-%d %H:%M"))),
                                    None => "none",
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn total_staged_bytes(state: &AppState, jobs: &[RepairJob]) -> u64 {
    let mut total = 0u64;
    for job in jobs {
        if let Some(staging_dir) = &job.staging_dir {
            total += state
                .deps
                .staging
                .usage(staging_dir)
                .await
                .unwrap_or_default();
        }
    }
    total
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::{
        repair::{JobId, ReviewReason},
        tracker::{TrackerId, TrackerTorrentId},
    };

    use super::*;

    fn job(state: RepairState, review_reason: Option<ReviewReason>) -> RepairJob {
        RepairJob {
            id: JobId(1),
            tracker: TrackerId::new("example"),
            torrent_id: TrackerTorrentId::new("t-1"),
            torrent_name: "Demo".to_owned(),
            state,
            review_from_state: (state == RepairState::AwaitingReview)
                .then_some(RepairState::Discovered),
            review_reason,
            failure_reason: None,
            info_hash: None,
            total_bytes: None,
            staging_dir: None,
            materialization: None,
            deadline: None,
            uploaded_bytes: None,
            seeding_seconds: None,
            rechecking_started_at: None,
            consecutive_unknown_tracker_status: 0,
            resume_approved: false,
            attempts: 0,
            next_attempt_at: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    /// Every state the state machine can produce, including two review
    /// reasons and "no reason recorded", renders without panicking and shows
    /// a count for each.
    #[test]
    fn renders_a_count_for_every_state() {
        let jobs: Vec<RepairJob> = RepairState::PROGRESSION
            .into_iter()
            .chain([RepairState::Failed])
            .map(|state| job(state, None))
            .chain([
                job(
                    RepairState::AwaitingReview,
                    Some(ReviewReason::NoCandidates),
                ),
                job(
                    RepairState::AwaitingReview,
                    Some(ReviewReason::AmbiguousMatch),
                ),
                job(RepairState::AwaitingReview, None),
            ])
            .collect();

        let markup = state_counts_table(&jobs).into_string();

        for state in RepairState::PROGRESSION
            .into_iter()
            .chain([RepairState::AwaitingReview, RepairState::Failed])
        {
            assert!(markup.contains(state.as_str()), "missing a row for {state}");
        }
        assert!(markup.contains(ReviewReason::NoCandidates.description()));
        assert!(markup.contains(ReviewReason::AmbiguousMatch.description()));
        assert!(markup.contains("No reason recorded"));
    }
}
