use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};

use crate::repair::{JobId, PlannedFile, RepairJob, RepairState, TransitionRecord};

use super::{AppState, error::WebError, layout};

/// The queue. Jobs needing a human come first, because they are the only ones
/// the operator has to do anything about.
pub async fn list(State(state): State<AppState>) -> Result<Response, WebError> {
    let mut jobs = state.deps.store.jobs(200).await?;
    jobs.sort_by_key(|job| (sort_rank(job.state), std::cmp::Reverse(job.id.0)));

    let review_count = jobs
        .iter()
        .filter(|job| job.state == RepairState::AwaitingReview)
        .count();

    let body = html! {
        @if review_count > 0 {
            div.notice {
                strong { (review_count) } " repair" @if review_count != 1 { "s" } " need a decision."
            }
        }

        @if jobs.is_empty() {
            p.empty { "No hit-and-runs discovered yet." }
        } @else {
            table {
                thead { tr {
                    th { "State" } th { "Torrent" } th { "Tracker" } th { "Why" } th { "Updated" }
                } }
                tbody {
                    @for job in &jobs {
                        tr {
                            td { (layout::state_chip(job.state)) }
                            td { a href={ "/jobs/" (job.id) } { (job.torrent_name) } }
                            td { (job.tracker) }
                            td { (explain(job)) }
                            td { (job.updated_at.format("%Y-%m-%d %H:%M")) }
                        }
                    }
                }
            }
        }
    };

    Ok(layout::page("Repairs", body).into_response())
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let id = JobId(id);
    let job = state.deps.store.job(id).await?.ok_or(WebError::NotFound)?;
    let files = state.deps.store.planned_files(id).await?;
    let history = state.deps.store.history(id).await?;

    let body = html! {
        h2 { (job.torrent_name) }
        p { (layout::state_chip(job.state)) " " (explain(&job)) }

        @if job.state == RepairState::AwaitingReview {
            (review_panel(&job))
        }

        h2 { "Job" }
        dl {
            dt { "Tracker" } dd { (job.tracker) " / " (job.torrent_id) }
            dt { "Info-hash" } dd { (job.info_hash.map(|hash| hash.to_hex()).unwrap_or_else(|| "—".into())) }
            dt { "Total size" } dd { (job.total_bytes.map(human_bytes).unwrap_or_else(|| "—".into())) }
            dt { "Staging" } dd { (job.staging_dir.as_ref().map_or("—", |dir| dir.as_str())) }
            dt { "Materialization" } dd {
                @match job.materialization {
                    Some(strategy) => {
                        (strategy.as_str())
                        @if strategy.aliases_library_file() {
                            " — shares inodes with the media library"
                        }
                    }
                    None => "—",
                }
            }
            dt { "Attempts" } dd { (job.attempts) }
            dt { "Created" } dd { (job.created_at.format("%Y-%m-%d %H:%M:%S")) }
        }

        h2 { "Files" }
        (file_table(&files))

        h2 { "History" }
        (history_table(&history))
    };

    Ok(layout::page(&job.torrent_name, body).into_response())
}

fn review_panel(job: &RepairJob) -> Markup {
    let resume_to = job
        .review_from_state
        .map_or_else(|| "the previous step".to_owned(), |state| state.to_string());

    html! {
        div.notice {
            p {
                @match job.review_reason {
                    Some(reason) => (reason.description()),
                    None => "This repair is waiting for a decision.",
                }
            }
            form.actions method="post" {
                button formaction={ "/jobs/" (job.id) "/retry" } {
                    "Retry from " (resume_to)
                }
                button formaction={ "/jobs/" (job.id) "/restart" } {
                    "Start over (discards staged files)"
                }
                button.danger formaction={ "/jobs/" (job.id) "/abandon" } {
                    "Abandon"
                }
            }
        }
    }
}

fn file_table(files: &[PlannedFile]) -> Markup {
    if files.is_empty() {
        return html! { p.empty { "No file plan yet." } };
    }

    html! {
        table {
            thead { tr {
                th { "Torrent path" } th { "Size" } th { "Matched library file" }
                th { "Confidence" } th { "Staged as" }
            } }
            tbody {
                @for file in files {
                    tr {
                        td.wrap { (file.torrent_path.as_str()) }
                        td { (human_bytes(file.length)) }
                        td.wrap {
                            @match &file.source {
                                Some(path) => (path.display().to_string()),
                                None => "—",
                            }
                        }
                        td {
                            @match file.confidence {
                                Some(confidence) => (format!("{confidence:?}").to_lowercase()),
                                None => "—",
                            }
                        }
                        td {
                            @match file.materialized_as {
                                Some(strategy) => (strategy.as_str()),
                                None => "—",
                            }
                        }
                    }
                }
            }
        }
    }
}

fn history_table(history: &[TransitionRecord]) -> Markup {
    html! {
        table {
            thead { tr { th { "When" } th { "Change" } th { "Reason" } th { "Detail" } } }
            tbody {
                @for record in history {
                    tr {
                        td { (record.occurred_at.format("%Y-%m-%d %H:%M:%S")) }
                        td {
                            @if record.from == record.to {
                                (record.to.as_str())
                            } @else {
                                (record.from.as_str()) " → " (record.to.as_str())
                            }
                        }
                        td { (record.reason) }
                        td {
                            @if let Some(detail) = &record.detail {
                                pre { (serde_json::to_string_pretty(detail).unwrap_or_default()) }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One line explaining where a job stands.
fn explain(job: &RepairJob) -> String {
    if let Some(reason) = job.review_reason
        && job.state == RepairState::AwaitingReview
    {
        return reason.description().to_owned();
    }
    if let Some(failure) = &job.failure_reason
        && job.state == RepairState::Failed
    {
        return failure.clone();
    }
    match job.state {
        RepairState::Completed => "The tracker cleared the hit-and-run.".to_owned(),
        RepairState::Seeding => "Seeding; waiting for the tracker to clear the warning.".to_owned(),
        other => format!("In progress ({other})."),
    }
}

/// Review first, then anything still running, then finished work.
fn sort_rank(state: RepairState) -> u8 {
    match state {
        RepairState::AwaitingReview => 0,
        RepairState::Failed => 1,
        RepairState::Completed => 3,
        _ => 2,
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
