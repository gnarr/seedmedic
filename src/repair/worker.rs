//! The coordinator: claim work, run one step, record the result, repeat.
//!
//! The worker holds no state of its own. Everything it needs to decide what to
//! do next is in the database, which is what makes killing it at any moment
//! safe — and what makes a second one, or a restarted one, pick up cleanly.

use std::{collections::HashMap, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use tracing::{error, info, warn};

use crate::{
    clock::Clock,
    library::CandidateSource,
    seeding::TorrentClient,
    staging::StagingFilesystem,
    torrent::TorrentInspector,
    tracker::{TrackerClient, TrackerId},
};

use super::{
    application::{StepOutcome, discover_hit_and_runs, step},
    domain::{JobId, RepairJob, RepairState, ReviewReason, TransitionReason},
    policy::{SafetyPolicy, retry_delay},
    ports::{Applied, RepairStore, StoreError, TransitionUpdate},
};

/// Everything the workflow needs from the outside world, in one place.
///
/// Assembled once at startup. Steps borrow it; nothing mutates it.
pub struct RepairDeps {
    pub store: Arc<dyn RepairStore>,
    pub trackers: HashMap<TrackerId, Arc<dyn TrackerClient>>,
    pub inspector: Arc<dyn TorrentInspector>,
    pub candidate_sources: Vec<Arc<dyn CandidateSource>>,
    pub staging: Arc<dyn StagingFilesystem>,
    pub client: Arc<dyn TorrentClient>,
    pub clock: Arc<dyn Clock>,
    pub policy: SafetyPolicy,
    /// Category to file repaired torrents under in the download client.
    pub category: Option<String>,
}

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// Identifies this instance's leases. On startup, leases with this owner
    /// are assumed to belong to the process that just died.
    pub owner: String,
    pub lease: Duration,
    pub batch_size: i64,
    pub poll_interval: Duration,
    pub discovery_interval: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            owner: "seedmedic".to_owned(),
            lease: Duration::from_secs(300),
            batch_size: 4,
            poll_interval: Duration::from_secs(10),
            discovery_interval: Duration::from_secs(900),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TickSummary {
    pub claimed: usize,
    pub advanced: usize,
    pub parked: usize,
    pub waiting: usize,
    pub retrying: usize,
    pub rewound: usize,
}

pub struct RepairWorker {
    deps: Arc<RepairDeps>,
    config: WorkerConfig,
}

impl RepairWorker {
    pub fn new(deps: Arc<RepairDeps>, config: WorkerConfig) -> Self {
        Self { deps, config }
    }

    pub fn deps(&self) -> &Arc<RepairDeps> {
        &self.deps
    }

    /// Claim whatever is due and drive each job as far as it will go.
    pub async fn tick(&self) -> TickSummary {
        let mut summary = TickSummary::default();

        let claimed = match self
            .deps
            .store
            .claim(
                &self.config.owner,
                self.config.lease,
                self.config.batch_size,
            )
            .await
        {
            Ok(jobs) => jobs,
            Err(error) => {
                error!(%error, "could not claim repair jobs");
                return summary;
            }
        };

        summary.claimed = claimed.len();
        for job in claimed {
            self.drive(job, &mut summary).await;
        }
        summary
    }

    /// Poll the trackers for new hit-and-runs.
    pub async fn discover(&self) {
        let summary = discover_hit_and_runs(&self.deps).await;
        if summary.jobs_created > 0 || summary.trackers_failed > 0 {
            info!(
                warnings = summary.warnings_seen,
                new_jobs = summary.jobs_created,
                trackers_failed = summary.trackers_failed,
                "tracker poll complete"
            );
        }
    }

    /// Run until `shutdown` resolves.
    pub async fn run(self, shutdown: impl Future<Output = ()> + Send) {
        let mut work = tokio::time::interval(self.config.poll_interval);
        let mut discovery = tokio::time::interval(self.config.discovery_interval);
        work.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        discovery.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let shutdown = std::pin::pin!(shutdown);
        let mut shutdown = shutdown;

        loop {
            tokio::select! {
                () = &mut shutdown => break,
                _ = work.tick() => { self.tick().await; }
                _ = discovery.tick() => self.discover().await,
            }
        }

        info!("repair worker stopped");
    }

    /// Step one job repeatedly while it keeps advancing.
    ///
    /// Bounded so neither a rewind-then-advance cycle nor a step that somehow
    /// advances in a circle can spin: twice the lifecycle is enough for one
    /// rewind and a full recovery, and anything beyond that waits for the next
    /// tick.
    async fn drive(&self, job: RepairJob, summary: &mut TickSummary) {
        let id = job.id;
        let mut job = job;

        // `None` means the lease turned out to belong to someone else
        // mid-drive; releasing it then would steal it back.
        let stop: Option<Stop> = 'drive: {
            for _ in 0..RepairState::PROGRESSION.len() * 2 {
                match step(&self.deps, &job).await {
                    StepOutcome::Advance { detail, patch } => {
                        let transition = match job.advance() {
                            Ok(transition) => transition,
                            Err(error) => {
                                error!(job = %id, %error, "step advanced from a state that cannot advance");
                                break 'drive Some(Stop::idle());
                            }
                        };

                        let update = TransitionUpdate {
                            detail,
                            failure_reason: None,
                            patch,
                        };
                        match self.deps.store.apply(id, transition, update).await {
                            Ok(Applied::Applied) => {
                                summary.advanced += 1;
                                info!(
                                    job = %id,
                                    from = %transition.from(),
                                    to = %transition.to(),
                                    "repair advanced"
                                );
                            }
                            Ok(Applied::AlreadyInTargetState) => info!(
                                job = %id,
                                state = %transition.to(),
                                "step replayed; job was already there"
                            ),
                            Err(StoreError::Conflict { actual, .. }) => {
                                // Somebody else moved it. Theirs is the current
                                // truth; drop the job and re-read next tick.
                                warn!(job = %id, %actual, "repair changed underneath us");
                                break 'drive Some(Stop::idle());
                            }
                            Err(error) => {
                                error!(job = %id, %error, "could not record transition");
                                break 'drive Some(self.stop_for_retry(&job));
                            }
                        }

                        // The step that just finished may have taken a while;
                        // renew now so a long one never outlives its lease.
                        if !self.renew_lease(id).await {
                            break 'drive None;
                        }

                        match self.reload(id).await {
                            Some(next) if next.state.is_actionable() => job = next,
                            Some(_) | None => break 'drive Some(Stop::idle()),
                        }
                    }

                    StepOutcome::Rewind { to, note } => {
                        summary.rewound += 1;
                        warn!(job = %id, from = %job.state, %to, note, "rewinding repair to match reality");

                        let transition =
                            match job.plan_transition(to, TransitionReason::Reconciliation) {
                                Ok(transition) => transition,
                                Err(error) => {
                                    error!(job = %id, %error, "step asked for an illegal rewind");
                                    break 'drive Some(Stop::idle());
                                }
                            };
                        let update = TransitionUpdate::with_detail(
                            serde_json::json!({ "note": note, "from": job.state.as_str() }),
                        );
                        if let Err(error) = self.deps.store.apply(id, transition, update).await {
                            error!(job = %id, %error, "could not rewind repair");
                            break 'drive Some(Stop::idle());
                        }

                        match self.reload(id).await {
                            Some(next) => job = next,
                            None => break 'drive Some(Stop::idle()),
                        }
                    }

                    StepOutcome::Review { reason, detail } => {
                        summary.parked += 1;
                        self.park(&job, reason, detail).await;
                        break 'drive Some(Stop::idle());
                    }

                    StepOutcome::Wait { after, note } => {
                        summary.waiting += 1;
                        info!(job = %id, state = %job.state, note, "waiting");
                        break 'drive Some(Stop {
                            retry_at: Some(self.deps.clock.now() + chrono_duration(after)),
                            count_attempt: false,
                        });
                    }

                    StepOutcome::Retry { error } => {
                        summary.retrying += 1;
                        let attempts = job.attempts + 1;
                        if attempts >= self.deps.policy.max_attempts {
                            warn!(job = %id, state = %job.state, attempts, error, "retry budget exhausted");
                            self.park(
                                &job,
                                ReviewReason::RetryBudgetExhausted,
                                Some(serde_json::json!({ "attempts": attempts, "error": error })),
                            )
                            .await;
                            break 'drive Some(Stop::idle());
                        }

                        warn!(job = %id, state = %job.state, attempts, error, "step failed; will retry");
                        break 'drive Some(Stop {
                            retry_at: Some(
                                self.deps.clock.now()
                                    + chrono_duration(retry_delay(attempts, &self.deps.policy)),
                            ),
                            count_attempt: true,
                        });
                    }
                }
            }

            Some(Stop::idle())
        };

        match stop {
            Some(stop) => self.release(id, stop).await,
            None => {
                warn!(job = %id, "lease was renewed by someone else; abandoning this job for this tick")
            }
        }
    }

    /// Renew the lease this worker holds on `id`. Returns whether it still
    /// does: a renewal that touches no rows means another worker's claim has
    /// already superseded ours, and this tick must stop touching the job.
    async fn renew_lease(&self, id: JobId) -> bool {
        match self
            .deps
            .store
            .renew_lease(id, &self.config.owner, self.config.lease)
            .await
        {
            Ok(true) => true,
            Ok(false) => {
                warn!(job = %id, "lease renewal affected no rows; another worker now owns this job");
                false
            }
            Err(error) => {
                // Unknown, not lost: apply's compare-and-swap still protects
                // correctness if a race actually happened.
                warn!(job = %id, %error, "could not renew repair lease");
                true
            }
        }
    }

    async fn park(&self, job: &RepairJob, reason: ReviewReason, detail: Option<serde_json::Value>) {
        let transition = match job.plan_transition(
            RepairState::AwaitingReview,
            TransitionReason::Review(reason),
        ) {
            Ok(transition) => transition,
            Err(error) => {
                error!(job = %job.id, %error, "cannot park this job for review");
                return;
            }
        };

        let update = TransitionUpdate {
            detail,
            failure_reason: None,
            patch: super::ports::JobPatch::default(),
        };
        if let Err(error) = self.deps.store.apply(job.id, transition, update).await {
            error!(job = %job.id, %error, "could not park job for review");
        } else {
            info!(job = %job.id, from = %job.state, reason = reason.as_str(), "repair parked for review");
        }
    }

    async fn reload(&self, id: JobId) -> Option<RepairJob> {
        match self.deps.store.job(id).await {
            Ok(job) => job,
            Err(error) => {
                error!(job = %id, %error, "could not re-read repair job");
                None
            }
        }
    }

    fn stop_for_retry(&self, job: &RepairJob) -> Stop {
        Stop {
            retry_at: Some(
                self.deps.clock.now()
                    + chrono_duration(retry_delay(job.attempts + 1, &self.deps.policy)),
            ),
            count_attempt: true,
        }
    }

    async fn release(&self, id: JobId, stop: Stop) {
        if let Err(error) = self
            .deps
            .store
            .release(id, stop.retry_at, stop.count_attempt)
            .await
        {
            // The lease will expire on its own, so this is loud but not fatal.
            error!(job = %id, %error, "could not release repair job lease");
        }
    }
}

/// How to leave a job when the worker stops touching it.
struct Stop {
    retry_at: Option<DateTime<Utc>>,
    count_attempt: bool,
}

impl Stop {
    /// Release the lease and leave the job due immediately.
    fn idle() -> Self {
        Self {
            retry_at: None,
            count_attempt: false,
        }
    }
}

fn chrono_duration(duration: Duration) -> chrono::Duration {
    chrono::Duration::from_std(duration).unwrap_or_else(|_| chrono::Duration::seconds(60))
}
