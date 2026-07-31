//! The page somebody links in a bug report: what is SeedMedic doing, and can
//! it reach everything? Read-only, no actions — see
//! `docs/todos/0012-observability.md`.

use axum::response::{IntoResponse, Response};
use maud::{Markup, html};

use crate::{
    bootstrap::Runtime,
    repair::{JobCounts, JobId, RepairJob, RepairState, ReviewReason},
};

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
    let runtime = state.runtime.current();

    // Four bounded queries. This page used to read every column of every row
    // via `jobs(i64::MAX)`, then issue one `history()` query per unfinished job
    // and one filesystem walk per job with a staging directory — O(n) queries
    // plus O(n) walks, on a page an operator refreshes while wondering why
    // nothing is happening.
    let counts = runtime.deps.store.counts().await?;
    let unfinished = runtime.deps.store.unfinished().await?;
    let staged_bytes = runtime.deps.store.staged_bytes_declared().await?;

    let mut tracker_ids: Vec<_> = runtime.deps.trackers.keys().cloned().collect();
    tracker_ids.sort();

    let client_summary = runtime.deps.client.summary().await;
    let free_bytes = runtime.deps.staging.free_bytes().await;
    let stuck = stuck_jobs(&runtime, &unfinished).await?;

    let body = html! {
        @if !stuck.is_empty() {
            (stuck_notice(&stuck))
        }

        h2 { "Repairs" }
        (state_counts_table(&counts))

        h2 { "Trackers" }
        (tracker_table(&runtime, &tracker_ids))

        h2 { "Download client" }
        dl {
            dt { "Adapter" } dd { @if runtime.deps.client_is_stub { "fake (stub)" } @else { "qbittorrent" } }
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
                @if runtime.deps.staging.root_path().as_os_str().is_empty() {
                    "not configured"
                } @else {
                    (runtime.deps.staging.root_path().display().to_string())
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
        pre { (runtime.config_summary.as_ref()) }
    };

    Ok(layout::page(&runtime.chrome, "Status", body).into_response())
}

/// Jobs the worker still owns that look wedged: parked or completed jobs are
/// exempt, since a human or the tracker already has the last word on those.
///
/// `unfinished` is already exactly the actionable jobs, and the rewind counts
/// arrive as one grouped query rather than one `history()` call per job.
async fn stuck_jobs(
    runtime: &Runtime,
    unfinished: &[RepairJob],
) -> Result<Vec<(JobId, String, StuckReason)>, WebError> {
    let now = runtime.deps.clock.now();

    // `> STUCK_REWIND_THRESHOLD` in the old per-job filter, so `>= threshold + 1`
    // here. Off by one in either direction changes which jobs are reported.
    let oscillating: Vec<(JobId, i64)> = runtime
        .deps
        .store
        .rewind_counts(STUCK_REWIND_THRESHOLD as i64 + 1)
        .await?;

    let mut stuck = Vec::new();
    for job in unfinished {
        if now - job.updated_at > STUCK_TIME_THRESHOLD {
            stuck.push((job.id, job.torrent_name.clone(), StuckReason::TimeInState));
            continue;
        }
        if let Some((_, rewinds)) = oscillating.iter().find(|(id, _)| *id == job.id) {
            stuck.push((
                job.id,
                job.torrent_name.clone(),
                StuckReason::Oscillating {
                    rewinds: usize::try_from(*rewinds).unwrap_or(usize::MAX),
                },
            ));
        }
    }

    Ok(stuck)
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

/// `counts` arrives ordered by the store: states by name, review reasons
/// biggest-group-first — the same ordering the review queue uses, so a dozen
/// jobs blocked on one cause is impossible to miss here either. Presenting
/// states in lifecycle order rather than alphabetically is this page's own
/// concern, so it happens here.
fn state_counts_table(counts: &JobCounts) -> Markup {
    let ordered: Vec<(RepairState, i64)> = RepairState::PROGRESSION
        .into_iter()
        .chain([RepairState::AwaitingReview, RepairState::Failed])
        .filter_map(|state| {
            counts
                .by_state
                .iter()
                .find(|(counted, _)| *counted == state)
                .map(|(_, count)| (state, *count))
        })
        .collect();

    html! {
        @if counts.total == 0 {
            p.empty { "No hit-and-runs discovered yet." }
        } @else {
            dl {
                @for (state, count) in &ordered {
                    dt { (layout::state_chip(*state)) } dd { (count) }
                }
            }
            @if !counts.by_review_reason.is_empty() {
                h3 { "Awaiting review, by reason" }
                dl {
                    @for (reason, count) in &counts.by_review_reason {
                        dt { (reason.map_or("No reason recorded", ReviewReason::description)) }
                        dd { (count) }
                    }
                }
            }
        }
    }
}

fn tracker_table(runtime: &Runtime, tracker_ids: &[crate::tracker::TrackerId]) -> Markup {
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
                        @let health = runtime.deps.diagnostics.tracker_health(id);
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
    use crate::repair::ReviewReason;

    use super::*;

    /// Every state the state machine can produce, including two review reasons
    /// and "no reason recorded", renders without panicking and shows a count for
    /// each.
    ///
    /// Takes a `JobCounts` directly now that the counting is SQL's job — so this
    /// tests the rendering, and
    /// `repair::adapters::sqlite::tests::counts_agree_with_folding_over_every_job`
    /// tests the counting. Neither one tests both, which is the point.
    #[test]
    fn renders_a_count_for_every_state() {
        let counts = JobCounts {
            by_state: RepairState::PROGRESSION
                .into_iter()
                .chain([RepairState::Failed, RepairState::AwaitingReview])
                .map(|state| (state, 1))
                .collect(),
            by_review_reason: vec![
                (Some(ReviewReason::NoCandidates), 1),
                (Some(ReviewReason::AmbiguousMatch), 1),
                (None, 1),
            ],
            total: 11,
        };

        let markup = state_counts_table(&counts).into_string();

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

    /// A state with no jobs is absent from `by_state` rather than present with a
    /// zero, and must not render a row — the page is a summary, not a schema
    /// dump.
    #[test]
    fn a_state_with_no_jobs_gets_no_row() {
        let counts = JobCounts {
            by_state: vec![(RepairState::Seeding, 2)],
            by_review_reason: Vec::new(),
            total: 2,
        };

        let markup = state_counts_table(&counts).into_string();

        assert!(markup.contains(RepairState::Seeding.as_str()));
        assert!(!markup.contains(RepairState::Failed.as_str()));
        assert!(!markup.contains("Awaiting review, by reason"));
    }

    #[test]
    fn no_jobs_at_all_says_so() {
        let markup = state_counts_table(&JobCounts::default()).into_string();
        assert!(markup.contains("No hit-and-runs discovered yet."));
    }
}
