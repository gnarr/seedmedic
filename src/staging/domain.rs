use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::torrent::SafeRelativePath;

/// How a staged file is backed by the library file it came from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializationStrategy {
    /// Copy-on-write clone. Costs no space and shares no fate: a write to the
    /// staged file allocates new extents and leaves the library file alone.
    /// Always preferred.
    Reflink,
    /// A second name for the *same inode*. Costs no space and shares every
    /// fate: anything that writes to the staged file writes to the library
    /// file. This is why an incomplete hardlinked torrent must never be
    /// resumed — the client would "repair" the user's media.
    Hardlink,
    /// An independent duplicate. Costs the full size, shares nothing.
    Copy,
}

impl MaterializationStrategy {
    /// Whether the staged file and the library file are the same bytes on disk,
    /// such that writing to one writes to the other.
    pub fn aliases_library_file(self) -> bool {
        matches!(self, Self::Hardlink)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reflink => "reflink",
            Self::Hardlink => "hardlink",
            Self::Copy => "copy",
        }
    }
}

impl std::str::FromStr for MaterializationStrategy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "reflink" => Ok(Self::Reflink),
            "hardlink" => Ok(Self::Hardlink),
            "copy" => Ok(Self::Copy),
            other => Err(format!("unknown materialization strategy `{other}`")),
        }
    }
}

/// Whether reflink cloning works between one device and another, established
/// by [`crate::staging::adapters::local::LocalStaging`] once per (source
/// device, staging device) pair and cached for the process lifetime, because a
/// mounted filesystem does not change its mind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReflinkSupport {
    Supported,
    Unsupported { reason: String },
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum StagingRootError {
    #[error("staging root must be an absolute path, got {0}")]
    NotAbsolute(PathBuf),
    #[error("cannot use staging root {path}: {reason}")]
    Unusable { path: PathBuf, reason: String },
    #[error(
        "staging root {staging} overlaps media library root {library}; staging must be a separate directory"
    )]
    OverlapsLibrary { staging: PathBuf, library: PathBuf },
}

/// A validated directory that SeedMedic owns and may write to.
///
/// Validated once at startup so that no later code has to wonder whether the
/// place it is writing to is really the user's media library.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagingRoot(PathBuf);

impl StagingRoot {
    /// Create (if needed) and validate the staging root.
    ///
    /// Fails if it is not absolute, cannot be created, or overlaps any media
    /// library root in either direction.
    pub fn new(path: PathBuf, library_roots: &[PathBuf]) -> Result<Self, StagingRootError> {
        if !path.is_absolute() {
            return Err(StagingRootError::NotAbsolute(path));
        }

        std::fs::create_dir_all(&path).map_err(|error| StagingRootError::Unusable {
            path: path.clone(),
            reason: error.to_string(),
        })?;
        let staging = path
            .canonicalize()
            .map_err(|error| StagingRootError::Unusable {
                path: path.clone(),
                reason: error.to_string(),
            })?;

        for library in library_roots {
            // A library root that does not exist cannot be overlapped, and is
            // the candidate source's problem to report.
            let Ok(library) = library.canonicalize() else {
                continue;
            };
            if staging.starts_with(&library) || library.starts_with(&staging) {
                return Err(StagingRootError::OverlapsLibrary { staging, library });
            }
        }

        Ok(Self(staging))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Checks the overlap invariant without creating or writing anything —
    /// unlike [`Self::new`], which must exist because it is used before the
    /// directory necessarily exists. Used by `--check-config`, which must
    /// never touch the filesystem beyond reading it. Less exact than `new`
    /// when a path involves a symlink that has not been created yet, but that
    /// is the best available answer before the directory exists.
    pub fn check_overlap(path: &Path, library_roots: &[PathBuf]) -> Result<(), StagingRootError> {
        if !path.is_absolute() {
            return Err(StagingRootError::NotAbsolute(path.to_path_buf()));
        }

        let staging = lexical_resolve(path);
        for library in library_roots {
            let library = lexical_resolve(library);
            if staging.starts_with(&library) || library.starts_with(&staging) {
                return Err(StagingRootError::OverlapsLibrary { staging, library });
            }
        }

        Ok(())
    }
}

/// Best-effort canonicalization that never writes: canonicalize the deepest
/// existing ancestor, then lexically append the components that do not exist
/// yet.
fn lexical_resolve(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut remainder = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                remainder.push(name.to_owned());
                existing = parent;
            }
            _ => break,
        }
    }

    let mut resolved = existing
        .canonicalize()
        .unwrap_or_else(|_| existing.to_path_buf());
    resolved.extend(remainder.into_iter().rev());
    resolved
}

/// One file to materialise: read `source`, produce `destination` under the
/// job's staging directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanItem {
    pub source: PathBuf,
    /// Relative to the staging root, job directory included.
    pub destination: SafeRelativePath,
    /// The size the torrent expects. Re-checked against `source` immediately
    /// before materialising, because the library may have changed since
    /// matching.
    pub length: u64,
    /// What the job row says an earlier attempt used, if this file was
    /// already staged once. `materialize` trusts this rather than re-deriving
    /// a strategy from the file it finds on disk: reflinks are not detectable
    /// portably, and guessing would risk reporting a safer strategy than what
    /// is really there. `None` when nothing has staged this file yet.
    pub previous_strategy: Option<MaterializationStrategy>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationPlan {
    pub items: Vec<PlanItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedFile {
    pub path: SafeRelativePath,
    pub strategy: MaterializationStrategy,
    pub bytes: u64,
}

/// What ended up on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedLayout {
    pub files: Vec<StagedFile>,
    /// The reflink probe outcome for this plan's (source device, staging
    /// device) pair, so a job's audit trail can say why reflink was or was not
    /// used. `None` when nothing in the plan asked for reflink.
    pub reflink: Option<ReflinkSupport>,
}

impl StagedLayout {
    /// True if any file shares an inode with the media library. The resume
    /// guard in `repair::policy` keys off this.
    pub fn aliases_library_files(&self) -> bool {
        self.files
            .iter()
            .any(|file| file.strategy.aliases_library_file())
    }

    /// The single strategy recorded on the job: the riskiest one used, because
    /// safety decisions must be driven by the worst file, not the average.
    pub fn summary_strategy(&self) -> Option<MaterializationStrategy> {
        if self.files.iter().any(|f| f.strategy.aliases_library_file()) {
            return Some(MaterializationStrategy::Hardlink);
        }
        if self
            .files
            .iter()
            .any(|f| f.strategy == MaterializationStrategy::Copy)
        {
            return Some(MaterializationStrategy::Copy);
        }
        self.files.first().map(|file| file.strategy)
    }

    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.bytes).sum()
    }
}

/// Whether a job's staging directory still holds what we think it holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StagingPresence {
    Absent,
    Incomplete { present: usize, expected: usize },
    Complete,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staged(strategy: MaterializationStrategy) -> StagedFile {
        StagedFile {
            path: SafeRelativePath::parse("job-1/e01.mkv").expect("valid"),
            strategy,
            bytes: 10,
        }
    }

    #[test]
    fn a_single_hardlink_makes_the_whole_layout_aliasing() {
        let layout = StagedLayout {
            files: vec![
                staged(MaterializationStrategy::Reflink),
                staged(MaterializationStrategy::Hardlink),
                staged(MaterializationStrategy::Copy),
            ],
            reflink: None,
        };

        assert!(layout.aliases_library_files());
        assert_eq!(
            layout.summary_strategy(),
            Some(MaterializationStrategy::Hardlink)
        );
    }

    #[test]
    fn reflink_only_layouts_do_not_alias() {
        let layout = StagedLayout {
            files: vec![staged(MaterializationStrategy::Reflink)],
            reflink: None,
        };

        assert!(!layout.aliases_library_files());
        assert_eq!(
            layout.summary_strategy(),
            Some(MaterializationStrategy::Reflink)
        );
    }

    #[test]
    fn staging_root_must_not_sit_inside_the_library() {
        let library = tempfile::tempdir().expect("tempdir");
        let staging = library.path().join("staging");

        assert!(matches!(
            StagingRoot::new(staging, &[library.path().to_path_buf()]),
            Err(StagingRootError::OverlapsLibrary { .. })
        ));
    }

    #[test]
    fn library_must_not_sit_inside_the_staging_root() {
        let staging = tempfile::tempdir().expect("tempdir");
        let library = staging.path().join("media");
        std::fs::create_dir_all(&library).expect("library dir");

        assert!(matches!(
            StagingRoot::new(staging.path().to_path_buf(), &[library]),
            Err(StagingRootError::OverlapsLibrary { .. })
        ));
    }

    #[test]
    fn a_separate_directory_is_accepted_and_created() {
        let library = tempfile::tempdir().expect("tempdir");
        let parent = tempfile::tempdir().expect("tempdir");
        let staging = parent.path().join("seedmedic/staging");

        let root = StagingRoot::new(staging, &[library.path().to_path_buf()]).expect("accepted");

        assert!(root.path().is_dir());
    }

    #[test]
    fn relative_staging_roots_are_rejected() {
        assert!(matches!(
            StagingRoot::new(PathBuf::from("staging"), &[]),
            Err(StagingRootError::NotAbsolute(_))
        ));
    }

    #[test]
    fn check_overlap_catches_a_staging_root_inside_the_library_before_it_is_created() {
        let library = tempfile::tempdir().expect("tempdir");
        let staging = library.path().join("not-created-yet/staging");

        assert!(matches!(
            StagingRoot::check_overlap(&staging, &[library.path().to_path_buf()]),
            Err(StagingRootError::OverlapsLibrary { .. })
        ));
    }

    #[test]
    fn check_overlap_accepts_a_separate_directory_without_creating_it() {
        let library = tempfile::tempdir().expect("tempdir");
        let parent = tempfile::tempdir().expect("tempdir");
        let staging = parent.path().join("not-created-yet/staging");

        assert!(StagingRoot::check_overlap(&staging, &[library.path().to_path_buf()]).is_ok());
        assert!(!staging.exists());
    }
}
