//! Candidate discovery by walking a media-library root.
//!
//! The fallback for content no *arr knows about. Read-only by construction: it
//! only ever calls `read_dir` and `symlink_metadata`.
//!
//! Two deliberate limitations:
//!
//! - It walks the root on every query, rather than keeping a cached size
//!   index. Repairs are rare and run one at a time, and the walk is
//!   `stat`-only, so this stays acceptable until measurement says otherwise —
//!   build the index when a real library shows it is not.
//! - It skips symlinks entirely. Following them invites loops and makes
//!   reflink/hardlink reasoning depend on where the link lands.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use async_trait::async_trait;

use crate::library::{
    domain::{Candidate, CandidateOrigin, CandidateQuery},
    ports::{CandidateError, CandidateSource},
};

/// Guards against pathological trees; media libraries are nowhere near this deep.
const MAX_DEPTH: usize = 16;

pub struct FilesystemCandidateSource {
    label: String,
    root: PathBuf,
}

impl FilesystemCandidateSource {
    pub fn new(root: PathBuf) -> Self {
        Self {
            label: format!("filesystem:{}", root.display()),
            root,
        }
    }
}

#[async_trait]
impl CandidateSource for FilesystemCandidateSource {
    fn label(&self) -> &str {
        &self.label
    }

    async fn find_candidates(
        &self,
        query: &CandidateQuery<'_>,
    ) -> Result<Vec<Candidate>, CandidateError> {
        let wanted: HashSet<u64> = query.files.iter().map(|file| file.length).collect();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }

        let root = self.root.clone();
        tokio::task::spawn_blocking(move || collect(&root, &wanted))
            .await
            .map_err(|error| CandidateError::Io(format!("library scan panicked: {error}")))?
    }
}

fn collect(root: &Path, wanted: &HashSet<u64>) -> Result<Vec<Candidate>, CandidateError> {
    let mut found = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0usize)];

    while let Some((directory, depth)) = pending.pop() {
        if depth > MAX_DEPTH {
            continue;
        }

        let entries = std::fs::read_dir(&directory).map_err(|error| {
            CandidateError::Io(format!("cannot read {}: {error}", directory.display()))
        })?;

        for entry in entries {
            let entry = entry.map_err(|error| {
                CandidateError::Io(format!("cannot read {}: {error}", directory.display()))
            })?;
            let path = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                // Vanished mid-walk, or unreadable. Not our business to fix.
                continue;
            };

            if metadata.is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                pending.push((path, depth + 1));
            } else if metadata.is_file() && wanted.contains(&metadata.len()) {
                found.push(Candidate {
                    path,
                    size_bytes: metadata.len(),
                    origin: CandidateOrigin::Filesystem {
                        root: root.to_path_buf(),
                    },
                });
            }
        }
    }

    // The walk order depends on the filesystem; matching must not.
    found.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::{SafeRelativePath, TorrentFile};

    #[tokio::test]
    async fn finds_only_files_of_a_wanted_size() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("Show/Season 01")).expect("dirs");
        std::fs::write(root.path().join("Show/Season 01/e01.mkv"), b"1234567890").expect("write");
        std::fs::write(root.path().join("Show/Season 01/e02.mkv"), b"short").expect("write");

        let source = FilesystemCandidateSource::new(root.path().to_path_buf());
        let files = vec![TorrentFile {
            path: SafeRelativePath::parse("Show/e01.mkv").expect("valid"),
            length: 10,
        }];

        let candidates = source
            .find_candidates(&CandidateQuery {
                torrent_name: "Show S01",
                files: &files,
            })
            .await
            .expect("scan succeeds");

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].size_bytes, 10);
        assert!(candidates[0].path.ends_with("e01.mkv"));
    }
}
