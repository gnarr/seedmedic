//! The wire types.
//!
//! Six response shapes, one per screen, plus a few element views — not one per
//! operation. The root `AGENTS.md` lists "a separate command/query/event/handler
//! /DTO type per operation" under things deliberately not built, and the test I
//! apply is: does this type exist because an *operation* exists, or because a
//! *representation* differs? Operation-driven types are the banned thing.
//! Representation-driven ones are the cost of having a wire format at all, and
//! refusing them does not remove that cost — it moves it into the client, where
//! the *rules* would then be duplicated, which the same document forbids more
//! strongly.
//!
//! Two conversions here are load-bearing rather than incidental:
//!
//! - [`path_text`] never lets a non-UTF-8 filesystem path fail a response.
//! - [`finite`] never lets a non-finite float fail one.
//!
//! Both are `serde_json` errors that would otherwise turn one oddly-named
//! library file into a 500 for a whole page.

use std::path::Path;

use serde::Serialize;

use crate::{
    library::{CandidateOrigin, CandidateSummary, MatchConfidence, MatchEvidence},
    repair::{PlannedFile, RepairJob, RepairState, ReviewReason, TransitionRecord},
    staging::MaterializationStrategy,
};

/// A filesystem path as text.
///
/// `to_string_lossy`, deliberately: a library path is not required to be UTF-8
/// on Linux, and serde's own `PathBuf` impl *fails* on one — which would turn a
/// single oddly-named library file into a 500 for the whole job-detail page. It
/// is safe here because a path is never round-tripped back through this type: an
/// operator picks a candidate by index into a server-held list, never by path
/// (see `web::review::choose_candidate`), so a replaced byte cannot become a
/// wrong filesystem operation.
pub fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Drop a non-finite float.
///
/// JSON has no NaN or Infinity and `serde_json` *errors* on one. SQLite converts
/// NaN to NULL on insert, so the read path cannot produce one today — two lines
/// to remove an entire class of 500 anyway.
pub fn finite(value: Option<f64>) -> Option<f64> {
    value.filter(|value| value.is_finite())
}

/// A repair job on the wire: the persisted record, flattened, plus the handful
/// of things every screen derives from it.
///
/// Derived here rather than in the client so the progress indicator, the state
/// ordering and the operator copy have exactly one definition — `src/web`'s
/// `AGENTS.md` rule that the web layer holds no rules of its own applies with
/// more force now that the client is a separate program that could disagree.
#[derive(Serialize)]
pub struct JobView<'a> {
    #[serde(flatten)]
    job: &'a RepairJob,
    /// `RepairState::rank` — position on the happy path, `null` off it.
    state_rank: Option<usize>,
    /// `PROGRESSION.len()`, so the client needs no hard-coded constant that
    /// could drift from the lifecycle.
    state_total: usize,
    is_terminal: bool,
    is_actionable: bool,
    /// One line saying where this stands, from the same logic the maud UI used.
    explain: String,
    /// `ReviewReason::description` — 19 pieces of operator prose that stay in
    /// `repair::domain` rather than being restated in TypeScript.
    review_reason_description: Option<&'static str>,
}

impl<'a> JobView<'a> {
    pub fn new(job: &'a RepairJob) -> Self {
        Self {
            job,
            state_rank: job.state.rank(),
            state_total: RepairState::PROGRESSION.len(),
            is_terminal: job.state.is_terminal(),
            is_actionable: job.state.is_actionable(),
            explain: explain(job),
            review_reason_description: job.review_reason.map(ReviewReason::description),
        }
    }
}

/// One line explaining where a job stands. Moved verbatim from `web::jobs`.
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

/// One file of the repair plan, plus the candidates an operator may choose from.
///
/// Hand-written rather than flattening [`PlannedFile`], precisely so `source`
/// goes through [`path_text`] instead of serde's `PathBuf` impl.
#[derive(Serialize)]
pub struct FileView {
    pub torrent_path: String,
    pub length: u64,
    pub source: Option<String>,
    pub confidence: Option<MatchConfidence>,
    /// Persisted for every file and displayed nowhere before 0021 — the "why do
    /// we believe this" an operator needs before approving an ambiguous match.
    pub evidence: Option<MatchEvidence>,
    pub materialized_as: Option<MaterializationStrategy>,
    pub recheck_progress: Option<f64>,
    pub candidates: Vec<CandidateChoice>,
}

impl FileView {
    pub fn new(file: &PlannedFile, candidates: &[CandidateSummary]) -> Self {
        Self {
            torrent_path: file.torrent_path.as_str().to_owned(),
            length: file.length,
            source: file.source.as_deref().map(path_text),
            confidence: file.confidence,
            evidence: file.evidence,
            materialized_as: file.materialized_as,
            recheck_progress: finite(file.recheck_progress),
            candidates: candidates
                .iter()
                .enumerate()
                .map(|(index, candidate)| CandidateChoice {
                    index,
                    path: path_text(&candidate.path),
                    origin: candidate.origin.clone(),
                })
                .collect(),
        }
    }
}

/// A candidate the operator may pick, with the index the server will resolve it
/// by.
///
/// `index` is emitted explicitly rather than left implicit in the array order,
/// so a client that filters or re-sorts the list cannot then send an index that
/// means a different file. The server resolves it against its own list from the
/// parking transition — never against a path from the request.
#[derive(Serialize)]
pub struct CandidateChoice {
    pub index: usize,
    pub path: String,
    pub origin: CandidateOrigin,
}

/// What an operator may do to a job right now, and why not when they may not.
///
/// Computed on the server, deliberately. The maud UI hard-coded which buttons
/// existed; if the client re-derived that from `state`, "which action is legal"
/// would live in Rust *and* TypeScript — exactly what `src/web/AGENTS.md`
/// forbids. `why` is the same string the action itself would return, so a
/// disabled tooltip and a 409 body cannot disagree.
#[derive(Serialize)]
pub struct Action {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<&'static str>,
    /// For `retry`: the step it will resume at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_to: Option<RepairState>,
    /// For `choose_candidate`: how many files still need a decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unresolved_files: Option<usize>,
}

impl Action {
    pub fn available() -> Self {
        Self {
            available: true,
            why: None,
            resume_to: None,
            unresolved_files: None,
        }
    }

    pub fn unavailable(why: &'static str) -> Self {
        Self {
            available: false,
            why: Some(why),
            resume_to: None,
            unresolved_files: None,
        }
    }

    pub fn resuming_at(state: RepairState) -> Self {
        Self {
            resume_to: Some(state),
            ..Self::available()
        }
    }

    pub fn with_unresolved(count: usize) -> Self {
        Self {
            unresolved_files: Some(count),
            ..Self::available()
        }
    }
}

/// Every action the job detail screen can offer.
#[derive(Serialize)]
pub struct Actions {
    pub retry: Action,
    pub restart: Action,
    pub abandon: Action,
    pub abandon_and_discard: Action,
    pub approve_resume: Action,
    pub choose_candidate: Action,
    pub discard_staging: Action,
}

/// One audit entry.
#[derive(Serialize)]
pub struct HistoryView<'a> {
    #[serde(flatten)]
    record: &'a TransitionRecord,
}

impl<'a> HistoryView<'a> {
    pub fn new(record: &'a TransitionRecord) -> Self {
        Self { record }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{repair::JobId, tracker::TrackerId, tracker::TrackerTorrentId};
    use chrono::Utc;

    fn job(state: RepairState) -> RepairJob {
        RepairJob {
            id: JobId(1),
            tracker: TrackerId::new("demo"),
            torrent_id: TrackerTorrentId::new("t-1"),
            torrent_name: "Demo.Release".to_owned(),
            state,
            review_from_state: None,
            review_reason: None,
            failure_reason: None,
            info_hash: None,
            total_bytes: Some(1024),
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

    /// `JobView` flattens a borrowed `RepairJob`, which means a new column
    /// silently widens the wire contract. Pin the key set so widening it is a
    /// deliberate edit here — the same discipline as
    /// `sqlite::tests::the_job_column_list_matches_the_schema`.
    #[test]
    fn the_job_view_names_every_field_deliberately() {
        let job = job(RepairState::Seeding);
        let value = serde_json::to_value(JobView::new(&job)).expect("serialises");
        let object = value.as_object().expect("an object");

        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();

        let mut expected = vec![
            // The 22 persisted fields.
            "id",
            "tracker",
            "torrent_id",
            "torrent_name",
            "state",
            "review_from_state",
            "review_reason",
            "failure_reason",
            "info_hash",
            "total_bytes",
            "staging_dir",
            "materialization",
            "deadline",
            "uploaded_bytes",
            "seeding_seconds",
            "rechecking_started_at",
            "consecutive_unknown_tracker_status",
            "resume_approved",
            "attempts",
            "next_attempt_at",
            "created_at",
            "updated_at",
            // Plus what the view derives.
            "state_rank",
            "state_total",
            "is_terminal",
            "is_actionable",
            "explain",
            "review_reason_description",
        ];
        expected.sort_unstable();

        assert_eq!(
            keys, expected,
            "the JobView key set changed. If that was deliberate, update this \
             list and the TypeScript type; if it was a new database column \
             leaking onto a versioned API, it was not."
        );
    }

    #[test]
    fn a_non_finite_recheck_progress_is_dropped_rather_than_failing_the_response() {
        assert_eq!(finite(Some(0.5)), Some(0.5));
        assert_eq!(finite(Some(f64::NAN)), None);
        assert_eq!(finite(Some(f64::INFINITY)), None);
        assert_eq!(finite(None), None);
    }

    /// serde's own `PathBuf` impl returns an error for a non-UTF-8 path, which
    /// `axum::Json` turns into a 500 for the whole response. One badly named
    /// library file must not take a page down.
    #[test]
    fn a_non_utf8_path_still_serialises() {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::ffi::OsStr::from_bytes(b"/srv/media/bad\xff.mkv");
        let path = Path::new(raw);

        assert!(
            serde_json::to_value(path).is_err(),
            "if serde ever starts accepting these, path_text can go"
        );
        assert!(path_text(path).contains("bad"));
        assert!(serde_json::to_value(path_text(path)).is_ok());
    }

    #[test]
    fn the_state_total_comes_from_the_lifecycle_not_a_literal() {
        let discovered = job(RepairState::Discovered);
        let view = JobView::new(&discovered);
        assert_eq!(view.state_total, 9);
        assert_eq!(view.state_rank, Some(0));

        let parked = job(RepairState::AwaitingReview);
        let view = JobView::new(&parked);
        assert_eq!(
            view.state_rank, None,
            "a parked job is off the happy path and has no position on it"
        );
    }
}
