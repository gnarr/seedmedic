//! `Seeding → Completed`: ask the tracker whether the hit-and-run is gone.
//!
//! A torrent seeding happily in the client proves nothing. The tracker decides
//! when a hit-and-run is cleared, and until it says so this job stays in
//! `Seeding` — visibly unfinished, being polled.

use serde_json::json;

use crate::{
    repair::{
        application::StepOutcome,
        domain::{RepairJob, ReviewReason},
        worker::RepairDeps,
    },
    tracker::{HitAndRunStatus, TrackerError},
};

pub async fn confirm_with_tracker(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    let Some(tracker) = deps.trackers.get(&job.tracker) else {
        return StepOutcome::review(
            ReviewReason::TrackerStatusUnclear,
            json!({ "error": format!("tracker `{}` is not configured", job.tracker) }),
        );
    };

    match tracker.hit_and_run_status(&job.torrent_id).await {
        Ok(HitAndRunStatus::Cleared) => StepOutcome::advance(),
        Ok(HitAndRunStatus::Active) => StepOutcome::wait(
            deps.policy.tracker_poll_interval,
            "tracker still shows the hit-and-run as outstanding",
        ),
        // Not an advance and not a failure: we simply do not know. Keep
        // seeding and keep asking.
        Ok(HitAndRunStatus::Unknown) => StepOutcome::wait(
            deps.policy.tracker_poll_interval,
            "tracker's answer could not be interpreted",
        ),
        Err(TrackerError::NotImplemented(details)) => StepOutcome::review(
            ReviewReason::AdapterNotImplemented,
            json!({ "adapter": details.adapter, "todo": details.todo }),
        ),
        Err(error) => StepOutcome::retry(error),
    }
}
