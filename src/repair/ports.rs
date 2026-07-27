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

/// A file in the repair plan, as persisted.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedFile {
    pub torrent_path: SafeRelativePath,
    pub length: u64,
    pub source: Option<PathBuf>,
    pub confidence: Option<MatchConfidence>,
    pub evidence: Option<MatchEvidence>,
    pub materialized_as: Option<MaterializationStrategy>,
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
    /// Replaces the whole file plan when set.
    pub files: Option<Vec<PlannedFile>>,
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

    /// Jobs the worker still has work to do on. Used by startup reconciliation.
    async fn unfinished(&self) -> Result<Vec<RepairJob>, StoreError>;

    async fn torrent_file(&self, id: JobId) -> Result<Option<Vec<u8>>, StoreError>;

    async fn planned_files(&self, id: JobId) -> Result<Vec<PlannedFile>, StoreError>;

    async fn history(&self, id: JobId) -> Result<Vec<TransitionRecord>, StoreError>;

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
}
