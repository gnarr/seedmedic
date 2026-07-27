//! qBittorrent WebUI adapter. Not implemented in the bootstrap.
//!
//! See `docs/todos/0007-qbittorrent-adapter.md` for the endpoints, the session
//! cookie handling, and the mapping from qBittorrent's many state strings onto
//! the five states this port cares about.

use async_trait::async_trait;
use url::Url;

use crate::{
    not_implemented::NotImplemented,
    seeding::{
        domain::{AddTorrent, TorrentStatus},
        ports::{ClientError, TorrentClient},
    },
    torrent::InfoHash,
};

const TODO: &str = "docs/todos/0007-qbittorrent-adapter.md";

pub struct QBittorrentClient {
    #[allow(dead_code, reason = "used once docs/todos/0007 lands")]
    base_url: Url,
    #[allow(dead_code, reason = "used once docs/todos/0007 lands")]
    category: Option<String>,
}

impl QBittorrentClient {
    pub fn new(base_url: Url, category: Option<String>) -> Self {
        Self { base_url, category }
    }

    fn unimplemented() -> ClientError {
        NotImplemented::new("seeding::adapters::qbittorrent", TODO).into()
    }
}

#[async_trait]
impl TorrentClient for QBittorrentClient {
    async fn add_paused(&self, _request: AddTorrent<'_>) -> Result<(), ClientError> {
        Err(Self::unimplemented())
    }

    async fn status(&self, _info_hash: InfoHash) -> Result<Option<TorrentStatus>, ClientError> {
        Err(Self::unimplemented())
    }

    async fn recheck(&self, _info_hash: InfoHash) -> Result<(), ClientError> {
        Err(Self::unimplemented())
    }

    async fn resume(&self, _info_hash: InfoHash) -> Result<(), ClientError> {
        Err(Self::unimplemented())
    }

    async fn remove(&self, _info_hash: InfoHash, _delete_files: bool) -> Result<(), ClientError> {
        Err(Self::unimplemented())
    }
}
