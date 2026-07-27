//! Torrent metadata and the path-safety rules that go with it.
//!
//! The `.torrent` bytes themselves are persisted inline on the repair job (see
//! `migrations/0001_initial.sql`) rather than in a separate blob store, so
//! acquiring a torrent is atomic with the transition that records it and there
//! is nothing extra to reconcile after a crash.

pub mod adapters;
mod domain;
pub mod path;
mod ports;

pub use domain::{InfoHash, InfoHashError, TorrentFile, TorrentMetadata};
pub use path::{PathRejection, SafeRelativePath};
pub use ports::{InspectError, TorrentInspector};
