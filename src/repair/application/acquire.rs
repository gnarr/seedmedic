//! `Discovered → TorrentFetched`: get the `.torrent` and read it.
//!
//! The bytes are stored on the job in the same transaction as the transition,
//! so the tracker is never asked for the same torrent twice unless the download
//! itself failed.

use serde_json::json;

use crate::{
    repair::{
        application::StepOutcome,
        domain::{RepairJob, ReviewReason},
        ports::{JobPatch, PlannedFile},
        worker::RepairDeps,
    },
    torrent::InspectError,
    tracker::TrackerError,
};

pub async fn fetch_torrent(deps: &RepairDeps, job: &RepairJob) -> StepOutcome {
    let Some(tracker) = deps.trackers.get(&job.tracker) else {
        return StepOutcome::review(
            ReviewReason::TrackerStatusUnclear,
            json!({ "error": format!("tracker `{}` is not configured", job.tracker) }),
        );
    };

    let bytes = match tracker.fetch_torrent_file(&job.torrent_id).await {
        Ok(bytes) => bytes,
        Err(TrackerError::NotImplemented(details)) => {
            return StepOutcome::review(
                ReviewReason::AdapterNotImplemented,
                json!({ "adapter": details.adapter, "todo": details.todo }),
            );
        }
        Err(error) => return StepOutcome::retry(error),
    };

    let metadata = match deps.inspector.inspect(&bytes) {
        Ok(metadata) => metadata,
        Err(InspectError::NotImplemented(details)) => {
            return StepOutcome::review(
                ReviewReason::AdapterNotImplemented,
                json!({ "adapter": details.adapter, "todo": details.todo }),
            );
        }
        Err(error @ InspectError::UnsafePath(_)) => {
            return StepOutcome::review(
                ReviewReason::UnsafeTorrentPaths,
                json!({ "error": error.to_string() }),
            );
        }
        Err(error) => {
            return StepOutcome::review(
                ReviewReason::TorrentUnreadable,
                json!({ "error": error.to_string() }),
            );
        }
    };

    // The tracker told us one info-hash and served a torrent with another.
    // Something is wrong on their side or ours; either way, do not guess.
    if let Some(expected) = job.info_hash
        && expected != metadata.info_hash
    {
        return StepOutcome::review(
            ReviewReason::InfoHashMismatch,
            json!({ "tracker": expected.to_hex(), "torrent": metadata.info_hash.to_hex() }),
        );
    }

    let files = metadata
        .files
        .iter()
        .map(|file| PlannedFile {
            torrent_path: file.path.clone(),
            length: file.length,
            source: None,
            confidence: None,
            evidence: None,
            materialized_as: None,
            recheck_progress: None,
        })
        .collect();

    StepOutcome::advance_with(
        json!({
            "info_hash": metadata.info_hash.to_hex(),
            "files": metadata.file_count(),
            "total_bytes": metadata.total_length(),
        }),
        JobPatch {
            info_hash: Some(metadata.info_hash),
            torrent_file: Some(bytes),
            total_bytes: Some(metadata.total_length()),
            files: Some(files),
            ..JobPatch::default()
        },
    )
}
