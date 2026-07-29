//! A [`RepairStore`] decorator that fails one chosen `apply` call.
//!
//! Every side-effecting step follows the same shape: perform the external
//! effect, then call `apply` to record the transition. The crash that matters
//! is the process dying in the gap between those two things — the effect
//! happened, but nothing durable says so yet. `FailAt` reproduces exactly that
//! gap without a real process restart: it lets the underlying store's `apply`
//! run for every call except the Nth, which it fails instead, as if the write
//! never reached disk. The next tick replays the step from scratch, which is
//! precisely the property every step must have.
//!
//! Gated behind `#[cfg(test)]` implicitly by living under `tests/`: this type
//! is not reachable from the production binary.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use seedmedic::{
    repair::{
        Applied, Discovered, JobId, JobPatch, PlannedFile, RepairJob, RepairState, RepairStore,
        StoreError, Transition, TransitionRecord, TransitionUpdate,
    },
    tracker::HitAndRun,
};

/// Wraps a [`RepairStore`] so its `count`-th call to `apply` fails, once.
pub struct FailAt {
    inner: Arc<dyn RepairStore>,
    calls: AtomicUsize,
    fail_call: usize,
}

impl FailAt {
    /// `fail_call` is 1-indexed: `1` fails the first `apply` a driven job
    /// makes, `2` the second, and so on. `0` never fails anything, which makes
    /// the all-crash-points loop's control case (no crash at all) expressible
    /// with the same helper.
    pub fn wrapping(inner: Arc<dyn RepairStore>, fail_call: usize) -> Arc<dyn RepairStore> {
        Arc::new(Self {
            inner,
            calls: AtomicUsize::new(0),
            fail_call,
        })
    }
}

#[async_trait]
impl RepairStore for FailAt {
    async fn record_discovery(&self, hit_and_run: &HitAndRun) -> Result<Discovered, StoreError> {
        self.inner.record_discovery(hit_and_run).await
    }

    async fn job(&self, id: JobId) -> Result<Option<RepairJob>, StoreError> {
        self.inner.job(id).await
    }

    async fn jobs(&self, limit: i64) -> Result<Vec<RepairJob>, StoreError> {
        self.inner.jobs(limit).await
    }

    async fn unfinished(&self) -> Result<Vec<RepairJob>, StoreError> {
        self.inner.unfinished().await
    }

    async fn parked(&self) -> Result<Vec<RepairJob>, StoreError> {
        self.inner.parked().await
    }

    async fn torrent_file(&self, id: JobId) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.torrent_file(id).await
    }

    async fn planned_files(&self, id: JobId) -> Result<Vec<PlannedFile>, StoreError> {
        self.inner.planned_files(id).await
    }

    async fn history(&self, id: JobId) -> Result<Vec<TransitionRecord>, StoreError> {
        self.inner.history(id).await
    }

    async fn set_review_resume_point(
        &self,
        id: JobId,
        state: RepairState,
    ) -> Result<(), StoreError> {
        self.inner.set_review_resume_point(id, state).await
    }

    async fn apply(
        &self,
        id: JobId,
        transition: Transition,
        update: TransitionUpdate,
    ) -> Result<Applied, StoreError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if call == self.fail_call {
            return Err(StoreError::Database(format!(
                "injected crash: the process died before recording {} -> {}",
                transition.from(),
                transition.to()
            )));
        }
        self.inner.apply(id, transition, update).await
    }

    async fn record_progress(&self, id: JobId, patch: JobPatch) -> Result<(), StoreError> {
        self.inner.record_progress(id, patch).await
    }

    async fn claim(
        &self,
        owner: &str,
        lease: std::time::Duration,
        limit: i64,
    ) -> Result<Vec<RepairJob>, StoreError> {
        self.inner.claim(owner, lease, limit).await
    }

    async fn release(
        &self,
        id: JobId,
        retry_at: Option<DateTime<Utc>>,
        count_attempt: bool,
    ) -> Result<(), StoreError> {
        self.inner.release(id, retry_at, count_attempt).await
    }

    async fn renew_lease(
        &self,
        id: JobId,
        owner: &str,
        lease: std::time::Duration,
    ) -> Result<bool, StoreError> {
        self.inner.renew_lease(id, owner, lease).await
    }

    async fn clear_stale_leases(&self, owner: &str) -> Result<u64, StoreError> {
        self.inner.clear_stale_leases(owner).await
    }

    async fn ping(&self) -> Result<(), StoreError> {
        self.inner.ping().await
    }

    async fn has_active_lease(&self) -> Result<bool, StoreError> {
        self.inner.has_active_lease().await
    }
}
