//! Getting repaired data seeding again, through a BitTorrent client.
//!
//! Named for the capability, not the vendor: qBittorrent is one adapter behind
//! [`TorrentClient`]. The rule that outlives any client choice is that a
//! torrent is added paused and only resumed once the recheck and the resume
//! policy agree.

pub mod adapters;
mod domain;
mod ports;

pub use domain::{AddTorrent, ClientTorrentState, DataCompleteness, TorrentStatus};
pub use ports::{ClientError, TorrentClient};
