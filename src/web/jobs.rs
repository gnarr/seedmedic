use axum::{
    extract::{Path, State},
    response::{IntoResponse, Response},
};
use maud::{Markup, html};
use serde_json::Value;

use crate::{
    library::{CandidateOrigin, CandidateSummary, UnmatchedReason},
    repair::{JobId, PlannedFile, RepairJob, RepairState, ReviewReason, TransitionRecord},
};

use super::{AppState, error::WebError, layout};

/// The queue. Jobs needing a human come first, grouped by why they are
/// parked — twenty jobs blocked on the same missing adapter should read as
/// one problem, not twenty identical rows to scan past.
pub async fn list(State(state): State<AppState>) -> Result<Response, WebError> {
    let runtime = state.runtime.current();
    let mut jobs = runtime.deps.store.jobs(200).await?;
    jobs.sort_by_key(|job| (sort_rank(job.state), std::cmp::Reverse(job.id.0)));

    let (awaiting, others): (Vec<&RepairJob>, Vec<&RepairJob>) = jobs
        .iter()
        .partition(|job| job.state == RepairState::AwaitingReview);
    let groups = group_by_review_reason(&awaiting);
    let review_count = awaiting.len();

    let body = html! {
        @if review_count > 0 {
            div.notice {
                strong { (review_count) } " repair" @if review_count != 1 { "s" } " need a decision."
            }
        }

        @if jobs.is_empty() {
            p.empty { "No hit-and-runs discovered yet." }
        } @else {
            @if !groups.is_empty() {
                form method="post" {
                    @for (reason, group) in &groups {
                        h3 {
                            (reason.map_or("No reason recorded", ReviewReason::description))
                            " (" (group.len()) ")"
                        }
                        (job_rows_with_checkboxes(group))
                    }
                    div.actions {
                        button formaction="/jobs/bulk/retry" { "Retry selected" }
                        button.danger formaction="/jobs/bulk/abandon" { "Abandon selected" }
                    }
                }
            }
            @if !others.is_empty() {
                @if !groups.is_empty() {
                    h3 { "Everything else" }
                }
                (job_rows(&others))
            }
        }
    };

    Ok(layout::page(&runtime.chrome, "Repairs", body).into_response())
}

/// Group parked jobs by why they are parked, biggest problem first — the
/// order that makes twenty jobs on one missing adapter impossible to miss.
/// `None` (no reason recorded) is its own group rather than being dropped.
fn group_by_review_reason<'a>(
    jobs: &[&'a RepairJob],
) -> Vec<(Option<ReviewReason>, Vec<&'a RepairJob>)> {
    let mut groups: Vec<(Option<ReviewReason>, Vec<&RepairJob>)> = Vec::new();
    for &job in jobs {
        match groups
            .iter_mut()
            .find(|(reason, _)| *reason == job.review_reason)
        {
            Some((_, group)) => group.push(job),
            None => groups.push((job.review_reason, vec![job])),
        }
    }
    groups.sort_by_key(|(_, group)| std::cmp::Reverse(group.len()));
    groups
}

fn job_rows(jobs: &[&RepairJob]) -> Markup {
    html! {
        table {
            thead { tr {
                th { "State" } th { "Torrent" } th { "Tracker" } th { "Why" } th { "Updated" }
            } }
            tbody {
                @for job in jobs {
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
}

/// Like [`job_rows`], but with a bulk-select checkbox per row and no "Why"
/// column — the group heading above already says why.
fn job_rows_with_checkboxes(jobs: &[&RepairJob]) -> Markup {
    html! {
        table {
            thead { tr {
                th { "" } th { "State" } th { "Torrent" } th { "Tracker" } th { "Updated" }
            } }
            tbody {
                @for job in jobs {
                    tr {
                        td { input type="checkbox" name="id" value=(job.id.0); }
                        td { (layout::state_chip(job.state)) }
                        td { a href={ "/jobs/" (job.id) } { (job.torrent_name) } }
                        td { (job.tracker) }
                        td { (job.updated_at.format("%Y-%m-%d %H:%M")) }
                    }
                }
            }
        }
    }
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let runtime = state.runtime.current();
    let id = JobId(id);
    let job = runtime
        .deps
        .store
        .job(id)
        .await?
        .ok_or(WebError::NotFound)?;
    let files = runtime.deps.store.planned_files(id).await?;
    let history = runtime.deps.store.history(id).await?;

    let staged_bytes = if job.state == RepairState::Completed {
        match &job.staging_dir {
            Some(staging_dir) => runtime.deps.staging.usage(staging_dir).await.ok(),
            None => None,
        }
    } else {
        None
    };

    let body = html! {
        h2 { (job.torrent_name) }
        p { (layout::state_chip(job.state)) " " (explain(&job)) }

        @if job.state == RepairState::AwaitingReview {
            (review_panel(&job, &files, &history))
        }
        @if job.state == RepairState::Rechecking {
            (rechecking_notice(&job, &runtime))
        }
        @if job.state == RepairState::Seeding {
            (seeding_progress_notice(&job))
        }
        @if job.state == RepairState::Completed {
            (completed_notice(&job, staged_bytes))
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

    Ok(layout::page(&runtime.chrome, &job.torrent_name, body).into_response())
}

fn review_panel(job: &RepairJob, files: &[PlannedFile], history: &[TransitionRecord]) -> Markup {
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
                @if job.review_reason == Some(ReviewReason::AutoResumeDisabled) {
                    button formaction={ "/jobs/" (job.id) "/approve-resume" } {
                        "Approve resume"
                    }
                }
                button formaction={ "/jobs/" (job.id) "/retry" } {
                    "Retry from " (resume_to)
                }
                button formaction={ "/jobs/" (job.id) "/restart" } {
                    "Start over (discards staged files)"
                }
                button.danger formaction={ "/jobs/" (job.id) "/abandon" } {
                    "Abandon"
                }
                @if job.staging_dir.is_some() {
                    button.danger formaction={ "/jobs/" (job.id) "/abandon-and-discard" } {
                        "Abandon and discard staged files"
                    }
                }
            }
        }
        (candidate_pickers(job, files, history))
    }
}

/// One picker per unmatched file that has candidates on record — from the
/// transition that parked the job, the only place they are kept, since a
/// park never touches `repair_job_files`. Empty for any other review reason.
fn candidate_pickers(
    job: &RepairJob,
    files: &[PlannedFile],
    history: &[TransitionRecord],
) -> Markup {
    let pickers: Vec<_> = files
        .iter()
        .filter(|file| file.source.is_none())
        .map(|file| {
            (
                file,
                ambiguous_candidates(history, file.torrent_path.as_str()),
            )
        })
        .filter(|(_, candidates)| !candidates.is_empty())
        .collect();

    if pickers.is_empty() {
        return html! {};
    }

    html! {
        div.notice {
            p { "Choose a library file for each of the following, from what was considered:" }
            @for (file, candidates) in &pickers {
                form.actions method="post" action={ "/jobs/" (job.id) "/choose-candidate" } {
                    input type="hidden" name="torrent_path" value=(file.torrent_path.as_str());
                    label {
                        (file.torrent_path.as_str()) ": "
                        select name="candidate_index" {
                            @for (index, candidate) in candidates.iter().enumerate() {
                                option value=(index) {
                                    (candidate.path.display()) " (" (origin_label(&candidate.origin)) ")"
                                }
                            }
                        }
                    }
                    button { "Choose" }
                }
            }
        }
    }
}

fn origin_label(origin: &CandidateOrigin) -> String {
    match origin {
        CandidateOrigin::Sonarr { instance } => format!("Sonarr: {instance}"),
        CandidateOrigin::Radarr { instance } => format!("Radarr: {instance}"),
        CandidateOrigin::Filesystem { root } => format!("filesystem: {}", root.display()),
    }
}

/// The candidates considered — and rejected — for one torrent file, as
/// recorded on the transition that parked this job for review. Reused by the
/// `choose-candidate` action to resolve an operator's choice back to a real
/// path, so the server — not the request — is the source of truth for what
/// counts as a valid choice.
pub(super) fn ambiguous_candidates(
    history: &[TransitionRecord],
    torrent_path: &str,
) -> Vec<CandidateSummary> {
    let Some(detail) = history
        .iter()
        .rev()
        .find(|record| record.reason == "review")
        .and_then(|record| record.detail.as_ref())
    else {
        return Vec::new();
    };

    let Some(unmatched) = detail.get("unmatched").and_then(Value::as_array) else {
        return Vec::new();
    };

    unmatched
        .iter()
        .find(|entry| entry.get("path").and_then(Value::as_str) == Some(torrent_path))
        .and_then(|entry| entry.get("reason"))
        .and_then(|reason| serde_json::from_value::<UnmatchedReason>(reason.clone()).ok())
        .map(|reason| match reason {
            UnmatchedReason::Ambiguous { candidates } => candidates,
            UnmatchedReason::NoCandidate => Vec::new(),
        })
        .unwrap_or_default()
}

/// While a check is running there is nothing to show in the file table yet —
/// this is the "not silently pending forever" half of surfacing progress: how
/// long it has been running, and when it parks if it never finishes.
fn rechecking_notice(job: &RepairJob, runtime: &crate::bootstrap::Runtime) -> Markup {
    let Some(started_at) = job.rechecking_started_at else {
        return html! {};
    };
    let elapsed = (runtime.deps.clock.now() - started_at).num_seconds();
    let timeout = runtime.deps.policy.recheck_timeout.as_secs();

    html! {
        p.notice {
            "Recheck running for " (human_duration(elapsed.max(0)))
            " — parks for review if it exceeds " (human_duration(timeout as i64)) "."
        }
    }
}

/// How a seed is doing while it waits for the tracker to clear the warning.
/// Everything here is client-reported telemetry: only the tracker's own
/// answer ever completes a repair (`src/tracker/AGENTS.md`).
fn seeding_progress_notice(job: &RepairJob) -> Markup {
    if job.uploaded_bytes.is_none() && job.seeding_seconds.is_none() && job.deadline.is_none() {
        return html! {};
    }

    html! {
        p.notice {
            @if let Some(uploaded) = job.uploaded_bytes {
                "Uploaded " (human_bytes(uploaded)) ". "
            }
            @if let Some(seconds) = job.seeding_seconds {
                "Seeding for " (human_duration(seconds as i64)) ", by the client's own count. "
            }
            @if let Some(deadline) = job.deadline {
                "Tracker deadline: " (deadline.format("%Y-%m-%d %H:%M")) "."
            }
        }
    }
}

/// The tracker cleared the hit-and-run, but the torrent it cleared is still
/// seeding from the staging directory — nothing here may delete it out from
/// under a live torrent, because deleting it means removing the torrent from
/// the client first, and that means the hit-and-run could come back.
/// Retention policy is `docs/todos/0010-manual-review.md`; today the
/// directory simply stays, and this only reports what it costs.
fn completed_notice(job: &RepairJob, staged_bytes: Option<u64>) -> Markup {
    let Some(staging_dir) = &job.staging_dir else {
        return html! {};
    };

    html! {
        p.notice {
            "The staged data at " code { (staging_dir.as_str()) }
            @if let Some(bytes) = staged_bytes {
                " (" (human_bytes(bytes)) ")"
            }
            " is still seeding and is kept — deleting it is not automatic yet."
        }
    }
}

fn human_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
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
                th { "Confidence" } th { "Staged as" } th { "Rechecked" }
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
                        td { (recheck_progress(file.recheck_progress)) }
                    }
                }
            }
        }
    }
}

/// The one place that says "S01E04 is the only mismatch" — the whole point of
/// recording per-file completeness rather than one number for the torrent.
fn recheck_progress(ratio: Option<f64>) -> String {
    match ratio {
        None => "—".to_owned(),
        Some(ratio) if ratio >= 1.0 => "complete".to_owned(),
        Some(ratio) => format!("{:.1}% — mismatch", ratio * 100.0),
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

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use crate::tracker::{TrackerId, TrackerTorrentId};

    use super::*;

    #[test]
    fn a_mismatched_file_says_so_rather_than_just_a_number() {
        assert_eq!(recheck_progress(None), "—");
        assert_eq!(recheck_progress(Some(1.0)), "complete");
        assert!(recheck_progress(Some(0.0)).contains("mismatch"));
    }

    #[test]
    fn jobs_are_grouped_by_review_reason_with_the_biggest_group_first() {
        let adapter_a = RepairJob {
            id: JobId(1),
            review_reason: Some(ReviewReason::AdapterNotImplemented),
            ..sample_job()
        };
        let adapter_b = RepairJob {
            id: JobId(2),
            review_reason: Some(ReviewReason::AdapterNotImplemented),
            ..sample_job()
        };
        let ambiguous = RepairJob {
            id: JobId(3),
            review_reason: Some(ReviewReason::AmbiguousMatch),
            ..sample_job()
        };
        let jobs = vec![&adapter_a, &adapter_b, &ambiguous];

        let groups = group_by_review_reason(&jobs);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, Some(ReviewReason::AdapterNotImplemented));
        assert_eq!(groups[0].1.len(), 2, "the two-job group sorts first");
        assert_eq!(groups[1].0, Some(ReviewReason::AmbiguousMatch));
        assert_eq!(groups[1].1.len(), 1);
    }

    #[test]
    fn the_approve_resume_button_only_appears_when_that_is_why_the_job_is_parked() {
        let parked_on_auto_resume = RepairJob {
            state: RepairState::AwaitingReview,
            review_from_state: Some(RepairState::Verified),
            review_reason: Some(ReviewReason::AutoResumeDisabled),
            ..sample_job()
        };
        assert!(
            review_panel(&parked_on_auto_resume, &[], &[])
                .into_string()
                .contains("Approve resume")
        );

        let parked_for_another_reason = RepairJob {
            state: RepairState::AwaitingReview,
            review_from_state: Some(RepairState::Matched),
            review_reason: Some(ReviewReason::AmbiguousMatch),
            ..sample_job()
        };
        assert!(
            !review_panel(&parked_for_another_reason, &[], &[])
                .into_string()
                .contains("Approve resume"),
            "approval must only be offered on the one review reason it can override"
        );
    }

    #[test]
    fn a_candidate_picker_renders_only_for_files_the_park_recorded_candidates_for() {
        let job = RepairJob {
            state: RepairState::AwaitingReview,
            review_from_state: Some(RepairState::TorrentFetched),
            review_reason: Some(ReviewReason::AmbiguousMatch),
            ..sample_job()
        };
        let files = vec![
            PlannedFile {
                torrent_path: crate::torrent::SafeRelativePath::parse("Show/e01.mkv").unwrap(),
                length: 100,
                source: None,
                confidence: None,
                evidence: None,
                materialized_as: None,
                recheck_progress: None,
            },
            PlannedFile {
                torrent_path: crate::torrent::SafeRelativePath::parse("Show/e02.mkv").unwrap(),
                length: 200,
                source: Some(std::path::PathBuf::from("/media/e02.mkv")),
                confidence: Some(crate::library::MatchConfidence::Probable),
                evidence: None,
                materialized_as: None,
                recheck_progress: None,
            },
        ];
        let history = vec![TransitionRecord {
            from: RepairState::TorrentFetched,
            to: RepairState::AwaitingReview,
            reason: "review".to_owned(),
            detail: Some(serde_json::json!({
                "unmatched": [{
                    "path": "Show/e01.mkv",
                    "reason": { "ambiguous": { "candidates": [
                        { "path": "/media/a.mkv", "origin": { "kind": "filesystem", "root": "/media" } },
                        { "path": "/media/b.mkv", "origin": { "kind": "filesystem", "root": "/media" } },
                    ] } },
                }],
            })),
            occurred_at: Utc::now(),
        }];

        let rendered = candidate_pickers(&job, &files, &history).into_string();
        assert!(rendered.contains("Show/e01.mkv"), "{rendered}");
        assert!(rendered.contains("/media/a.mkv"), "{rendered}");
        assert!(rendered.contains("/media/b.mkv"), "{rendered}");
        assert!(
            !rendered.contains("e02.mkv"),
            "an already-matched file has no candidates to pick from: {rendered}"
        );
    }

    #[test]
    fn human_duration_picks_the_coarsest_useful_unit() {
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(125), "2m 5s");
        assert_eq!(human_duration(3 * 3600 + 61), "3h 1m");
    }

    fn sample_job() -> RepairJob {
        RepairJob {
            id: JobId(1),
            tracker: TrackerId::new("test-tracker"),
            torrent_id: TrackerTorrentId::new("1"),
            torrent_name: "Demo.Show.S01".to_owned(),
            state: RepairState::Seeding,
            review_from_state: None,
            review_reason: None,
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

    #[test]
    fn a_seeding_job_with_no_progress_yet_shows_no_notice() {
        let job = sample_job();
        assert!(seeding_progress_notice(&job).into_string().is_empty());
    }

    #[test]
    fn a_completed_job_says_its_staging_directory_is_kept() {
        let job = RepairJob {
            state: RepairState::Completed,
            staging_dir: Some(crate::torrent::SafeRelativePath::parse("job-1").unwrap()),
            ..sample_job()
        };
        let rendered = completed_notice(&job, None).into_string();
        assert!(rendered.contains("job-1"), "{rendered}");
        assert!(rendered.contains("kept"), "{rendered}");
    }

    #[test]
    fn a_completed_job_with_a_known_size_reports_it() {
        let job = RepairJob {
            state: RepairState::Completed,
            staging_dir: Some(crate::torrent::SafeRelativePath::parse("job-1").unwrap()),
            ..sample_job()
        };
        let rendered = completed_notice(&job, Some(3 * 1024)).into_string();
        assert!(rendered.contains("3.0 KiB"), "{rendered}");
    }

    #[test]
    fn seeding_progress_shows_uploaded_bytes_and_client_seed_time() {
        let job = RepairJob {
            uploaded_bytes: Some(5 * 1024 * 1024 * 1024),
            seeding_seconds: Some(3 * 3600 + 61),
            ..sample_job()
        };
        let rendered = seeding_progress_notice(&job).into_string();
        assert!(rendered.contains("5.0 GiB"), "{rendered}");
        assert!(rendered.contains("3h 1m"), "{rendered}");
        assert!(
            rendered.contains("client"),
            "the client's own count must be labelled as such, not confused with the tracker's: {rendered}"
        );
    }
}
