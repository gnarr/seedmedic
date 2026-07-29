//! Stands in for the staging filesystem when `staging.root` is unset.
//!
//! Every fallible method fails loudly with `NotImplemented`, so a repair that
//! reaches materialization parks for review naming the missing setting rather
//! than guessing a path or silently succeeding. See
//! `docs/todos/0015-start-without-a-configuration-file.md`.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::{not_implemented::NotImplemented, torrent::SafeRelativePath};

use super::super::{
    domain::{MaterializationPlan, MaterializationStrategy, StagedLayout, StagingPresence},
    ports::{StagingError, StagingFilesystem},
};

const TODO: &str = "set staging.root — see Settings → Staging";

pub struct UnconfiguredStaging;

#[async_trait]
impl StagingFilesystem for UnconfiguredStaging {
    fn save_path(&self, job_dir: &SafeRelativePath) -> PathBuf {
        job_dir.join_onto(Path::new(""))
    }

    /// Synchronous and infallible, so it cannot report the missing setting
    /// the way the other methods do. An empty path is the honest answer;
    /// callers must render it as "not configured" rather than a bare string.
    fn root_path(&self) -> &Path {
        Path::new("")
    }

    async fn free_bytes(&self) -> Result<u64, StagingError> {
        Err(NotImplemented::new("staging", TODO).into())
    }

    async fn materialize(
        &self,
        _plan: &MaterializationPlan,
        _preference: &[MaterializationStrategy],
    ) -> Result<StagedLayout, StagingError> {
        Err(NotImplemented::new("staging", TODO).into())
    }

    async fn inspect(&self, _plan: &MaterializationPlan) -> Result<StagingPresence, StagingError> {
        Err(NotImplemented::new("staging", TODO).into())
    }

    async fn discard(&self, _job_dir: &SafeRelativePath) -> Result<(), StagingError> {
        Err(NotImplemented::new("staging", TODO).into())
    }

    async fn usage(&self, _job_dir: &SafeRelativePath) -> Result<u64, StagingError> {
        Err(NotImplemented::new("staging", TODO).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_path_is_empty_rather_than_panicking() {
        assert_eq!(UnconfiguredStaging.root_path(), Path::new(""));
    }

    #[tokio::test]
    async fn every_fallible_method_names_staging_root() {
        let adapter = UnconfiguredStaging;
        let plan = MaterializationPlan { items: Vec::new() };
        let job_dir = SafeRelativePath::parse("job-1").expect("valid");

        let errors = [
            adapter.free_bytes().await.unwrap_err().to_string(),
            adapter
                .materialize(&plan, &[])
                .await
                .unwrap_err()
                .to_string(),
            adapter.inspect(&plan).await.unwrap_err().to_string(),
            adapter.discard(&job_dir).await.unwrap_err().to_string(),
            adapter.usage(&job_dir).await.unwrap_err().to_string(),
        ];

        for error in errors {
            assert!(error.contains("staging.root"), "{error}");
        }
    }
}
