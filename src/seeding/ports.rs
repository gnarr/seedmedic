use async_trait::async_trait;
use thiserror::Error;

use crate::{not_implemented::NotImplemented, torrent::InfoHash};

use super::domain::{AddTorrent, TorrentStatus};

#[derive(Clone, Debug, Error)]
pub enum ClientError {
    #[error(transparent)]
    NotImplemented(#[from] NotImplemented),
    #[error("download client request failed: {0}")]
    Transport(String),
    #[error("download client returned data we cannot interpret: {0}")]
    Protocol(String),
    #[error("download client rejected our credentials")]
    Unauthorized,
    #[error("download client rejected the torrent: {0}")]
    Rejected(String),
}

impl ClientError {
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transport(_) | Self::Protocol(_))
    }
}

/// A cheap reachability check for the diagnostics page: not tied to any one
/// torrent, so it doubles as "is the client even there at all".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClientSummary {
    pub torrent_count: usize,
}

/// The BitTorrent client that will do the seeding.
///
/// Named for the capability rather than for qBittorrent, but shaped by it: this
/// is the set of operations a repair performs, no more.
///
/// All methods must be idempotent. The workflow may call any of them again
/// after a crash, and "already in that state" is success, not an error.
#[async_trait]
pub trait TorrentClient: Send + Sync {
    /// Add the torrent in a paused state. Adding one that is already present is
    /// a no-op, not a failure.
    async fn add_paused(&self, request: AddTorrent<'_>) -> Result<(), ClientError>;

    /// `None` when the client has never heard of this torrent — which, after we
    /// believe we added it, means somebody removed it and the repair must go
    /// back a step.
    async fn status(&self, info_hash: InfoHash) -> Result<Option<TorrentStatus>, ClientError>;

    /// Force a hash check of the data on disk. This is what turns "the files
    /// look right" into "the client agrees the files are right".
    async fn recheck(&self, info_hash: InfoHash) -> Result<(), ClientError>;

    async fn resume(&self, info_hash: InfoHash) -> Result<(), ClientError>;

    /// Remove the torrent. `delete_files` is never set for a repair whose data
    /// might be hardlinked into the library.
    async fn remove(&self, info_hash: InfoHash, delete_files: bool) -> Result<(), ClientError>;

    /// For the diagnostics page: proves the client is reachable at all, not
    /// just that one known torrent is.
    async fn summary(&self) -> Result<ClientSummary, ClientError>;
}
