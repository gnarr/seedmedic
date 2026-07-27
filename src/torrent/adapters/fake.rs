//! Inspector for the fake tracker's torrents.
//!
//! The fake tracker serves JSON instead of bencode; this adapter reads it back.
//! Paths and info-hashes still go through the same validating constructors as
//! real input, so a fake run exercises the real safety rules.

use crate::torrent::{
    domain::TorrentMetadata,
    ports::{InspectError, TorrentInspector},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct FakeInspector;

impl FakeInspector {
    /// Encode metadata the way the fake tracker serves it.
    pub fn encode(metadata: &TorrentMetadata) -> Vec<u8> {
        serde_json::to_vec(metadata).expect("fake torrent metadata is serialisable")
    }
}

impl TorrentInspector for FakeInspector {
    fn inspect(&self, bytes: &[u8]) -> Result<TorrentMetadata, InspectError> {
        serde_json::from_slice(bytes).map_err(|error| InspectError::Malformed(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::{InfoHash, SafeRelativePath, TorrentFile};

    #[test]
    fn round_trips_and_revalidates_paths() {
        let metadata = TorrentMetadata {
            info_hash: InfoHash::from_bytes([7; 20]),
            name: SafeRelativePath::parse("Show S01").expect("valid"),
            piece_length: 1 << 18,
            files: vec![TorrentFile {
                path: SafeRelativePath::parse("Show S01/e01.mkv").expect("valid"),
                length: 100,
            }],
        };

        let bytes = FakeInspector::encode(&metadata);
        assert_eq!(FakeInspector.inspect(&bytes).expect("round trip"), metadata);
    }

    #[test]
    fn rejects_a_traversal_path_smuggled_through_json() {
        let bytes = br#"{
            "info_hash": "0000000000000000000000000000000000000000",
            "name": "Show",
            "piece_length": 16384,
            "files": [{"path": "../../etc/passwd", "length": 1}]
        }"#;
        assert!(matches!(
            FakeInspector.inspect(bytes),
            Err(InspectError::Malformed(_))
        ));
    }
}
