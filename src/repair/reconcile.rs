//! Startup reconciliation.
//!
//! The persisted state says what SeedMedic believed when it was last running.
//! Reality — the download client, the staging directory — may have moved on
//! while it was down. Before doing any new work, every unfinished job is walked
//! back to the furthest state reality actually supports.
//!
//! Only ever backwards. Reconciliation never advances a job on the strength of
//! external state, because "the torrent is in the client" does not tell us that
//! *we* put it there with the data we think we staged.

use serde_json::json;
use tracing::{info, warn};

use super::{
    domain::{RepairJob, RepairState, TransitionReason},
    ports::TransitionUpdate,
    worker::RepairDeps,
};
use crate::staging::{MaterializationPlan, PlanItem, StagingPresence};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReconcileSummary {
    pub leases_cleared: u64,
    pub jobs_examined: usize,
    pub jobs_rewound: usize,
}

/// Reconcile every unfinished job. Call once, before the worker starts.
pub async fn reconcile_on_startup(deps: &RepairDeps, owner: &str) -> ReconcileSummary {
    let mut summary = ReconcileSummary::default();

    match deps.store.clear_stale_leases(owner).await {
        Ok(cleared) => summary.leases_cleared = cleared,
        Err(error) => warn!(%error, "could not clear stale leases"),
    }

    let jobs = match deps.store.unfinished().await {
        Ok(jobs) => jobs,
        Err(error) => {
            warn!(%error, "could not list unfinished repairs; skipping reconciliation");
            return summary;
        }
    };

    summary.jobs_examined = jobs.len();
    for job in jobs {
        if reconcile_job(deps, &job).await {
            summary.jobs_rewound += 1;
        }
    }

    if summary.jobs_rewound > 0 || summary.leases_cleared > 0 {
        info!(
            leases_cleared = summary.leases_cleared,
            examined = summary.jobs_examined,
            rewound = summary.jobs_rewound,
            "startup reconciliation complete"
        );
    }
    summary
}

/// Returns whether the job was moved.
async fn reconcile_job(deps: &RepairDeps, job: &RepairJob) -> bool {
    let mut target = job.state;

    // Past injection: does the client still have it?
    if rank(job.state) >= rank(RepairState::Injected)
        && let Some(info_hash) = job.info_hash
    {
        match deps.client.status(info_hash).await {
            Ok(None) => target = earliest(target, RepairState::Staged),
            Ok(Some(_)) => {}
            // Cannot tell: leave the job where it is. The worker will find out
            // soon enough, and guessing here would be the unsafe direction.
            Err(error) => {
                warn!(job = %job.id, %error, "could not ask the download client about this repair");
            }
        }
    }

    // Past staging: is the data still on disk?
    if rank(job.state) >= rank(RepairState::Staged)
        && let Some(plan) = staging_plan(deps, job).await
    {
        match deps.staging.inspect(&plan).await {
            Ok(StagingPresence::Complete) => {}
            Ok(presence) => {
                warn!(job = %job.id, ?presence, "staged data is missing or incomplete");
                target = earliest(target, RepairState::Matched);
            }
            Err(error) => warn!(job = %job.id, %error, "could not inspect staged data"),
        }
    }

    if target == job.state {
        return false;
    }

    let Ok(transition) = job
        .plan_transition(target, TransitionReason::Reconciliation)
        .inspect_err(|error| warn!(job = %job.id, %error, "invalid reconciliation"))
    else {
        return false;
    };

    let update = TransitionUpdate::with_detail(json!({
        "reason": "external state was behind the persisted state",
        "from": job.state.as_str(),
        "to": target.as_str(),
    }));

    match deps.store.apply(job.id, transition, update).await {
        Ok(_) => {
            info!(job = %job.id, from = %job.state, to = %target, "repair rewound to match reality");
            true
        }
        Err(error) => {
            warn!(job = %job.id, %error, "could not rewind repair");
            false
        }
    }
}

async fn staging_plan(deps: &RepairDeps, job: &RepairJob) -> Option<MaterializationPlan> {
    let staging_dir = job.staging_dir.as_ref()?;
    let files = deps.store.planned_files(job.id).await.ok()?;

    let items = files
        .into_iter()
        .map(|file| PlanItem {
            // Only the destination and length matter for an inspection; the
            // source may well be gone, and that is the staging step's problem.
            source: file.source.unwrap_or_default(),
            destination: file.torrent_path.under(staging_dir),
            length: file.length,
        })
        .collect::<Vec<_>>();

    (!items.is_empty()).then_some(MaterializationPlan { items })
}

fn rank(state: RepairState) -> usize {
    state.rank().unwrap_or(usize::MAX)
}

fn earliest(left: RepairState, right: RepairState) -> RepairState {
    if rank(left) <= rank(right) {
        left
    } else {
        right
    }
}
