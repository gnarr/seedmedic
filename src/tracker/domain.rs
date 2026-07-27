use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::torrent::InfoHash;

/// Operator-assigned identifier for a configured tracker (`[[trackers]] id`).
/// Not the tracker's own name: it is the stable key repair jobs are filed under.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TrackerId(String);

impl TrackerId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TrackerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The tracker's own identifier for a torrent. Opaque: Unit3D uses a numeric
/// id, other families use hashes or slugs, and nothing outside the adapter may
/// assume a shape.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct TrackerTorrentId(String);

impl TrackerTorrentId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TrackerTorrentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A hit-and-run the tracker is currently holding against the user.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HitAndRun {
    pub tracker: TrackerId,
    pub torrent_id: TrackerTorrentId,
    pub torrent_name: String,
    /// Trackers do not always expose it on the warning listing, so a repair
    /// must be able to start without one and learn it from the `.torrent`.
    pub info_hash: Option<InfoHash>,
    pub size_bytes: u64,
    /// When the warning becomes a penalty, if the tracker says.
    pub deadline: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
}

/// Whether the tracker still considers the hit-and-run outstanding.
///
/// The tracker is the source of truth for completion — a torrent seeding
/// happily in the client proves nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HitAndRunStatus {
    /// Still outstanding: keep seeding.
    Active,
    /// Cleared. The repair is done.
    Cleared,
    /// The tracker answered, but not in a way we can interpret. Never treated
    /// as `Cleared`.
    Unknown,
}
