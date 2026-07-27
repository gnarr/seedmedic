//! `Staged → Injected → Rechecking`: hand the torrent to the download client.
//!
//! Always paused. The client is not allowed to start doing anything with the
//! staged data until a hash check has run and the resume policy has agreed.

use serde_json::json;

use crate::{
    repair::{
        application::StepOutcome,
        domain::{RepairJob, RepairState, ReviewReason},
        ports::JobPatch,
        worker::RepairDeps,
    },
    seeding::{AddTorrent, ClientError},
};

pub async fn inject(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    let (Some(info_hash), Some(staging_dir)) = (job.info_hash, job.staging_dir.as_ref()) else {
        return StepOutcome::review(
            ReviewReason::TorrentUnreadable,
            json!({ "error": "job reached injection without an info-hash or staging directory" }),
        );
    };

    let torrent_file = match deps.store.torrent_file(job.id).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return StepOutcome::review(
                ReviewReason::TorrentUnreadable,
                json!({ "error": "the stored .torrent is missing" }),
            );
        }
        Err(error) => return StepOutcome::retry(error),
    };

    let save_path = deps.staging.save_path(staging_dir);
    let request = AddTorrent {
        info_hash,
        torrent_file: &torrent_file,
        save_path: &save_path,
        category: deps.category.as_deref(),
    };

    match deps.client.add_paused(request).await {
        // Adding an already-present torrent is a no-op per the port contract,
        // so a replay after a crash lands here too.
        Ok(()) => StepOutcome::advance_with(
            json!({ "save_path": save_path, "info_hash": info_hash.to_hex() }),
            JobPatch::default(),
        ),
        Err(ClientError::NotImplemented(details)) => StepOutcome::review(
            ReviewReason::AdapterNotImplemented,
            json!({ "adapter": details.adapter, "todo": details.todo }),
        ),
        Err(error) => StepOutcome::retry(error),
    }
}

pub async fn start_recheck(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    let Some(info_hash) = job.info_hash else {
        return StepOutcome::review(
            ReviewReason::TorrentUnreadable,
            json!({ "error": "job reached rechecking without an info-hash" }),
        );
    };

    // Confirm the torrent is still there before asking for a hash check, so a
    // torrent removed since injection rewinds instead of failing repeatedly.
    match deps.client.status(info_hash).await {
        Ok(None) => {
            return StepOutcome::rewind(
                RepairState::Staged,
                "the download client no longer has this torrent",
            );
        }
        Ok(Some(_)) => {}
        Err(ClientError::NotImplemented(details)) => {
            return StepOutcome::review(
                ReviewReason::AdapterNotImplemented,
                json!({ "adapter": details.adapter, "todo": details.todo }),
            );
        }
        Err(error) => return StepOutcome::retry(error),
    }

    match deps.client.recheck(info_hash).await {
        Ok(()) => StepOutcome::advance(),
        Err(ClientError::NotImplemented(details)) => StepOutcome::review(
            ReviewReason::AdapterNotImplemented,
            json!({ "adapter": details.adapter, "todo": details.todo }),
        ),
        Err(error) => StepOutcome::retry(error),
    }
}
