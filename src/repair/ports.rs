use std::{path::PathBuf, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::{
    library::{MatchConfidence, MatchEvidence},
    staging::MaterializationStrategy,
    torrent::{InfoHash, SafeRelativePath},
    tracker::HitAndRun,
};

use super::domain::{
    InvalidTransition, JobId, RepairJob, RepairState, Transition, TransitionRecord,
};

#[derive(Clone, Debug, Error)]
pub enum StoreError {
    #[error("repair job {0} does not exist")]
    Missing(JobId),
    #[error("repair job {id} is in state {actual}, not {expected}")]
    Conflict {
        id: JobId,
        expected: RepairState,
        actual: RepairState,
    },
    #[error(transparent)]
    Invalid(#[from] InvalidTransition),
    #[error("stored repair job {id} cannot be read: {reason}")]
    Corrupt { id: JobId, reason: String },
    #[error("database error: {0}")]
    Database(String),
}

/// Outcome of applying a transition.
///
/// [`Applied::AlreadyInTargetState`] is the idempotency signal: the job was
/// already where we were trying to put it, so nothing changed and no second
/// audit row was written. A step that crashed after its side effect but before
/// its transition can safely be replayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Applied {
    Applied,
    AlreadyInTargetState,
}

/// Result of recording a hit-and-run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Discovered {
    pub id: JobId,
    /// False when the warning was already known — the common case, since
    /// trackers keep showing a hit-and-run until it is cleared.
    pub created: bool,
}

/// How a job list is narrowed and ordered.
///
/// Every field is optional and they compose with AND; the default is the whole
/// table, newest activity first — the order [`RepairStore::jobs`] has always
/// used. Deliberately not a builder and deliberately not generic: it is the
/// argument list of exactly one query, and a struct only because eight
/// positional parameters would be worse.
#[derive(Clone, Debug)]
pub struct JobFilter {
    pub states: Vec<RepairState>,
    pub review_reasons: Vec<super::domain::ReviewReason>,
    pub trackers: Vec<crate::tracker::TrackerId>,
    /// Case-insensitive substring of `torrent_name` — or, when it is exactly 40
    /// hex characters, an exact `info_hash` match. That one special case is
    /// there because it is the query an operator actually types when a recheck
    /// disagrees with the tracker.
    pub search: Option<String>,
    pub sort: JobSort,
    pub descending: bool,
    /// The last row of the previous page. Keyset, never an offset: the worker
    /// mutates this table between one page and the next, and an offset silently
    /// skips and duplicates rows across the boundary.
    pub after: Option<JobCursor>,
    pub limit: i64,
}

impl Default for JobFilter {
    fn default() -> Self {
        Self {
            states: Vec::new(),
            review_reasons: Vec::new(),
            trackers: Vec::new(),
            search: None,
            sort: JobSort::UpdatedAt,
            descending: true,
            after: None,
            limit: 50,
        }
    }
}

/// The columns a job list may be ordered by.
///
/// Two, not four. `deadline` and `attempts` are plausible and unasked-for, and
/// each costs another `(column, direction)` pair of literal SQL whose keyset
/// predicate and `ORDER BY` have to agree exactly — see `page_of_jobs!` in the
/// SQLite adapter. Adding one later is one macro arm.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum JobSort {
    #[default]
    UpdatedAt,
    CreatedAt,
}

/// Where a page of jobs resumes: the sort column's value, and the id that
/// breaks ties on it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobCursor {
    /// The RFC 3339 text as stored, so lexical ordering matches chronological —
    /// the same property the claim query already depends on.
    pub sort_value: String,
    pub id: JobId,
}

/// How many jobs are in each state, and why the parked ones are parked.
///
/// Counted in SQL. The `/status` page used to load every job — every column of
/// `job_columns!()`, parsing six timestamps and four enums per row — and fold
/// them in Rust.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobCounts {
    /// Only states that actually have jobs; a zero count is an absent entry.
    pub by_state: Vec<(RepairState, i64)>,
    /// Parked jobs only. `None` is a job parked before a reason was recorded.
    pub by_review_reason: Vec<(Option<super::domain::ReviewReason>, i64)>,
    pub total: i64,
}

/// A file in the repair plan, as persisted.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedFile {
    pub torrent_path: SafeRelativePath,
    pub length: u64,
    pub source: Option<PathBuf>,
    pub confidence: Option<MatchConfidence>,
    pub evidence: Option<MatchEvidence>,
    pub materialized_as: Option<MaterializationStrategy>,
    /// How much of this file the most recent recheck confirmed, `0.0..=1.0`.
    /// `None` before any recheck has run, or when the client offered no
    /// per-file breakdown.
    pub recheck_progress: Option<f64>,
}

/// One file's completeness from a recheck, keyed by the path already in its
/// `repair_job_files` row. Unlike [`JobPatch::files`], applying this updates
/// existing rows in place rather than replacing the whole plan — a recheck
/// knows nothing about matching or materialization.
#[derive(Clone, Debug, PartialEq)]
pub struct FileCompleteness {
    pub torrent_path: SafeRelativePath,
    pub ratio: f64,
}

/// Job fields a transition may set. Anything left `None` is untouched, so a
/// step only writes what it learned.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobPatch {
    pub info_hash: Option<InfoHash>,
    pub torrent_file: Option<Vec<u8>>,
    pub total_bytes: Option<u64>,
    pub staging_dir: Option<SafeRelativePath>,
    pub materialization: Option<MaterializationStrategy>,
    /// Set on the `injected → rechecking` transition; see
    /// [`RepairJob::rechecking_started_at`].
    pub rechecking_started_at: Option<DateTime<Utc>>,
    /// Replaces the whole file plan when set.
    pub files: Option<Vec<PlannedFile>>,
    /// Updates `recheck_progress` on the named files' existing rows. Applied
    /// after `files`, so setting both in one patch replaces the plan and then
    /// records progress against the new rows.
    pub file_progress: Option<Vec<FileCompleteness>>,
    /// See [`RepairJob::consecutive_unknown_tracker_status`]. `Some(0)` is a
    /// real reset, not "leave alone" — only `None` means that.
    pub consecutive_unknown_tracker_status: Option<u32>,
    /// See [`RepairJob::uploaded_bytes`] and [`RepairJob::seeding_seconds`].
    pub uploaded_bytes: Option<u64>,
    pub seeding_seconds: Option<u64>,
    /// See [`RepairJob::resume_approved`]. `Some(false)` is a real reset, not
    /// "leave alone" — only `None` means that.
    pub resume_approved: Option<bool>,
}

/// Everything written alongside a transition, in the same database transaction.
#[derive(Clone, Debug, Default)]
pub struct TransitionUpdate {
    /// Evidence for the audit trail. This is what makes an automated decision
    /// explainable months later.
    pub detail: Option<serde_json::Value>,
    pub failure_reason: Option<String>,
    pub patch: JobPatch,
}

impl TransitionUpdate {
    pub fn with_detail(detail: serde_json::Value) -> Self {
        Self {
            detail: Some(detail),
            ..Self::default()
        }
    }

    pub fn patch(mut self, patch: JobPatch) -> Self {
        self.patch = patch;
        self
    }

    pub fn failed_because(mut self, reason: impl Into<String>) -> Self {
        self.failure_reason = Some(reason.into());
        self
    }
}

/// Durable repair state.
///
/// The contract that matters is on [`RepairStore::apply`]: it is a
/// compare-and-swap, and it writes the audit row in the same transaction as the
/// state change. Everything else in the system is allowed to crash between any
/// two operations because of that one guarantee.
#[async_trait]
pub trait RepairStore: Send + Sync {
    /// Record a hit-and-run. Idempotent on `(tracker, torrent id)`: seeing the
    /// same warning again returns the existing job untouched.
    async fn record_discovery(&self, hit_and_run: &HitAndRun) -> Result<Discovered, StoreError>;

    async fn job(&self, id: JobId) -> Result<Option<RepairJob>, StoreError>;

    /// Most recently updated first.
    async fn jobs(&self, limit: i64) -> Result<Vec<RepairJob>, StoreError>;

    /// One page of jobs, narrowed and ordered by `filter`.
    ///
    /// Returns at most `filter.limit` rows; the caller builds the next
    /// [`JobCursor`] from the last one.
    async fn find_jobs(&self, filter: &JobFilter) -> Result<Vec<RepairJob>, StoreError>;

    /// How many jobs `filter` matches, ignoring its `after` and `limit`.
    ///
    /// Can disagree with the page it accompanies by a row or two while the
    /// worker is writing, which is why the UI presents it as approximate.
    async fn count_jobs(&self, filter: &JobFilter) -> Result<i64, StoreError>;

    /// Population per state, and per review reason for the parked ones.
    async fn counts(&self) -> Result<JobCounts, StoreError>;

    /// Total `total_bytes` over jobs that have a staging directory.
    ///
    /// What SeedMedic *believes* it is holding, not what is on disk. The honest
    /// measurement is [`crate::staging::StagingFilesystem::usage`], which walks
    /// the filesystem once per job — fine on a page about one repair, far too
    /// expensive for a dashboard that refreshes itself. Callers must label which
    /// of the two they are showing.
    async fn staged_bytes_declared(&self) -> Result<u64, StoreError>;

    /// Jobs with at least `at_least` `reconciliation` audit rows — a job that
    /// keeps being walked backwards and may be oscillating.
    ///
    /// Replaces one [`RepairStore::history`] call per unfinished job.
    async fn rewind_counts(&self, at_least: i64) -> Result<Vec<(JobId, i64)>, StoreError>;

    /// Unfinished jobs per tracker.
    ///
    /// Two callers: the dashboard's tracker panel, and the confirmation shown
    /// before removing a tracker from the configuration — which needs to say how
    /// many repairs the removal would orphan.
    async fn unfinished_by_tracker(
        &self,
    ) -> Result<Vec<(crate::tracker::TrackerId, i64)>, StoreError>;

    /// Jobs the worker still has work to do on. Used by startup reconciliation.
    async fn unfinished(&self) -> Result<Vec<RepairJob>, StoreError>;

    /// Jobs parked for review. Used by startup reconciliation to correct a
    /// resume point without un-parking the job — only an operator does that.
    async fn parked(&self) -> Result<Vec<RepairJob>, StoreError>;

    async fn torrent_file(&self, id: JobId) -> Result<Option<Vec<u8>>, StoreError>;

    async fn planned_files(&self, id: JobId) -> Result<Vec<PlannedFile>, StoreError>;

    async fn history(&self, id: JobId) -> Result<Vec<TransitionRecord>, StoreError>;

    /// Correct where a parked job will resume, writing an audit row with
    /// reason `reconciliation`. Leaves `state` at `awaiting_review` — this
    /// never un-parks a job, it only stops an operator's retry from resuming
    /// somewhere reality no longer supports. A no-op if the job is not
    /// currently parked, or already resumes at `state`.
    async fn set_review_resume_point(
        &self,
        id: JobId,
        state: RepairState,
    ) -> Result<(), StoreError>;

    /// Move a job, atomically with its audit record and any field updates.
    ///
    /// Implementations must compare and swap on the transition's `from` state,
    /// and must return [`Applied::AlreadyInTargetState`] — not an error, and
    /// without writing a second audit row — when the job is already at `to`.
    async fn apply(
        &self,
        id: JobId,
        transition: Transition,
        update: TransitionUpdate,
    ) -> Result<Applied, StoreError>;

    /// Persist field updates without moving state and without an audit row —
    /// used for telemetry that arrives alongside a [`super::application::StepOutcome::Wait`]
    /// (see its `patch` field): the tracker's unknown-answer streak, seeding
    /// progress. Not a decision, so it does not belong in
    /// `repair_job_transitions`.
    async fn record_progress(&self, id: JobId, patch: JobPatch) -> Result<(), StoreError>;

    /// Take a lease on up to `limit` jobs that are due for work.
    ///
    /// A lease is the only thing stopping two workers from acting on one job,
    /// and its expiry is the only thing that recovers a job from a worker that
    /// died. There is no queue to rebuild.
    async fn claim(
        &self,
        owner: &str,
        lease: Duration,
        limit: i64,
    ) -> Result<Vec<RepairJob>, StoreError>;

    /// Give up the lease. `retry_at` schedules the next attempt;
    /// `count_attempt` distinguishes "this failed" from "not ready yet", so
    /// polling does not burn the retry budget.
    async fn release(
        &self,
        id: JobId,
        retry_at: Option<DateTime<Utc>>,
        count_attempt: bool,
    ) -> Result<(), StoreError>;

    /// Extend a held lease. Only takes effect while `owner` still holds it —
    /// a worker that lost its lease to expiry must not reacquire it by
    /// renewing. Returns whether the renewal actually applied.
    async fn renew_lease(
        &self,
        id: JobId,
        owner: &str,
        lease: Duration,
    ) -> Result<bool, StoreError>;

    /// Drop leases that have expired, plus any still held by `owner` — which,
    /// at startup, means the leases this instance was holding when it died.
    /// Called before anything else looks at the jobs.
    async fn clear_stale_leases(&self, owner: &str) -> Result<u64, StoreError>;

    /// Cheapest possible round trip to the database, for `/health`. Proves
    /// only that the connection is alive — nothing about job data.
    async fn ping(&self) -> Result<(), StoreError>;

    /// Whether any job currently holds an unexpired lease.
    ///
    /// Checked before a config reload may apply a new `worker.owner`: leases
    /// are keyed on the owner, so changing it out from under a leased job
    /// would let a second process claim it while the first still holds it —
    /// see `docs/todos/0016-a-swappable-runtime.md` step 11.
    async fn has_active_lease(&self) -> Result<bool, StoreError>;
}
