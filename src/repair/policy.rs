//! The rules that decide when automation is allowed to act.
//!
//! Pure functions over plain data, so every safety decision is a unit test away
//! from being proven. Nothing here does I/O and nothing here is configurable
//! beyond the values in [`SafetyPolicy`] — with one deliberate exception,
//! marked below, that no configuration can switch off.

use std::time::Duration;

use crate::{
    library::{MatchConfidence, MatchPlan, UnmatchedReason},
    seeding::DataCompleteness,
    staging::MaterializationStrategy,
};

use super::domain::ReviewReason;

/// When SeedMedic may start seeding without being asked.
///
/// There is no `Always`: resuming data the client has not verified is the one
/// thing that can damage a library, so the strongest setting still requires a
/// clean recheck.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoResume {
    /// Every repair waits for a human, even a perfect one.
    #[default]
    Never,
    /// Resume once the client has verified the data is complete.
    WhenVerifiedComplete,
}

/// Which ways of putting library content into staging are permitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MaterializationPolicy {
    /// Try copy-on-write first. Free, and safe because writes diverge.
    pub prefer_reflink: bool,
    /// Permit hardlinks. Free, but the staged file *is* the library file — see
    /// [`MaterializationStrategy::Hardlink`]. Off by default.
    pub allow_hardlink: bool,
    /// Permit a full copy. Costs disk, safe everywhere.
    pub allow_copy: bool,
}

impl Default for MaterializationPolicy {
    fn default() -> Self {
        Self {
            prefer_reflink: true,
            allow_hardlink: false,
            allow_copy: true,
        }
    }
}

impl MaterializationPolicy {
    /// Strategies to try, best first.
    pub fn preference(self) -> Vec<MaterializationStrategy> {
        let mut order = Vec::with_capacity(3);
        if self.prefer_reflink {
            order.push(MaterializationStrategy::Reflink);
        }
        if self.allow_copy {
            order.push(MaterializationStrategy::Copy);
        }
        // Last resort: it aliases the library, so anything else is preferable.
        if self.allow_hardlink {
            order.push(MaterializationStrategy::Hardlink);
        }
        order
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SafetyPolicy {
    pub auto_resume: AutoResume,
    /// The weakest match the workflow will act on without asking.
    pub min_match_confidence: MatchConfidence,
    /// Pieces hashed per file to confirm a match, at most. `0` disables piece
    /// verification, matching behaviour before it existed.
    pub verification_pieces: usize,
    pub materialization: MaterializationPolicy,
    /// Consecutive failures at one step before the job is parked.
    pub max_attempts: u32,
    pub retry_base_delay: Duration,
    pub retry_max_delay: Duration,
    /// How often to ask the client whether a recheck has finished. Also the
    /// floor of the adaptive backoff: [`recheck_poll_delay`] never polls
    /// faster than this.
    pub recheck_poll_interval: Duration,
    /// The cap on [`recheck_poll_delay`]'s backoff, and the interval used
    /// while a check is queued rather than running.
    pub recheck_poll_max_interval: Duration,
    /// How long a recheck may run before it is parked for review instead of
    /// polled forever. Measured from the `injected → rechecking` transition.
    pub recheck_timeout: Duration,
    /// How often to ask the tracker whether the hit-and-run is cleared.
    pub tracker_poll_interval: Duration,
}

impl Default for SafetyPolicy {
    fn default() -> Self {
        Self {
            auto_resume: AutoResume::Never,
            min_match_confidence: MatchConfidence::Probable,
            verification_pieces: 3,
            materialization: MaterializationPolicy::default(),
            max_attempts: 5,
            retry_base_delay: Duration::from_secs(30),
            retry_max_delay: Duration::from_secs(3600),
            recheck_poll_interval: Duration::from_secs(15),
            recheck_poll_max_interval: Duration::from_secs(300),
            recheck_timeout: Duration::from_secs(4 * 3600),
            tracker_poll_interval: Duration::from_secs(900),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataVerdict {
    CompleteAndSafe,
    HoldForReview(ReviewReason),
}

/// Is the staged data something we could hand back to the swarm at all?
///
/// This rule is not configurable: **incomplete data that shares inodes with the
/// media library is never let near a running torrent.** The client would treat
/// the library file as a partial download and write the missing pieces into it.
///
/// An unknown materialization is treated as the dangerous case. If we cannot
/// say how the data got there, we do not get to assume it is safe.
pub fn assess_data(
    completeness: DataCompleteness,
    materialization: Option<MaterializationStrategy>,
) -> DataVerdict {
    if completeness.is_complete() {
        return DataVerdict::CompleteAndSafe;
    }

    let aliases_library = materialization.is_none_or(MaterializationStrategy::aliases_library_file);
    DataVerdict::HoldForReview(if aliases_library {
        ReviewReason::AliasedIncompleteData
    } else {
        ReviewReason::IncompleteData
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeDecision {
    Resume,
    HoldForReview(ReviewReason),
}

/// Decide whether a verified torrent may start seeding: [`assess_data`] first,
/// then policy. Policy can only ever make the answer more conservative.
pub fn decide_resume(
    completeness: DataCompleteness,
    materialization: Option<MaterializationStrategy>,
    policy: &SafetyPolicy,
) -> ResumeDecision {
    match assess_data(completeness, materialization) {
        DataVerdict::HoldForReview(reason) => ResumeDecision::HoldForReview(reason),
        DataVerdict::CompleteAndSafe => match policy.auto_resume {
            AutoResume::Never => ResumeDecision::HoldForReview(ReviewReason::AutoResumeDisabled),
            AutoResume::WhenVerifiedComplete => ResumeDecision::Resume,
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MatchDecision {
    Accept,
    HoldForReview(ReviewReason),
}

/// Decide whether a match plan is good enough to stage.
///
/// A repair needs every file, so one unmatched entry parks the whole job, and
/// the plan is judged by its weakest file rather than its best.
pub fn decide_match(plan: &MatchPlan, policy: &SafetyPolicy) -> MatchDecision {
    if let Some(unmatched) = plan.unmatched.first() {
        return MatchDecision::HoldForReview(match unmatched.reason {
            UnmatchedReason::NoCandidate => ReviewReason::NoCandidates,
            UnmatchedReason::Ambiguous { .. } => ReviewReason::AmbiguousMatch,
        });
    }

    match plan.lowest_confidence() {
        None => MatchDecision::HoldForReview(ReviewReason::NoCandidates),
        Some(confidence) if confidence < policy.min_match_confidence => {
            MatchDecision::HoldForReview(ReviewReason::ConfidenceBelowPolicy)
        }
        Some(_) => MatchDecision::Accept,
    }
}

/// How long to wait before polling a *queued* check again. A queued check is
/// not making progress at all, so it is worth even less frequent polling than
/// a running one ever backs off to.
pub fn queued_recheck_poll_delay(policy: &SafetyPolicy) -> Duration {
    policy.recheck_poll_max_interval
}

/// How long to wait before polling a *running* check again, given how long it
/// has already run. Starts at `recheck_poll_interval` and doubles each time
/// that much time has passed again, capped at `recheck_poll_max_interval` — a
/// 40-minute check does not need a request every 15 seconds throughout, but a
/// check that finishes on the first poll should never notice the schedule.
pub fn recheck_poll_delay(elapsed: Duration, policy: &SafetyPolicy) -> Duration {
    let base = policy.recheck_poll_interval;
    let cap = policy.recheck_poll_max_interval;
    if base.is_zero() {
        return cap;
    }

    let mut delay = base;
    while delay <= elapsed && delay < cap {
        delay = delay.saturating_mul(2);
    }
    delay.min(cap)
}

/// How long a check has been running, from the `injected → rechecking`
/// transition timestamp recorded on the job. `None` (a check that somehow has
/// no recorded start) is treated as just started, never as overdue.
pub fn recheck_elapsed(
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> Duration {
    started_at
        .and_then(|start| (now - start).to_std().ok())
        .unwrap_or(Duration::ZERO)
}

/// Exponential backoff, capped. `attempts` is the number of failures so far.
pub fn retry_delay(attempts: u32, policy: &SafetyPolicy) -> Duration {
    let exponent = attempts.saturating_sub(1).min(16);
    policy
        .retry_base_delay
        .saturating_mul(1u32 << exponent)
        .min(policy.retry_max_delay)
}

/// [`retry_delay`], spread out by full jitter so jobs failing against the same
/// down tracker do not retry in lockstep and hammer it in bursts.
///
/// `jitter` is the randomness source, supplied by the caller rather than drawn
/// from a generator here, so this stays a pure function: the same inputs
/// always give the same delay. The result is uniform over `(0, computed]` —
/// never zero, because a zero delay would turn a failing job into a hot loop.
pub fn retry_delay_with_jitter(attempts: u32, policy: &SafetyPolicy, jitter: u64) -> Duration {
    let base_nanos = retry_delay(attempts, policy).as_nanos().max(1);
    let spread_nanos = (u128::from(jitter) % base_nanos) + 1;
    Duration::from_nanos(spread_nanos.min(base_nanos) as u64)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::{
        library::{CandidateOrigin, FileMatch, MatchEvidence, UnmatchedFile},
        torrent::SafeRelativePath,
    };

    const PARTIAL: DataCompleteness = DataCompleteness::Partial { ratio: 0.99 };

    fn permissive() -> SafetyPolicy {
        SafetyPolicy {
            auto_resume: AutoResume::WhenVerifiedComplete,
            ..SafetyPolicy::default()
        }
    }

    #[test]
    fn incomplete_hardlinked_data_is_never_resumed_however_permissive_the_policy() {
        let policy = SafetyPolicy {
            auto_resume: AutoResume::WhenVerifiedComplete,
            min_match_confidence: MatchConfidence::Ambiguous,
            materialization: MaterializationPolicy {
                prefer_reflink: false,
                allow_hardlink: true,
                allow_copy: true,
            },
            max_attempts: u32::MAX,
            ..SafetyPolicy::default()
        };

        assert_eq!(
            decide_resume(PARTIAL, Some(MaterializationStrategy::Hardlink), &policy),
            ResumeDecision::HoldForReview(ReviewReason::AliasedIncompleteData)
        );
    }

    #[test]
    fn unknown_materialization_is_treated_as_the_dangerous_case() {
        assert_eq!(
            decide_resume(PARTIAL, None, &permissive()),
            ResumeDecision::HoldForReview(ReviewReason::AliasedIncompleteData)
        );
    }

    #[test]
    fn incomplete_but_independent_data_is_still_not_resumed_automatically() {
        for strategy in [
            MaterializationStrategy::Reflink,
            MaterializationStrategy::Copy,
        ] {
            assert_eq!(
                decide_resume(PARTIAL, Some(strategy), &permissive()),
                ResumeDecision::HoldForReview(ReviewReason::IncompleteData),
                "{strategy:?}"
            );
        }
    }

    #[test]
    fn complete_data_resumes_only_when_policy_allows_it() {
        assert_eq!(
            decide_resume(
                DataCompleteness::Complete,
                Some(MaterializationStrategy::Reflink),
                &permissive()
            ),
            ResumeDecision::Resume
        );
        assert_eq!(
            decide_resume(
                DataCompleteness::Complete,
                Some(MaterializationStrategy::Reflink),
                &SafetyPolicy::default()
            ),
            ResumeDecision::HoldForReview(ReviewReason::AutoResumeDisabled)
        );
    }

    #[test]
    fn complete_hardlinked_data_may_resume_because_seeding_only_reads() {
        assert_eq!(
            decide_resume(
                DataCompleteness::Complete,
                Some(MaterializationStrategy::Hardlink),
                &permissive()
            ),
            ResumeDecision::Resume
        );
    }

    #[test]
    fn the_default_policy_never_resumes_by_itself() {
        assert_eq!(SafetyPolicy::default().auto_resume, AutoResume::Never);
    }

    #[test]
    fn the_default_materialization_policy_prefers_reflinks_and_refuses_hardlinks() {
        let policy = MaterializationPolicy::default();
        assert_eq!(
            policy.preference(),
            vec![
                MaterializationStrategy::Reflink,
                MaterializationStrategy::Copy
            ]
        );
    }

    #[test]
    fn hardlinks_are_always_the_last_strategy_tried() {
        let policy = MaterializationPolicy {
            prefer_reflink: true,
            allow_hardlink: true,
            allow_copy: true,
        };
        assert_eq!(
            policy.preference().last(),
            Some(&MaterializationStrategy::Hardlink)
        );
    }

    fn matched(confidence: MatchConfidence) -> FileMatch {
        FileMatch {
            torrent_path: SafeRelativePath::parse("job/e01.mkv").expect("valid"),
            length: 10,
            source: PathBuf::from("/media/e01.mkv"),
            origin: CandidateOrigin::Filesystem {
                root: PathBuf::from("/media"),
            },
            confidence,
            evidence: MatchEvidence::default(),
        }
    }

    #[test]
    fn a_plan_below_the_confidence_floor_is_parked() {
        let plan = MatchPlan {
            matched: vec![
                matched(MatchConfidence::Probable),
                matched(MatchConfidence::Ambiguous),
            ],
            unmatched: Vec::new(),
        };

        assert_eq!(
            decide_match(&plan, &SafetyPolicy::default()),
            MatchDecision::HoldForReview(ReviewReason::ConfidenceBelowPolicy)
        );
    }

    #[test]
    fn a_plan_meeting_the_floor_is_accepted() {
        let plan = MatchPlan {
            matched: vec![matched(MatchConfidence::Probable)],
            unmatched: Vec::new(),
        };

        assert_eq!(
            decide_match(&plan, &SafetyPolicy::default()),
            MatchDecision::Accept
        );
    }

    #[test]
    fn one_unmatched_file_parks_the_whole_repair() {
        let plan = MatchPlan {
            matched: vec![matched(MatchConfidence::Exact)],
            unmatched: vec![UnmatchedFile {
                torrent_path: SafeRelativePath::parse("job/e02.mkv").expect("valid"),
                length: 20,
                reason: UnmatchedReason::Ambiguous { candidates: 3 },
            }],
        };

        assert_eq!(
            decide_match(&plan, &SafetyPolicy::default()),
            MatchDecision::HoldForReview(ReviewReason::AmbiguousMatch)
        );
    }

    #[test]
    fn an_empty_plan_is_not_a_successful_match() {
        assert_eq!(
            decide_match(&MatchPlan::default(), &SafetyPolicy::default()),
            MatchDecision::HoldForReview(ReviewReason::NoCandidates)
        );
    }

    #[test]
    fn a_queued_check_is_polled_less_often_than_a_running_one() {
        let policy = SafetyPolicy {
            recheck_poll_interval: Duration::from_secs(15),
            ..SafetyPolicy::default()
        };
        assert!(queued_recheck_poll_delay(&policy) > policy.recheck_poll_interval);
    }

    #[test]
    fn a_check_that_just_started_is_polled_at_the_base_interval() {
        let policy = SafetyPolicy {
            recheck_poll_interval: Duration::from_secs(15),
            recheck_poll_max_interval: Duration::from_secs(300),
            ..SafetyPolicy::default()
        };
        assert_eq!(
            recheck_poll_delay(Duration::ZERO, &policy),
            policy.recheck_poll_interval
        );
    }

    #[test]
    fn recheck_poll_delay_backs_off_as_the_check_keeps_running_then_caps() {
        let policy = SafetyPolicy {
            recheck_poll_interval: Duration::from_secs(10),
            recheck_poll_max_interval: Duration::from_secs(60),
            ..SafetyPolicy::default()
        };

        let mut delay = recheck_poll_delay(Duration::ZERO, &policy);
        let mut elapsed = Duration::ZERO;
        let mut previous = delay;
        for _ in 0..10 {
            elapsed += delay;
            delay = recheck_poll_delay(elapsed, &policy);
            assert!(delay >= previous, "backoff must never shrink");
            assert!(
                delay <= policy.recheck_poll_max_interval,
                "backoff must respect the cap"
            );
            previous = delay;
        }
        assert_eq!(
            delay, policy.recheck_poll_max_interval,
            "a check running far longer than the cap must have reached it"
        );
    }

    #[test]
    fn recheck_poll_delay_never_goes_below_the_base_interval() {
        let policy = SafetyPolicy {
            recheck_poll_interval: Duration::from_secs(15),
            recheck_poll_max_interval: Duration::from_secs(15),
            ..SafetyPolicy::default()
        };
        assert_eq!(
            recheck_poll_delay(Duration::from_secs(600), &policy),
            Duration::from_secs(15)
        );
    }

    #[test]
    fn recheck_elapsed_is_zero_for_a_check_with_no_recorded_start() {
        assert_eq!(recheck_elapsed(None, chrono::Utc::now()), Duration::ZERO);
    }

    #[test]
    fn recheck_elapsed_measures_from_the_recorded_start() {
        let start = chrono::Utc::now();
        let now = start + chrono::Duration::seconds(90);
        assert_eq!(recheck_elapsed(Some(start), now), Duration::from_secs(90));
    }

    #[test]
    fn backoff_grows_and_then_stops_growing() {
        let policy = SafetyPolicy {
            retry_base_delay: Duration::from_secs(10),
            retry_max_delay: Duration::from_secs(100),
            ..SafetyPolicy::default()
        };

        assert_eq!(retry_delay(1, &policy), Duration::from_secs(10));
        assert_eq!(retry_delay(2, &policy), Duration::from_secs(20));
        assert_eq!(retry_delay(3, &policy), Duration::from_secs(40));
        assert_eq!(retry_delay(99, &policy), Duration::from_secs(100));
    }

    #[test]
    fn jittered_delay_is_deterministic_for_a_fixed_source() {
        let policy = SafetyPolicy::default();
        assert_eq!(
            retry_delay_with_jitter(3, &policy, 42),
            retry_delay_with_jitter(3, &policy, 42)
        );
    }

    #[test]
    fn jittered_delay_is_never_zero_across_many_seeds() {
        let policy = SafetyPolicy::default();
        for seed in 0..1000u64 {
            assert!(retry_delay_with_jitter(4, &policy, seed) > Duration::ZERO);
        }
    }

    #[test]
    fn jittered_delay_never_exceeds_the_unjittered_backoff() {
        let policy = SafetyPolicy::default();
        for seed in [0, 1, u64::MAX, u64::MAX / 2, 123_456_789] {
            assert!(retry_delay_with_jitter(2, &policy, seed) <= retry_delay(2, &policy));
        }
    }

    #[test]
    fn jittered_delay_spreads_out_across_different_sources() {
        let policy = SafetyPolicy {
            retry_base_delay: Duration::from_secs(30),
            ..SafetyPolicy::default()
        };
        let delays: std::collections::BTreeSet<_> = (0..10u64)
            .map(|seed| retry_delay_with_jitter(1, &policy, seed * 987_654_321))
            .collect();
        assert!(
            delays.len() > 1,
            "different jitter sources should usually produce different delays"
        );
    }
}
