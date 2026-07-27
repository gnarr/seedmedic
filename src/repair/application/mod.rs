//! One module per step of the lifecycle.
//!
//! Every step is a function from a job to a [`StepOutcome`]. Steps do not
//! persist anything: the worker turns the outcome into exactly one transition,
//! which is what keeps "did the side effect happen?" and "was it recorded?" a
//! single question rather than two.
//!
//! Steps must therefore be safe to run again. Where a step performs an external
//! side effect, that effect must be idempotent (see the port contracts), because
//! a crash between the effect and the transition will replay it.

mod acquire;
mod confirm;
mod discover;
mod inject;
mod match_media;
mod stage;
mod verify;

use std::time::Duration;

use serde_json::Value;

pub use discover::{DiscoverySummary, discover_hit_and_runs};

use crate::repair::{
    domain::{RepairJob, RepairState, ReviewReason},
    ports::JobPatch,
    worker::RepairDeps,
};

/// What a step concluded. Deliberately small: there are only so many honest
/// answers to "what happened?".
#[derive(Debug)]
pub enum StepOutcome {
    /// The step succeeded. Move to the next state.
    Advance {
        detail: Option<Value>,
        patch: JobPatch,
    },
    /// A human has to decide. Park the job.
    Review {
        reason: ReviewReason,
        detail: Option<Value>,
        /// Field updates worth keeping even though the job is parking, not
        /// advancing — recheck progress is the motivating case: the review
        /// page needs it precisely when the recheck did not go cleanly.
        patch: JobPatch,
    },
    /// Reality is behind the persisted state — the torrent is not in the
    /// client any more, the staged files are gone. Move the job back to the
    /// last state that is still true and let it re-do the work.
    ///
    /// The same correction startup reconciliation makes, available mid-flight
    /// so a repair recovers without waiting for a restart.
    Rewind { to: RepairState, note: String },
    /// Not ready yet — a recheck is running, a tracker has not updated. Poll
    /// again later *without* spending an attempt.
    Wait {
        after: Duration,
        note: String,
        /// Telemetry worth persisting even though nothing durable "happened"
        /// — the tracker's unknown-answer streak, seeding progress. Applied
        /// without an audit row: it is not a decision, so it does not belong
        /// in the audit trail.
        patch: JobPatch,
    },
    /// Something failed in a way that might not fail next time. Costs an
    /// attempt; running out of attempts parks the job for review.
    Retry { error: String },
}

impl StepOutcome {
    pub fn advance() -> Self {
        Self::Advance {
            detail: None,
            patch: JobPatch::default(),
        }
    }

    pub fn advance_with(detail: Value, patch: JobPatch) -> Self {
        Self::Advance {
            detail: Some(detail),
            patch,
        }
    }

    pub fn review(reason: ReviewReason, detail: Value) -> Self {
        Self::Review {
            reason,
            detail: Some(detail),
            patch: JobPatch::default(),
        }
    }

    pub fn review_with(reason: ReviewReason, detail: Value, patch: JobPatch) -> Self {
        Self::Review {
            reason,
            detail: Some(detail),
            patch,
        }
    }

    pub fn retry(error: impl std::fmt::Display) -> Self {
        Self::Retry {
            error: error.to_string(),
        }
    }

    pub fn wait(after: Duration, note: impl Into<String>) -> Self {
        Self::Wait {
            after,
            note: note.into(),
            patch: JobPatch::default(),
        }
    }

    pub fn wait_with(after: Duration, note: impl Into<String>, patch: JobPatch) -> Self {
        Self::Wait {
            after,
            note: note.into(),
            patch,
        }
    }

    pub fn rewind(to: RepairState, note: impl Into<String>) -> Self {
        Self::Rewind {
            to,
            note: note.into(),
        }
    }
}

/// Dispatch a job to the step its state calls for.
///
/// The exhaustive match is the point: adding a state to the lifecycle makes
/// this fail to compile until somebody decides what happens in it.
pub async fn step(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    match job.state {
        RepairState::Discovered => acquire::fetch_torrent(deps, job).await,
        RepairState::TorrentFetched => match_media::match_media(deps, job).await,
        RepairState::Matched => stage::stage_files(deps, job).await,
        RepairState::Staged => inject::inject(deps, job).await,
        RepairState::Injected => inject::start_recheck(deps, job).await,
        RepairState::Rechecking => verify::verify(deps, job).await,
        RepairState::Verified => verify::resume(deps, job).await,
        RepairState::Seeding => confirm::confirm_with_tracker(deps, job).await,
        // Not actionable: the worker never claims these.
        RepairState::Completed | RepairState::AwaitingReview | RepairState::Failed => {
            StepOutcome::wait(
                Duration::from_secs(3600),
                format!("{} is not a state the worker acts on", job.state),
            )
        }
    }
}
