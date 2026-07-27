use async_trait::async_trait;
use thiserror::Error;

use crate::not_implemented::NotImplemented;

use super::domain::{HitAndRun, HitAndRunStatus, TrackerId, TrackerTorrentId};

#[derive(Clone, Debug, Error)]
pub enum TrackerError {
    #[error(transparent)]
    NotImplemented(#[from] NotImplemented),
    #[error("tracker request failed: {0}")]
    Transport(String),
    #[error("tracker returned data we cannot interpret: {0}")]
    Protocol(String),
    #[error("tracker rate limited us; retry in {retry_after_seconds}s")]
    RateLimited { retry_after_seconds: u64 },
    #[error("tracker rejected our credentials")]
    Unauthorized,
    #[error("torrent {0} is not on the tracker")]
    NotFound(TrackerTorrentId),
}

impl TrackerError {
    /// Transient failures are retried with backoff; the rest park the job for
    /// review rather than burning the retry budget on something that will not
    /// fix itself.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Transport(_) | Self::RateLimited { .. } | Self::Protocol(_)
        )
    }
}

/// One configured private tracker.
///
/// Three methods because the workflow needs exactly three things from a
/// tracker: find the warnings, get the torrent, and ask whether a warning is
/// gone. Anything a specific tracker family offers beyond that stays inside its
/// adapter until a use case needs it.
#[async_trait]
pub trait TrackerClient: Send + Sync {
    fn id(&self) -> &TrackerId;

    /// Every hit-and-run currently outstanding for the configured account.
    async fn list_hit_and_runs(&self) -> Result<Vec<HitAndRun>, TrackerError>;

    /// The raw `.torrent`. Returned as bytes, not parsed: decoding belongs to
    /// [`crate::torrent::TorrentInspector`], and the bytes are what we persist.
    async fn fetch_torrent_file(&self, id: &TrackerTorrentId) -> Result<Vec<u8>, TrackerError>;

    async fn hit_and_run_status(
        &self,
        id: &TrackerTorrentId,
    ) -> Result<HitAndRunStatus, TrackerError>;
}
