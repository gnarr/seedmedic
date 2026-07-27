use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::path::SafeRelativePath;

/// BitTorrent v1 info-hash. Kept as bytes so the hex form has exactly one
/// spelling (lowercase) everywhere it is persisted, logged, or sent to a client.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InfoHash([u8; 20]);

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum InfoHashError {
    #[error("info-hash must be 40 hex characters, got {0}")]
    Length(usize),
    #[error("info-hash contains a non-hex character")]
    NotHex,
}

impl InfoHash {
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub fn parse_hex(value: &str) -> Result<Self, InfoHashError> {
        if value.len() != 40 {
            return Err(InfoHashError::Length(value.len()));
        }
        let mut bytes = [0u8; 20];
        for (index, byte) in bytes.iter_mut().enumerate() {
            let pair = &value[index * 2..index * 2 + 2];
            *byte = u8::from_str_radix(pair, 16).map_err(|_| InfoHashError::NotHex)?;
        }
        Ok(Self(bytes))
    }

    pub fn to_hex(self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

impl std::fmt::Display for InfoHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl serde::Serialize for InfoHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for InfoHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse_hex(&raw).map_err(serde::de::Error::custom)
    }
}

/// One entry from the torrent's file list, with its path already validated.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TorrentFile {
    pub path: SafeRelativePath,
    pub length: u64,
}

/// The SHA-1 of one piece, as recorded in `info.pieces`.
///
/// Comparing this against a hash computed from a candidate's bytes is what
/// lets a match reach `MatchConfidence::Exact` — see
/// `library::verification`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PieceHash([u8; 20]);

impl PieceHash {
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
}

impl serde::Serialize for PieceHash {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for PieceHash {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <[u8; 20]>::deserialize(deserializer).map(Self)
    }
}

/// Everything the repair workflow needs from a `.torrent`.
///
/// Deliberately not a faithful representation of the bencode document: we only
/// keep what drives a repair, so there is nothing extra to keep correct.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TorrentMetadata {
    pub info_hash: InfoHash,
    /// The torrent's own name. For a multi-file torrent this is the directory
    /// every path is relative to; `files` already includes it.
    pub name: SafeRelativePath,
    pub piece_length: u64,
    pub files: Vec<TorrentFile>,
    /// One SHA-1 per piece, in order. Empty means "not available" — piece
    /// verification degrades to today's size/name matching rather than
    /// treating that as an error; see `docs/todos/0005-media-matching.md`.
    #[serde(default)]
    pub pieces: Vec<PieceHash>,
}

impl TorrentMetadata {
    pub fn total_length(&self) -> u64 {
        self.files.iter().map(|file| file.length).sum()
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_hash_round_trips_through_hex() {
        let hex = "0123456789abcdef0123456789abcdef01234567";
        let hash = InfoHash::parse_hex(hex).expect("valid info-hash");
        assert_eq!(hash.to_hex(), hex);
    }

    #[test]
    fn info_hash_rejects_bad_input() {
        assert_eq!(InfoHash::parse_hex("abc"), Err(InfoHashError::Length(3)));
        assert_eq!(
            InfoHash::parse_hex("zz23456789abcdef0123456789abcdef01234567"),
            Err(InfoHashError::NotHex)
        );
    }

    #[test]
    fn total_length_sums_files() {
        let metadata = TorrentMetadata {
            info_hash: InfoHash::from_bytes([0; 20]),
            name: SafeRelativePath::parse("Show S01").expect("valid"),
            piece_length: 1 << 20,
            files: vec![
                TorrentFile {
                    path: SafeRelativePath::parse("Show S01/e01.mkv").expect("valid"),
                    length: 10,
                },
                TorrentFile {
                    path: SafeRelativePath::parse("Show S01/e02.mkv").expect("valid"),
                    length: 32,
                },
            ],
            pieces: Vec::new(),
        };
        assert_eq!(metadata.total_length(), 42);
        assert_eq!(metadata.file_count(), 2);
    }
}
