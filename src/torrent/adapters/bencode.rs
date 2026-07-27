//! Real `.torrent` decoding. Not implemented in the bootstrap.
//!
//! See `docs/todos/0003-torrent-parsing.md`, which also records the open
//! decision about obtaining the raw `info` dictionary bytes for the info-hash
//! without a lossy re-encode.

use crate::{
    not_implemented::NotImplemented,
    torrent::{
        domain::TorrentMetadata,
        ports::{InspectError, TorrentInspector},
    },
};

const TODO: &str = "docs/todos/0003-torrent-parsing.md";

#[derive(Clone, Copy, Debug, Default)]
pub struct BencodeInspector;

impl TorrentInspector for BencodeInspector {
    fn inspect(&self, _bytes: &[u8]) -> Result<TorrentMetadata, InspectError> {
        Err(NotImplemented::new("torrent::adapters::bencode", TODO).into())
    }
}
