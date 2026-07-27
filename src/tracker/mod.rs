//! Tracker monitoring and hit-and-run discovery.
//!
//! The tracker is the source of truth for whether a hit-and-run exists and
//! whether it has been cleared. See `src/tracker/AGENTS.md` before adding an
//! adapter.

pub mod adapters;
mod domain;
mod ports;

pub use domain::{HitAndRun, HitAndRunStatus, TrackerId, TrackerTorrentId};
pub use ports::{TrackerClient, TrackerError};
