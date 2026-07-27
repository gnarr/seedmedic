use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::torrent::{SafeRelativePath, TorrentFile};

/// What a repair is looking for. Passed to every configured candidate source.
#[derive(Clone, Copy, Debug)]
pub struct CandidateQuery<'a> {
    /// The torrent's own name — usually the release name, which is what the
    /// *arr APIs can be searched by.
    pub torrent_name: &'a str,
    pub files: &'a [TorrentFile],
}

/// Where a candidate came from, kept for the audit trail: "we picked this file
/// because Sonarr instance `main` says it is episode 3" is an explanation, and
/// "we found a file of the right size somewhere under /media" is not.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CandidateOrigin {
    Sonarr { instance: String },
    Radarr { instance: String },
    Filesystem { root: PathBuf },
}

/// A file in the user's library that might be the content a torrent wants.
///
/// `path` always points into the media library and is only ever read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub origin: CandidateOrigin,
}

impl Candidate {
    pub fn file_name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    }
}

/// How sure we are that a candidate is the content the torrent wants.
///
/// Ordered: `Ambiguous < Probable < Exact`, so policy can express a floor.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchConfidence {
    /// Several plausible candidates, or one that only agrees on size.
    Ambiguous,
    /// A single candidate agreeing on both size and name.
    Probable,
    /// Verified against the torrent's piece hashes. Size alone never reaches
    /// this level — see `docs/todos/0005-media-matching.md`.
    Exact,
}

/// Why we believe what we believe. Persisted as JSON on `repair_job_files`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct MatchEvidence {
    pub size_matches: bool,
    pub name_matches: bool,
    pub candidates_with_matching_size: usize,
    /// Always false until piece verification lands (TODO 0005). Exact
    /// confidence requires it.
    pub piece_verified: bool,
}

/// One torrent file paired with the library file chosen for it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileMatch {
    pub torrent_path: SafeRelativePath,
    pub length: u64,
    pub source: PathBuf,
    pub origin: CandidateOrigin,
    pub confidence: MatchConfidence,
    pub evidence: MatchEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnmatchedFile {
    pub torrent_path: SafeRelativePath,
    pub length: u64,
    pub reason: UnmatchedReason,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnmatchedReason {
    /// Nothing in the library is the right size.
    NoCandidate,
    /// Several files are the right size and nothing distinguishes them.
    Ambiguous { candidates: usize },
}

/// The result of matching a whole torrent. A repair needs *every* file, so a
/// plan with any unmatched entry cannot be staged.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MatchPlan {
    pub matched: Vec<FileMatch>,
    pub unmatched: Vec<UnmatchedFile>,
}

impl MatchPlan {
    pub fn is_complete(&self) -> bool {
        self.unmatched.is_empty() && !self.matched.is_empty()
    }

    /// The weakest link. A plan is only as trustworthy as its worst file.
    pub fn lowest_confidence(&self) -> Option<MatchConfidence> {
        self.matched.iter().map(|file| file.confidence).min()
    }
}
