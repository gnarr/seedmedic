use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::torrent::{InfoHash, SafeRelativePath};

/// What the download client says a torrent is doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientTorrentState {
    Paused,
    Checking,
    /// Actively downloading, which for a repair means the data was not
    /// complete and the client is now fetching the rest from peers.
    Downloading,
    Seeding,
    Errored,
}

/// How much of the torrent the client can account for on disk.
///
/// Not `f64` on its own: the difference between "all of it" and "99.9% of it"
/// is the difference between a safe resume and writing into the user's library.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "completeness")]
pub enum DataCompleteness {
    Complete,
    Partial { ratio: f64 },
}

impl DataCompleteness {
    /// Anything short of a full 1.0 is partial. Deliberately not rounded.
    pub fn from_ratio(ratio: f64) -> Self {
        if ratio >= 1.0 {
            Self::Complete
        } else {
            Self::Partial {
                ratio: ratio.max(0.0),
            }
        }
    }

    pub fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }

    /// The fraction complete, as a plain number. `1.0` for [`Self::Complete`],
    /// so callers that just want "how much of this file" do not have to match.
    pub fn ratio(self) -> f64 {
        match self {
            Self::Complete => 1.0,
            Self::Partial { ratio } => ratio,
        }
    }
}

/// How much of one file within a torrent the client can account for.
///
/// Turns a single torrent-wide ratio into the detail an operator actually
/// needs: which file is the mismatch, not just that one exists.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FileProgress {
    pub torrent_path: SafeRelativePath,
    pub completeness: DataCompleteness,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TorrentStatus {
    pub state: ClientTorrentState,
    pub completeness: DataCompleteness,
    pub save_path: PathBuf,
    /// `None` when the client did not offer a breakdown, or a check has not
    /// run yet. The resume policy never depends on this: absent per-file data
    /// must leave the gate exactly as conservative as it already is.
    pub files: Option<Vec<FileProgress>>,
    /// `true` when a check is queued but has not started — the client is not
    /// making progress, so polling it as if it were requires a longer
    /// interval. Only meaningful when `state == ClientTorrentState::Checking`.
    pub queued: bool,
    /// Whatever the client can offer about why `state == Errored`. Not every
    /// adapter can populate this.
    pub message: Option<String>,
    /// Total bytes uploaded for this torrent, as the client accounts for it.
    /// Telemetry only — see `docs/todos/0009-tracker-confirmation.md`: only
    /// the tracker's own clearance ever completes a repair.
    pub uploaded_bytes: u64,
    /// How long the client has been seeding this torrent, if it tracks that.
    /// The tracker's own accounting is what actually matters for a
    /// hit-and-run and will disagree with this; both are worth showing,
    /// labelled.
    pub seeding_seconds: Option<u64>,
}

/// A torrent to hand to the client, always paused.
///
/// There is no `add_started`: nothing may begin seeding before the recheck and
/// the resume policy have both had their say.
#[derive(Clone, Copy, Debug)]
pub struct AddTorrent<'a> {
    pub info_hash: InfoHash,
    pub torrent_file: &'a [u8],
    pub save_path: &'a Path,
    pub category: Option<&'a str>,
}
