use std::path::{Path, PathBuf};

use async_trait::async_trait;
use thiserror::Error;

use crate::{not_implemented::NotImplemented, torrent::SafeRelativePath};

use super::domain::{MaterializationPlan, MaterializationStrategy, StagedLayout, StagingPresence};

#[derive(Clone, Debug, Error)]
pub enum StagingError {
    #[error(transparent)]
    NotImplemented(#[from] NotImplemented),
    #[error("{strategy:?} is not available here: {reason}")]
    StrategyUnavailable {
        strategy: MaterializationStrategy,
        reason: String,
    },
    #[error("no permitted materialization strategy works for this repair")]
    NoStrategyPermitted,
    #[error(
        "staging filesystem has {available} bytes free but this plan needs {needed} bytes, \
         after keeping {margin} bytes free"
    )]
    InsufficientSpace {
        needed: u64,
        available: u64,
        margin: u64,
    },
    #[error("library file {0} no longer exists")]
    SourceMissing(PathBuf),
    #[error("library file {path} is now {actual} bytes, expected {expected}")]
    SourceChanged {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    #[error("refusing to touch {path}: {reason}")]
    UnsafePath { path: PathBuf, reason: &'static str },
    #[error("staging I/O failed: {0}")]
    Io(String),
}

impl StagingError {
    /// Only genuine I/O trouble is worth retrying. A missing source or an
    /// unavailable strategy will still be missing or unavailable next time.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Io(_))
    }
}

/// The recovery staging area.
///
/// Implementations may only write beneath their own staging root, and may only
/// ever read from library paths. See `src/staging/AGENTS.md`.
#[async_trait]
pub trait StagingFilesystem: Send + Sync {
    /// Absolute path handed to the download client as the torrent's save path.
    fn save_path(&self, job_dir: &SafeRelativePath) -> PathBuf;

    /// The staging root itself, for the diagnostics page.
    fn root_path(&self) -> &Path;

    /// Bytes free for an unprivileged writer on the staging filesystem, for
    /// the diagnostics page.
    async fn free_bytes(&self) -> Result<u64, StagingError>;

    /// Materialise every item, trying `preference` in order for each file.
    ///
    /// Must be idempotent: an item whose destination already exists at the
    /// right size is left alone, so a retry after a crash resumes rather than
    /// restarts.
    async fn materialize(
        &self,
        plan: &MaterializationPlan,
        preference: &[MaterializationStrategy],
    ) -> Result<StagedLayout, StagingError>;

    /// Does the staged data still exist? Used by startup reconciliation to
    /// detect a staging area that was cleaned up underneath us.
    async fn inspect(&self, plan: &MaterializationPlan) -> Result<StagingPresence, StagingError>;

    /// Delete a job's staging directory. The only destructive operation in the
    /// system, and it is confined to a directory SeedMedic created.
    async fn discard(&self, job_dir: &SafeRelativePath) -> Result<(), StagingError>;

    /// Total apparent size of a job's staging directory, for the job page to
    /// show how much space a repair is holding. `0` if nothing is staged.
    ///
    /// Apparent, not actual: a hardlinked file's bytes are counted even though
    /// they share an inode with the library file and cost no extra disk. That
    /// is still the number an operator deciding whether to clean up wants —
    /// "how big is this repair", not "how much would deleting it free".
    async fn usage(&self, job_dir: &SafeRelativePath) -> Result<u64, StagingError>;
}
