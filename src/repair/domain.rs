//! The repair lifecycle.
//!
//! One repair job per hit-and-run, advancing through a fixed sequence of
//! states. The sequence is deliberately boring: each state means "everything up
//! to here is durably done", so the worker can always answer "what next?" from
//! the persisted state alone, and a crash costs at most one step's work.

use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;

use crate::{
    staging::MaterializationStrategy,
    torrent::{InfoHash, SafeRelativePath},
    tracker::{TrackerId, TrackerTorrentId},
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct JobId(pub i64);

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairState {
    /// The tracker says there is a hit-and-run. Nothing has been done yet.
    Discovered,
    /// The `.torrent` is downloaded, parsed, and stored on the job.
    TorrentFetched,
    /// Every file in the torrent has a library file chosen for it.
    Matched,
    /// The chosen files exist in the staging area in the torrent's layout.
    Staged,
    /// The download client has the torrent, paused.
    Injected,
    /// A hash check is running.
    Rechecking,
    /// The client agrees the staged data matches the torrent.
    Verified,
    /// The torrent is resumed and uploading.
    Seeding,
    /// The tracker says the hit-and-run is cleared. Terminal.
    Completed,
    /// A human has to decide. See [`RepairJob::review_from_state`].
    AwaitingReview,
    /// Abandoned. Terminal for the worker; an operator can still restart it.
    Failed,
}

impl RepairState {
    /// States on the happy path, in order.
    pub const PROGRESSION: [Self; 9] = [
        Self::Discovered,
        Self::TorrentFetched,
        Self::Matched,
        Self::Staged,
        Self::Injected,
        Self::Rechecking,
        Self::Verified,
        Self::Seeding,
        Self::Completed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::TorrentFetched => "torrent_fetched",
            Self::Matched => "matched",
            Self::Staged => "staged",
            Self::Injected => "injected",
            Self::Rechecking => "rechecking",
            Self::Verified => "verified",
            Self::Seeding => "seeding",
            Self::Completed => "completed",
            Self::AwaitingReview => "awaiting_review",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Result<Self, UnknownState> {
        Self::PROGRESSION
            .into_iter()
            .chain([Self::AwaitingReview, Self::Failed])
            .find(|state| state.as_str() == value)
            .ok_or_else(|| UnknownState(value.to_owned()))
    }

    /// Position on the happy path. `None` for the states that sit off it.
    pub fn rank(self) -> Option<usize> {
        Self::PROGRESSION.iter().position(|state| *state == self)
    }

    /// The one state a successful step may advance to.
    pub fn next(self) -> Option<Self> {
        let rank = self.rank()?;
        Self::PROGRESSION.get(rank + 1).copied()
    }

    /// Terminal means "the worker will not touch it again". An operator can
    /// still restart a `Failed` job; nothing restarts a `Completed` one.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }

    /// Whether the worker should be doing something about this job.
    pub fn is_actionable(self) -> bool {
        !self.is_terminal() && self != Self::AwaitingReview
    }
}

impl std::fmt::Display for RepairState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("unknown repair state `{0}`")]
pub struct UnknownState(pub String);

/// Why a job needs a human. Persisted, so every parked job explains itself.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewReason {
    /// Several library files fit and nothing chose between them.
    AmbiguousMatch,
    /// Nothing in the library is the right size.
    NoCandidates,
    /// A match was found but is weaker than policy allows.
    ConfidenceBelowPolicy,
    /// No permitted materialization strategy works here.
    MaterializationUnavailable,
    /// The staging filesystem does not have enough free space for this plan.
    InsufficientStagingSpace,
    /// The recheck says the staged data is incomplete.
    IncompleteData,
    /// Incomplete *and* hardlinked into the library. Resuming would let the
    /// client write into the user's media. Never automatic.
    AliasedIncompleteData,
    /// A recheck ran longer than `policy.recheck_timeout_seconds` without
    /// finishing. Never resumed and never retried automatically — a stuck
    /// check does not become unstuck by asking again.
    RecheckTimedOut,
    /// Complete and safe, but policy says a human presses the button.
    AutoResumeDisabled,
    /// Too many failed attempts at the same step.
    RetryBudgetExhausted,
    /// The adapter for this step is a stub.
    AdapterNotImplemented,
    /// The tracker answered in a way we will not guess about.
    TrackerStatusUnclear,
    /// The torrent contains paths we refuse to create.
    UnsafeTorrentPaths,
    /// The `.torrent` could not be decoded.
    TorrentUnreadable,
    /// The tracker's info-hash and the `.torrent`'s disagree.
    InfoHashMismatch,
    /// A library file moved or changed size between matching and staging.
    LibraryChanged,
}

impl ReviewReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AmbiguousMatch => "ambiguous_match",
            Self::NoCandidates => "no_candidates",
            Self::ConfidenceBelowPolicy => "confidence_below_policy",
            Self::MaterializationUnavailable => "materialization_unavailable",
            Self::InsufficientStagingSpace => "insufficient_staging_space",
            Self::IncompleteData => "incomplete_data",
            Self::AliasedIncompleteData => "aliased_incomplete_data",
            Self::RecheckTimedOut => "recheck_timed_out",
            Self::AutoResumeDisabled => "auto_resume_disabled",
            Self::RetryBudgetExhausted => "retry_budget_exhausted",
            Self::AdapterNotImplemented => "adapter_not_implemented",
            Self::TrackerStatusUnclear => "tracker_status_unclear",
            Self::UnsafeTorrentPaths => "unsafe_torrent_paths",
            Self::TorrentUnreadable => "torrent_unreadable",
            Self::InfoHashMismatch => "info_hash_mismatch",
            Self::LibraryChanged => "library_changed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [
            Self::AmbiguousMatch,
            Self::NoCandidates,
            Self::ConfidenceBelowPolicy,
            Self::MaterializationUnavailable,
            Self::InsufficientStagingSpace,
            Self::IncompleteData,
            Self::AliasedIncompleteData,
            Self::RecheckTimedOut,
            Self::AutoResumeDisabled,
            Self::RetryBudgetExhausted,
            Self::AdapterNotImplemented,
            Self::TrackerStatusUnclear,
            Self::UnsafeTorrentPaths,
            Self::TorrentUnreadable,
            Self::InfoHashMismatch,
            Self::LibraryChanged,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == value)
    }

    /// One line for the operator, in the web UI.
    pub fn description(self) -> &'static str {
        match self {
            Self::AmbiguousMatch => "Several library files fit; none could be chosen safely.",
            Self::NoCandidates => "No library file matches the torrent's contents.",
            Self::ConfidenceBelowPolicy => "The best match is weaker than the configured minimum.",
            Self::MaterializationUnavailable => {
                "No permitted way to stage the files works on this filesystem."
            }
            Self::InsufficientStagingSpace => {
                "Not enough free space on the staging filesystem for this repair."
            }
            Self::IncompleteData => "The recheck found the staged data incomplete.",
            Self::AliasedIncompleteData => {
                "The staged data is incomplete and hardlinked to the library. \
                 Resuming would let the client write into your media."
            }
            Self::RecheckTimedOut => {
                "The recheck did not finish within the configured time limit. \
                 It may still be running in the download client — check there \
                 before retrying."
            }
            Self::AutoResumeDisabled => {
                "Verified and safe to resume, but policy.auto_resume is \"never\". \
                 Set it to \"when_verified_complete\" and retry. Per-job approval \
                 from this page is docs/todos/0010-manual-review.md."
            }
            Self::RetryBudgetExhausted => "This step failed too many times in a row.",
            Self::AdapterNotImplemented => "The integration needed for this step is not built yet.",
            Self::TrackerStatusUnclear => "The tracker's answer could not be interpreted.",
            Self::UnsafeTorrentPaths => {
                "The torrent contains file paths SeedMedic refuses to create."
            }
            Self::TorrentUnreadable => "The .torrent file could not be decoded.",
            Self::InfoHashMismatch => {
                "The tracker's info-hash does not match the .torrent it served."
            }
            Self::LibraryChanged => {
                "A matched library file moved or changed size before it could be staged."
            }
        }
    }
}

/// Why a transition is happening. Determines which transitions are legal, and
/// is written to the audit trail verbatim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionReason {
    /// A step succeeded.
    Progress,
    /// Something needs a human.
    Review(ReviewReason),
    /// Unrecoverable.
    Failure,
    /// An operator sent a parked job back to the step it stopped at.
    OperatorRetry,
    /// An operator gave up on a parked job.
    OperatorAbandon,
    /// An operator sent a job back to the start. Discards staged data.
    OperatorRestart,
    /// Startup found reality behind the persisted state and moved the job back.
    Reconciliation,
}

impl TransitionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Review(_) => "review",
            Self::Failure => "failure",
            Self::OperatorRetry => "operator_retry",
            Self::OperatorAbandon => "operator_abandon",
            Self::OperatorRestart => "operator_restart",
            Self::Reconciliation => "reconciliation",
        }
    }
}

/// A validated state change. The only way to construct one is
/// [`validate_transition`], so a `Transition` value is proof the rules held.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Transition {
    from: RepairState,
    to: RepairState,
    reason: TransitionReason,
}

impl Transition {
    pub fn from(&self) -> RepairState {
        self.from
    }

    pub fn to(&self) -> RepairState {
        self.to
    }

    pub fn reason(&self) -> TransitionReason {
        self.reason
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("cannot move a repair from {from} to {to} ({reason}): {because}")]
pub struct InvalidTransition {
    pub from: RepairState,
    pub to: RepairState,
    pub reason: &'static str,
    pub because: &'static str,
}

/// The whole transition table, in one place.
///
/// `review_from` is the state a parked job must return to; it is only consulted
/// for [`TransitionReason::OperatorRetry`], and its absence there is itself an
/// error rather than a licence to guess.
pub fn validate_transition(
    from: RepairState,
    to: RepairState,
    reason: TransitionReason,
    review_from: Option<RepairState>,
) -> Result<Transition, InvalidTransition> {
    let reject = |because| InvalidTransition {
        from,
        to,
        reason: reason.as_str(),
        because,
    };
    let ok = || Ok(Transition { from, to, reason });

    match reason {
        TransitionReason::Progress => {
            if from.next() == Some(to) {
                ok()
            } else {
                Err(reject(
                    "progress may only advance one step along the lifecycle",
                ))
            }
        }
        TransitionReason::Review(_) => {
            if to != RepairState::AwaitingReview {
                Err(reject("review must move to awaiting_review"))
            } else if !from.is_actionable() {
                Err(reject("only a job the worker is acting on can be parked"))
            } else {
                ok()
            }
        }
        TransitionReason::Failure => {
            if to != RepairState::Failed {
                Err(reject("failure must move to failed"))
            } else if from.is_terminal() {
                Err(reject("a terminal job cannot fail again"))
            } else {
                ok()
            }
        }
        TransitionReason::OperatorRetry => {
            if from != RepairState::AwaitingReview {
                Err(reject("only a parked job can be retried"))
            } else if review_from != Some(to) {
                Err(reject(
                    "a retry must resume the exact step the job stopped at",
                ))
            } else {
                ok()
            }
        }
        TransitionReason::OperatorAbandon => {
            if from != RepairState::AwaitingReview {
                Err(reject("only a parked job can be abandoned"))
            } else if to != RepairState::Failed {
                Err(reject("abandoning must move to failed"))
            } else {
                ok()
            }
        }
        TransitionReason::OperatorRestart => {
            if !matches!(from, RepairState::AwaitingReview | RepairState::Failed) {
                Err(reject("only a parked or failed job can be restarted"))
            } else if to != RepairState::Discovered {
                Err(reject("a restart must return to discovered"))
            } else {
                ok()
            }
        }
        TransitionReason::Reconciliation => match (from.rank(), to.rank()) {
            (Some(from_rank), Some(to_rank)) if to_rank < from_rank => ok(),
            (Some(_), Some(_)) => Err(reject("reconciliation may only move a job backwards")),
            _ => Err(reject(
                "reconciliation only applies to the lifecycle states",
            )),
        },
    }
}

/// A repair job as persisted.
///
/// The `.torrent` bytes and the per-file plan are stored alongside it but
/// fetched separately: most of the time the workflow only needs the header.
#[derive(Clone, Debug, PartialEq)]
pub struct RepairJob {
    pub id: JobId,
    pub tracker: TrackerId,
    pub torrent_id: TrackerTorrentId,
    pub torrent_name: String,
    pub state: RepairState,
    /// Set exactly when `state == AwaitingReview`.
    pub review_from_state: Option<RepairState>,
    pub review_reason: Option<ReviewReason>,
    pub failure_reason: Option<String>,
    pub info_hash: Option<InfoHash>,
    pub total_bytes: Option<u64>,
    pub staging_dir: Option<SafeRelativePath>,
    pub materialization: Option<MaterializationStrategy>,
    /// When the current `rechecking` episode began — the timestamp of the
    /// `injected → rechecking` transition. Drives the adaptive poll backoff
    /// and the recheck ceiling; unset outside that state.
    pub rechecking_started_at: Option<DateTime<Utc>>,
    pub attempts: u32,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RepairJob {
    /// Validate a transition for *this* job, so callers cannot forget to pass
    /// `review_from_state`.
    pub fn plan_transition(
        &self,
        to: RepairState,
        reason: TransitionReason,
    ) -> Result<Transition, InvalidTransition> {
        validate_transition(self.state, to, reason, self.review_from_state)
    }

    /// The step a successful attempt would complete.
    pub fn advance(&self) -> Result<Transition, InvalidTransition> {
        let to = self.state.next().unwrap_or(self.state);
        self.plan_transition(to, TransitionReason::Progress)
    }

    /// Where this job's files live, relative to the staging root.
    pub fn default_staging_dir(&self) -> SafeRelativePath {
        SafeRelativePath::parse(&format!("job-{}", self.id))
            .expect("job directory names are generated, not supplied")
    }
}

/// One row of the audit trail.
#[derive(Clone, Debug, PartialEq)]
pub struct TransitionRecord {
    pub from: RepairState,
    pub to: RepairState,
    pub reason: String,
    pub detail: Option<serde_json::Value>,
    pub occurred_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(
        from: RepairState,
        to: RepairState,
        reason: TransitionReason,
    ) -> Result<Transition, InvalidTransition> {
        validate_transition(from, to, reason, None)
    }

    #[test]
    fn every_state_round_trips_through_its_string_form() {
        for state in RepairState::PROGRESSION
            .into_iter()
            .chain([RepairState::AwaitingReview, RepairState::Failed])
        {
            assert_eq!(RepairState::parse(state.as_str()), Ok(state));
        }
        assert!(RepairState::parse("nonsense").is_err());
    }

    #[test]
    fn progress_walks_the_lifecycle_one_step_at_a_time() {
        let mut state = RepairState::Discovered;
        while let Some(next) = state.next() {
            validate(state, next, TransitionReason::Progress)
                .unwrap_or_else(|error| panic!("{state} -> {next} must be legal: {error}"));
            state = next;
        }
        assert_eq!(state, RepairState::Completed);
    }

    #[test]
    fn progress_cannot_skip_a_step() {
        assert!(
            validate(
                RepairState::Matched,
                RepairState::Injected,
                TransitionReason::Progress
            )
            .is_err()
        );
    }

    #[test]
    fn progress_cannot_go_backwards() {
        assert!(
            validate(
                RepairState::Staged,
                RepairState::Matched,
                TransitionReason::Progress
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_states_do_not_advance() {
        assert_eq!(RepairState::Completed.next(), None);
        assert!(RepairState::Completed.is_terminal());
        assert!(RepairState::Failed.is_terminal());
        assert!(!RepairState::AwaitingReview.is_terminal());
        assert!(!RepairState::AwaitingReview.is_actionable());
    }

    #[test]
    fn any_actionable_state_can_be_parked_for_review() {
        for state in RepairState::PROGRESSION {
            let expected = state.is_actionable();
            let allowed = validate(
                state,
                RepairState::AwaitingReview,
                TransitionReason::Review(ReviewReason::NoCandidates),
            )
            .is_ok();
            assert_eq!(allowed, expected, "review from {state}");
        }
    }

    #[test]
    fn review_must_target_awaiting_review() {
        assert!(
            validate(
                RepairState::Matched,
                RepairState::Failed,
                TransitionReason::Review(ReviewReason::NoCandidates)
            )
            .is_err()
        );
    }

    #[test]
    fn a_retry_may_only_resume_the_step_the_job_stopped_at() {
        let resume = validate_transition(
            RepairState::AwaitingReview,
            RepairState::Matched,
            TransitionReason::OperatorRetry,
            Some(RepairState::Matched),
        );
        assert!(resume.is_ok());

        // Not the recorded step: this would let review skip work.
        assert!(
            validate_transition(
                RepairState::AwaitingReview,
                RepairState::Seeding,
                TransitionReason::OperatorRetry,
                Some(RepairState::Matched),
            )
            .is_err()
        );

        // No recorded step at all: refuse rather than guess.
        assert!(
            validate_transition(
                RepairState::AwaitingReview,
                RepairState::Matched,
                TransitionReason::OperatorRetry,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn only_parked_jobs_can_be_abandoned() {
        assert!(
            validate(
                RepairState::AwaitingReview,
                RepairState::Failed,
                TransitionReason::OperatorAbandon
            )
            .is_ok()
        );
        assert!(
            validate(
                RepairState::Staged,
                RepairState::Failed,
                TransitionReason::OperatorAbandon
            )
            .is_err()
        );
    }

    #[test]
    fn parked_and_failed_jobs_can_be_restarted_but_completed_ones_cannot() {
        for from in [RepairState::AwaitingReview, RepairState::Failed] {
            assert!(
                validate(
                    from,
                    RepairState::Discovered,
                    TransitionReason::OperatorRestart
                )
                .is_ok()
            );
        }
        assert!(
            validate(
                RepairState::Completed,
                RepairState::Discovered,
                TransitionReason::OperatorRestart
            )
            .is_err()
        );
    }

    #[test]
    fn a_completed_repair_cannot_be_failed() {
        assert!(
            validate(
                RepairState::Completed,
                RepairState::Failed,
                TransitionReason::Failure
            )
            .is_err()
        );
        assert!(
            validate(
                RepairState::Seeding,
                RepairState::Failed,
                TransitionReason::Failure
            )
            .is_ok()
        );
    }

    #[test]
    fn reconciliation_only_moves_backwards_along_the_lifecycle() {
        assert!(
            validate(
                RepairState::Injected,
                RepairState::Staged,
                TransitionReason::Reconciliation
            )
            .is_ok()
        );
        assert!(
            validate(
                RepairState::Staged,
                RepairState::Injected,
                TransitionReason::Reconciliation
            )
            .is_err()
        );
        assert!(
            validate(
                RepairState::AwaitingReview,
                RepairState::Staged,
                TransitionReason::Reconciliation
            )
            .is_err()
        );
    }
}
