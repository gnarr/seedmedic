//! Stands in for the download client when `download_client` is unset.
//!
//! Every method fails loudly with `NotImplemented`, so a repair that reaches
//! seeding parks for review naming the missing setting rather than pretending
//! a client is there. See
//! `docs/todos/0015-start-without-a-configuration-file.md`.

use async_trait::async_trait;

use crate::{not_implemented::NotImplemented, torrent::InfoHash};

use super::super::{
    domain::{AddTorrent, TorrentStatus},
    ports::{ClientError, ClientSummary, TorrentClient},
};

const TODO: &str = "set download_client — see Settings → Download client";

pub struct UnconfiguredClient;

#[async_trait]
impl TorrentClient for UnconfiguredClient {
    async fn add_paused(&self, _request: AddTorrent<'_>) -> Result<(), ClientError> {
        Err(NotImplemented::new("download_client", TODO).into())
    }

    async fn status(&self, _info_hash: InfoHash) -> Result<Option<TorrentStatus>, ClientError> {
        Err(NotImplemented::new("download_client", TODO).into())
    }

    async fn recheck(&self, _info_hash: InfoHash) -> Result<(), ClientError> {
        Err(NotImplemented::new("download_client", TODO).into())
    }

    async fn resume(&self, _info_hash: InfoHash) -> Result<(), ClientError> {
        Err(NotImplemented::new("download_client", TODO).into())
    }

    async fn remove(&self, _info_hash: InfoHash, _delete_files: bool) -> Result<(), ClientError> {
        Err(NotImplemented::new("download_client", TODO).into())
    }

    async fn summary(&self) -> Result<ClientSummary, ClientError> {
        Err(NotImplemented::new("download_client", TODO).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_method_names_download_client() {
        let adapter = UnconfiguredClient;
        let hash = InfoHash::from_bytes([0; 20]);

        let errors = [
            adapter
                .add_paused(AddTorrent {
                    info_hash: hash,
                    torrent_file: &[],
                    save_path: std::path::Path::new(""),
                    category: None,
                })
                .await
                .unwrap_err()
                .to_string(),
            adapter.status(hash).await.unwrap_err().to_string(),
            adapter.recheck(hash).await.unwrap_err().to_string(),
            adapter.resume(hash).await.unwrap_err().to_string(),
            adapter.remove(hash, false).await.unwrap_err().to_string(),
            adapter.summary().await.unwrap_err().to_string(),
        ];

        for error in errors {
            assert!(error.contains("download_client"), "{error}");
        }
    }
}
