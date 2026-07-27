//! `Rechecking → Verified → Seeding`: the two steps that guard the library.
//!
//! Both ask the client what it actually found on disk and hand the answer to
//! [`decide_resume`], which is where the "never resume incomplete hardlinked
//! data" rule lives. The status is re-read immediately before resuming rather
//! than trusted from the verify step — the gap between them is exactly where
//! something could have changed.

use serde_json::json;

use crate::{
    repair::{
        application::StepOutcome,
        domain::{RepairJob, RepairState, ReviewReason},
        policy::{DataVerdict, ResumeDecision, assess_data, decide_resume},
        ports::JobPatch,
        worker::RepairDeps,
    },
    seeding::{ClientError, ClientTorrentState, TorrentStatus},
};

pub async fn verify(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    let status = match current_status(deps, job).await {
        Ok(Progress::Ready(status)) => status,
        Ok(Progress::NotReady(outcome)) | Err(outcome) => return outcome,
    };

    // Only asks whether the data is sound. Whether to start seeding it is the
    // next step's question.
    match assess_data(status.completeness, job.materialization) {
        DataVerdict::CompleteAndSafe => StepOutcome::advance_with(
            json!({ "completeness": status.completeness, "state": status.state }),
            JobPatch::default(),
        ),
        DataVerdict::HoldForReview(reason) => StepOutcome::review(
            reason,
            json!({
                "completeness": status.completeness,
                "materialization": job.materialization.map(|strategy| strategy.as_str()),
            }),
        ),
    }
}

pub async fn resume(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    let status = match current_status(deps, job).await {
        Ok(Progress::Ready(status)) => status,
        Ok(Progress::NotReady(outcome)) | Err(outcome) => return outcome,
    };

    match decide_resume(status.completeness, job.materialization, &deps.policy) {
        ResumeDecision::HoldForReview(reason) => StepOutcome::review(
            reason,
            json!({
                "completeness": status.completeness,
                "materialization": job.materialization.map(|strategy| strategy.as_str()),
            }),
        ),
        ResumeDecision::Resume => {
            let info_hash = job
                .info_hash
                .expect("current_status already proved the info-hash exists");
            match deps.client.resume(info_hash).await {
                Ok(()) => StepOutcome::advance_with(
                    json!({ "resumed": true, "completeness": status.completeness }),
                    JobPatch::default(),
                ),
                Err(ClientError::NotImplemented(details)) => StepOutcome::review(
                    ReviewReason::AdapterNotImplemented,
                    json!({ "adapter": details.adapter, "todo": details.todo }),
                ),
                Err(error) => StepOutcome::retry(error),
            }
        }
    }
}

enum Progress {
    Ready(TorrentStatus),
    NotReady(StepOutcome),
}

/// Fetch the client's view, turning "still checking" and "gone" into outcomes
/// so both callers handle them identically.
async fn current_status(deps: &RepairDeps, job: &RepairJob) -> Result<Progress, StepOutcome> {
    let Some(info_hash) = job.info_hash else {
        return Err(StepOutcome::review(
            ReviewReason::TorrentUnreadable,
            json!({ "error": "job reached verification without an info-hash" }),
        ));
    };

    match deps.client.status(info_hash).await {
        Ok(Some(status)) if status.state == ClientTorrentState::Checking => Ok(Progress::NotReady(
            StepOutcome::wait(deps.policy.recheck_poll_interval, "hash check in progress"),
        )),
        Ok(Some(status)) => Ok(Progress::Ready(status)),
        // The torrent is not in the client any more — removed by hand, or lost
        // when the client's state was reset. The staged files are still ours,
        // so go back to injection rather than burning retries on a torrent
        // that is not coming back on its own.
        Ok(None) => Err(StepOutcome::rewind(
            RepairState::Staged,
            "the download client no longer has this torrent",
        )),
        Err(ClientError::NotImplemented(details)) => Err(StepOutcome::review(
            ReviewReason::AdapterNotImplemented,
            json!({ "adapter": details.adapter, "todo": details.todo }),
        )),
        Err(error) => Err(StepOutcome::retry(error)),
    }
}
