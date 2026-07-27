//! `Seeding → Completed`: ask the tracker whether the hit-and-run is gone.
//!
//! A torrent seeding happily in the client proves nothing on its own — the
//! tracker decides when a hit-and-run is cleared, and until it says so this
//! job stays in `Seeding`, being polled. But a repair can sit here for days,
//! and in that window the client is the only thing that would notice if the
//! torrent stopped seeding, so every poll checks both.

use serde_json::json;

use crate::{
    repair::{
        application::{StepOutcome, verify::file_progress_patch},
        domain::{RepairJob, RepairState, ReviewReason},
        policy::{ResumeDecision, decide_resume},
        worker::RepairDeps,
    },
    seeding::{ClientError, ClientTorrentState, TorrentStatus},
    torrent::InfoHash,
    tracker::{HitAndRunStatus, TrackerError},
};

pub async fn confirm_with_tracker(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    let Some(tracker) = deps.trackers.get(&job.tracker) else {
        return StepOutcome::review(
            ReviewReason::TrackerStatusUnclear,
            json!({ "error": format!("tracker `{}` is not configured", job.tracker) }),
        );
    };

    let tracker_note = match tracker.hit_and_run_status(&job.torrent_id).await {
        Ok(HitAndRunStatus::Cleared) => return StepOutcome::advance(),
        Ok(HitAndRunStatus::Active) => "tracker still shows the hit-and-run as outstanding",
        // Not an advance and not a failure: we simply do not know. Keep
        // seeding and keep asking.
        Ok(HitAndRunStatus::Unknown) => "tracker's answer could not be interpreted",
        Err(TrackerError::NotImplemented(details)) => {
            return StepOutcome::review(
                ReviewReason::AdapterNotImplemented,
                json!({ "adapter": details.adapter, "todo": details.todo }),
            );
        }
        Err(error) => return StepOutcome::retry(error),
    };

    let Some(info_hash) = job.info_hash else {
        return StepOutcome::review(
            ReviewReason::TorrentUnreadable,
            json!({ "error": "job reached seeding confirmation without an info-hash" }),
        );
    };

    match check_client(deps, job, info_hash).await {
        ClientCheck::Exit(outcome) => outcome,
        ClientCheck::Healthy(_status) => {
            StepOutcome::wait(deps.policy.tracker_poll_interval, tracker_note)
        }
    }
}

/// What the client had to say, boiled down to "keep waiting" or "stop here".
enum ClientCheck {
    Healthy(TorrentStatus),
    Exit(StepOutcome),
}

/// The other half of a `Seeding`-state poll: is the torrent still doing what
/// we left it doing?
///
/// `Checking` is treated the same as `Seeding` here — nothing in this
/// lifecycle re-triggers a check once a torrent is seeding, so if one is
/// running it is outside our control, and it will resolve to `Paused` or
/// `Errored` on its own, at which point this function sees it.
async fn check_client(deps: &RepairDeps, job: &RepairJob, info_hash: InfoHash) -> ClientCheck {
    match deps.client.status(info_hash).await {
        Ok(Some(status))
            if matches!(
                status.state,
                ClientTorrentState::Seeding | ClientTorrentState::Checking
            ) =>
        {
            ClientCheck::Healthy(status)
        }
        // Somebody paused it, or a restart did. Never resume without asking
        // the same gate that governed the original resume.
        Ok(Some(status)) if status.state == ClientTorrentState::Paused => {
            match decide_resume(status.completeness, job.materialization, &deps.policy) {
                ResumeDecision::HoldForReview(reason) => {
                    ClientCheck::Exit(StepOutcome::review_with(
                        reason,
                        json!({
                            "completeness": status.completeness,
                            "materialization": job.materialization.map(|strategy| strategy.as_str()),
                            "files": status.files,
                        }),
                        file_progress_patch(&status),
                    ))
                }
                ResumeDecision::Resume => match deps.client.resume(info_hash).await {
                    Ok(()) => ClientCheck::Healthy(status),
                    Err(ClientError::NotImplemented(details)) => {
                        ClientCheck::Exit(StepOutcome::review(
                            ReviewReason::AdapterNotImplemented,
                            json!({ "adapter": details.adapter, "todo": details.todo }),
                        ))
                    }
                    Err(error) => ClientCheck::Exit(StepOutcome::retry(error)),
                },
            }
        }
        // The client is fetching data for a torrent we believed was complete
        // and seeding. The staged data was not actually complete, and it may
        // be hardlinked into the library — this is the case that most needs a
        // human, and never resolves itself by waiting.
        Ok(Some(status)) if status.state == ClientTorrentState::Downloading => {
            ClientCheck::Exit(StepOutcome::review_with(
                ReviewReason::DownloadingDuringSeeding,
                json!({ "completeness": status.completeness, "files": status.files }),
                file_progress_patch(&status),
            ))
        }
        // Errored, or gone entirely: the staged files are still ours, so go
        // back to injection rather than parking on a torrent that will not
        // recover on its own — the same recovery the verification steps use
        // for a vanished torrent.
        Ok(Some(_)) => ClientCheck::Exit(StepOutcome::rewind(
            RepairState::Staged,
            "the download client reported this torrent as errored while seeding",
        )),
        Ok(None) => ClientCheck::Exit(StepOutcome::rewind(
            RepairState::Staged,
            "the download client no longer has this torrent",
        )),
        Err(ClientError::NotImplemented(details)) => ClientCheck::Exit(StepOutcome::review(
            ReviewReason::AdapterNotImplemented,
            json!({ "adapter": details.adapter, "todo": details.todo }),
        )),
        Err(error) => ClientCheck::Exit(StepOutcome::retry(error)),
    }
}
