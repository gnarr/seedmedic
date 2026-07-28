use std::time::Duration;

use crate::repair::JobId;

/// The short, deliberately short, list of events an operator wants to hear
/// about without watching logs.
#[derive(Clone, Debug, PartialEq)]
pub enum NotificationEvent {
    ParkedForReview {
        job: JobId,
        tracker: String,
        torrent_name: String,
        reason: &'static str,
    },
    Completed {
        job: JobId,
        tracker: String,
        torrent_name: String,
    },
    TrackerUnreachable {
        tracker: String,
        unreachable_for: Duration,
    },
}

impl NotificationEvent {
    /// A flat JSON body any generic webhook receiver — Apprise or otherwise
    /// — can consume without knowing SeedMedic's domain types.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::ParkedForReview {
                job,
                tracker,
                torrent_name,
                reason,
            } => serde_json::json!({
                "event": "parked_for_review",
                "job": job.0,
                "tracker": tracker,
                "torrent_name": torrent_name,
                "reason": reason,
            }),
            Self::Completed {
                job,
                tracker,
                torrent_name,
            } => serde_json::json!({
                "event": "completed",
                "job": job.0,
                "tracker": tracker,
                "torrent_name": torrent_name,
            }),
            Self::TrackerUnreachable {
                tracker,
                unreachable_for,
            } => serde_json::json!({
                "event": "tracker_unreachable",
                "tracker": tracker,
                "unreachable_for_seconds": unreachable_for.as_secs(),
            }),
        }
    }
}
