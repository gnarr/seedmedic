use thiserror::Error;

use crate::not_implemented::NotImplemented;

use super::{domain::TorrentMetadata, path::PathRejection};

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum InspectError {
    #[error(transparent)]
    NotImplemented(#[from] NotImplemented),
    #[error("torrent is not valid bencode: {0}")]
    Malformed(String),
    #[error("torrent declares an unsafe path: {0}")]
    UnsafePath(#[from] PathRejection),
    #[error("torrent is missing required field `{0}`")]
    MissingField(&'static str),
}

/// Turns `.torrent` bytes into the subset of metadata a repair needs.
///
/// A port rather than a plain function because the real implementation is a
/// third-party bencode decoder — an external system by the same argument as the
/// tracker or the download client — and because it lets the workflow be tested
/// end to end before that decoder exists.
///
/// Synchronous: inputs are tens of kilobytes and the work is pure CPU.
pub trait TorrentInspector: Send + Sync {
    fn inspect(&self, bytes: &[u8]) -> Result<TorrentMetadata, InspectError>;
}
