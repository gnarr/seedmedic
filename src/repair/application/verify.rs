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
        policy::{
            DataVerdict, ResumeDecision, assess_data, decide_resume, queued_recheck_poll_delay,
            recheck_elapsed, recheck_poll_delay,
        },
        ports::{FileCompleteness, JobPatch},
        worker::RepairDeps,
    },
    seeding::{ClientError, ClientTorrentState, FileProgress, TorrentStatus},
};

pub async fn verify(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    let status = match current_status(deps, job).await {
        Ok(Progress::Ready(status)) => status,
        Ok(Progress::NotReady(outcome)) | Err(outcome) => return outcome,
    };
    let patch = file_progress_patch(&status);

    // Only asks whether the data is sound. Whether to start seeding it is the
    // next step's question.
    match assess_data(status.completeness, job.materialization) {
        DataVerdict::CompleteAndSafe => StepOutcome::advance_with(
            json!({ "completeness": status.completeness, "state": status.state }),
            patch,
        ),
        DataVerdict::HoldForReview(reason) => StepOutcome::review_with(
            reason,
            json!({
                "completeness": status.completeness,
                "materialization": job.materialization.map(|strategy| strategy.as_str()),
                "files": status.files,
            }),
            patch,
        ),
    }
}

/// Turn what the client reported per file into the update `apply` will write
/// onto each file's existing `repair_job_files` row. `None` when the client
/// offered no breakdown, which leaves those rows exactly as they were.
///
/// `pub(super)`: `confirm` reuses it for the same reason — a client status
/// read that turns up a problem during seeding deserves the same per-file
/// detail on the review page as one found during verification.
pub(super) fn file_progress_patch(status: &TorrentStatus) -> JobPatch {
    JobPatch {
        file_progress: status.files.as_ref().map(|files| {
            files
                .iter()
                .map(|file: &FileProgress| FileCompleteness {
                    torrent_path: file.torrent_path.clone(),
                    ratio: file.completeness.ratio(),
                })
                .collect()
        }),
        ..JobPatch::default()
    }
}

pub async fn resume(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    let status = match current_status(deps, job).await {
        Ok(Progress::Ready(status)) => status,
        Ok(Progress::NotReady(outcome)) | Err(outcome) => return outcome,
    };
    let patch = file_progress_patch(&status);

    match decide_resume(
        status.completeness,
        job.materialization,
        &deps.policy,
        job.resume_approved,
    ) {
        ResumeDecision::HoldForReview(reason) => StepOutcome::review_with(
            reason,
            json!({
                "completeness": status.completeness,
                "materialization": job.materialization.map(|strategy| strategy.as_str()),
                "files": status.files,
            }),
            patch,
        ),
        ResumeDecision::Resume => {
            let info_hash = job
                .info_hash
                .expect("current_status already proved the info-hash exists");
            match deps.client.resume(info_hash).await {
                Ok(()) => StepOutcome::advance_with(
                    json!({ "resumed": true, "completeness": status.completeness }),
                    patch,
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
        Ok(Some(status)) if status.state == ClientTorrentState::Checking => {
            let elapsed = recheck_elapsed(job.rechecking_started_at, deps.clock.now());
            if elapsed >= deps.policy.recheck_timeout {
                return Err(StepOutcome::review(
                    ReviewReason::RecheckTimedOut,
                    json!({ "elapsed_seconds": elapsed.as_secs() }),
                ));
            }

            let (delay, note) = if status.queued {
                (queued_recheck_poll_delay(&deps.policy), "hash check queued")
            } else {
                (
                    recheck_poll_delay(elapsed, &deps.policy),
                    "hash check in progress",
                )
            };
            Ok(Progress::NotReady(StepOutcome::wait(delay, note)))
        }
        // An errored torrent does not recover by being asked again, and
        // nothing here may resume one — checked before any other read of the
        // status so it protects `resume` as well as `verify`.
        Ok(Some(status)) if status.state == ClientTorrentState::Errored => {
            Err(StepOutcome::review(
                ReviewReason::RecheckErrored,
                json!({ "message": status.message, "completeness": status.completeness }),
            ))
        }
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
