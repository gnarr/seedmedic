//! Staging on the local filesystem.
//!
//! Implements hardlink and copy. Reflink — the strategy we actually want — needs
//! `FICLONE`/`copy_file_range` handling and a filesystem probe, and is left to
//! `docs/todos/0006-staging-materialization.md`; until then it reports itself
//! unavailable so the preference list falls through to the next permitted
//! strategy, or the repair parks for review if none is permitted.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::{
    staging::{
        domain::{
            MaterializationPlan, MaterializationStrategy, PlanItem, StagedFile, StagedLayout,
            StagingPresence, StagingRoot,
        },
        ports::{StagingError, StagingFilesystem},
        safety::{create_directories, resolve_under},
    },
    torrent::SafeRelativePath,
};

const TODO: &str = "docs/todos/0006-staging-materialization.md";

pub struct LocalStaging {
    root: StagingRoot,
}

impl LocalStaging {
    pub fn new(root: StagingRoot) -> Self {
        Self { root }
    }
}

#[async_trait]
impl StagingFilesystem for LocalStaging {
    fn save_path(&self, job_dir: &SafeRelativePath) -> PathBuf {
        job_dir.join_onto(self.root.path())
    }

    async fn materialize(
        &self,
        plan: &MaterializationPlan,
        preference: &[MaterializationStrategy],
    ) -> Result<StagedLayout, StagingError> {
        let root = self.root.path().to_path_buf();
        let items = plan.items.clone();
        let preference = preference.to_vec();

        blocking(move || {
            let mut files = Vec::with_capacity(items.len());
            for item in &items {
                files.push(materialize_one(&root, item, &preference)?);
            }
            Ok(StagedLayout { files })
        })
        .await
    }

    async fn inspect(&self, plan: &MaterializationPlan) -> Result<StagingPresence, StagingError> {
        let root = self.root.path().to_path_buf();
        let items = plan.items.clone();

        blocking(move || {
            let expected = items.len();
            let present = items
                .iter()
                .filter(|item| {
                    std::fs::symlink_metadata(item.destination.join_onto(&root))
                        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == item.length)
                })
                .count();

            Ok(match present {
                0 => StagingPresence::Absent,
                present if present == expected => StagingPresence::Complete,
                present => StagingPresence::Incomplete { present, expected },
            })
        })
        .await
    }

    async fn discard(&self, job_dir: &SafeRelativePath) -> Result<(), StagingError> {
        let root = self.root.path().to_path_buf();
        let job_dir = job_dir.clone();

        blocking(move || {
            // Resolving first means we never recurse through a symlink into
            // somebody else's data.
            let directory = resolve_under(&root, &job_dir)?;
            if !directory.starts_with(&root) {
                return Err(StagingError::UnsafePath {
                    path: directory,
                    reason: "resolved outside the staging root",
                });
            }
            match std::fs::remove_dir_all(&directory) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(StagingError::Io(format!(
                    "cannot remove {}: {error}",
                    directory.display()
                ))),
            }
        })
        .await
    }
}

async fn blocking<T, F>(work: F) -> Result<T, StagingError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, StagingError> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| StagingError::Io(format!("staging task panicked: {error}")))?
}

fn materialize_one(
    root: &Path,
    item: &PlanItem,
    preference: &[MaterializationStrategy],
) -> Result<StagedFile, StagingError> {
    let destination = resolve_under(root, &item.destination)?;

    // Already there and the right size: a previous attempt got this far. Leave
    // it alone — that is what makes staging safe to retry.
    if let Ok(metadata) = std::fs::symlink_metadata(&destination) {
        if metadata.is_symlink() {
            return Err(StagingError::UnsafePath {
                path: destination,
                reason: "destination exists and is a symlink",
            });
        }
        if metadata.is_file() && metadata.len() == item.length {
            return Ok(StagedFile {
                path: item.destination.clone(),
                // The strategy is whatever the earlier attempt used; the job row
                // is the record of that, not this re-inspection.
                strategy: existing_strategy(&metadata),
                bytes: metadata.len(),
            });
        }
        // Wrong size: our own half-written leftover. Narrow, explicit, and
        // confined to our staging directory.
        std::fs::remove_file(&destination).map_err(|error| {
            StagingError::Io(format!("cannot replace {}: {error}", destination.display()))
        })?;
    }

    let source = std::fs::symlink_metadata(&item.source)
        .map_err(|_| StagingError::SourceMissing(item.source.clone()))?;
    if source.len() != item.length {
        return Err(StagingError::SourceChanged {
            path: item.source.clone(),
            expected: item.length,
            actual: source.len(),
        });
    }

    create_directories(root, &item.destination)?;

    let mut last_error = None;
    for strategy in preference {
        match attempt(*strategy, &item.source, &destination) {
            Ok(()) => {
                return Ok(StagedFile {
                    path: item.destination.clone(),
                    strategy: *strategy,
                    bytes: item.length,
                });
            }
            Err(error @ StagingError::StrategyUnavailable { .. }) => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }

    Err(last_error.unwrap_or(StagingError::NoStrategyPermitted))
}

fn attempt(
    strategy: MaterializationStrategy,
    source: &Path,
    destination: &Path,
) -> Result<(), StagingError> {
    match strategy {
        MaterializationStrategy::Reflink => Err(StagingError::StrategyUnavailable {
            strategy,
            reason: format!("reflink support is not implemented yet (see {TODO})"),
        }),
        MaterializationStrategy::Hardlink => {
            std::fs::hard_link(source, destination).map_err(|error| {
                // Cross-device is the common, expected case: fall through to
                // the next strategy rather than failing the repair.
                StagingError::StrategyUnavailable {
                    strategy,
                    reason: error.to_string(),
                }
            })
        }
        MaterializationStrategy::Copy => {
            std::fs::copy(source, destination)
                .map(|_| ())
                .map_err(|error| {
                    StagingError::Io(format!(
                        "cannot copy {} to {}: {error}",
                        source.display(),
                        destination.display()
                    ))
                })
        }
    }
}

/// Best-effort classification of a file staged by an earlier attempt. More than
/// one link means it shares an inode with something — treat that as a hardlink,
/// because assuming the safer answer here would be assuming the *less* safe
/// behaviour later.
fn existing_strategy(metadata: &std::fs::Metadata) -> MaterializationStrategy {
    use std::os::unix::fs::MetadataExt;

    if metadata.nlink() > 1 {
        MaterializationStrategy::Hardlink
    } else {
        MaterializationStrategy::Copy
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;

    struct Fixture {
        _staging: tempfile::TempDir,
        library: tempfile::TempDir,
        staging: LocalStaging,
    }

    fn fixture() -> Fixture {
        let staging_dir = tempfile::tempdir().expect("tempdir");
        let library = tempfile::tempdir().expect("tempdir");
        let root = StagingRoot::new(
            staging_dir.path().to_path_buf(),
            &[library.path().to_path_buf()],
        )
        .expect("valid staging root");

        Fixture {
            _staging: staging_dir,
            library,
            staging: LocalStaging::new(root),
        }
    }

    fn plan(fixture: &Fixture, name: &str, contents: &[u8]) -> MaterializationPlan {
        let source = fixture.library.path().join(name);
        std::fs::write(&source, contents).expect("write library file");

        MaterializationPlan {
            items: vec![PlanItem {
                source,
                destination: SafeRelativePath::parse(&format!("job-1/Show/{name}"))
                    .expect("valid destination"),
                length: contents.len() as u64,
            }],
        }
    }

    #[tokio::test]
    async fn copies_when_copy_is_the_only_permitted_strategy() {
        let fixture = fixture();
        let plan = plan(&fixture, "e01.mkv", b"contents");

        let layout = fixture
            .staging
            .materialize(&plan, &[MaterializationStrategy::Copy])
            .await
            .expect("materializes");

        assert_eq!(layout.files[0].strategy, MaterializationStrategy::Copy);
        assert!(!layout.aliases_library_files());
        assert_eq!(
            std::fs::read(
                plan.items[0]
                    .destination
                    .join_onto(fixture.staging.root.path())
            )
            .expect("staged file"),
            b"contents"
        );
    }

    #[tokio::test]
    async fn reflink_is_unavailable_and_falls_through_to_the_next_strategy() {
        let fixture = fixture();
        let plan = plan(&fixture, "e01.mkv", b"contents");

        let layout = fixture
            .staging
            .materialize(
                &plan,
                &[
                    MaterializationStrategy::Reflink,
                    MaterializationStrategy::Copy,
                ],
            )
            .await
            .expect("falls through to copy");

        assert_eq!(layout.files[0].strategy, MaterializationStrategy::Copy);
    }

    #[tokio::test]
    async fn reflink_alone_fails_rather_than_silently_downgrading() {
        let fixture = fixture();
        let plan = plan(&fixture, "e01.mkv", b"contents");

        assert!(matches!(
            fixture
                .staging
                .materialize(&plan, &[MaterializationStrategy::Reflink])
                .await,
            Err(StagingError::StrategyUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn hardlinking_is_recorded_as_aliasing_the_library() {
        let fixture = fixture();
        let plan = plan(&fixture, "e01.mkv", b"contents");

        let layout = fixture
            .staging
            .materialize(&plan, &[MaterializationStrategy::Hardlink])
            .await
            .expect("hardlinks within one filesystem");

        assert!(layout.aliases_library_files());
    }

    #[tokio::test]
    async fn materializing_twice_leaves_the_first_result_in_place() {
        let fixture = fixture();
        let plan = plan(&fixture, "e01.mkv", b"contents");
        let destination = plan.items[0]
            .destination
            .join_onto(fixture.staging.root.path());

        fixture
            .staging
            .materialize(&plan, &[MaterializationStrategy::Copy])
            .await
            .expect("first attempt");
        let first = std::fs::symlink_metadata(&destination)
            .expect("staged")
            .ino();
        fixture
            .staging
            .materialize(&plan, &[MaterializationStrategy::Copy])
            .await
            .expect("second attempt");
        let second = std::fs::symlink_metadata(&destination)
            .expect("staged")
            .ino();

        assert_eq!(first, second, "a retry must not rewrite completed work");
    }

    #[tokio::test]
    async fn a_library_file_that_changed_since_matching_is_refused() {
        let fixture = fixture();
        let mut plan = plan(&fixture, "e01.mkv", b"contents");
        plan.items[0].length = 999;

        assert!(matches!(
            fixture
                .staging
                .materialize(&plan, &[MaterializationStrategy::Copy])
                .await,
            Err(StagingError::SourceChanged { .. })
        ));
    }

    #[tokio::test]
    async fn a_missing_library_file_is_refused() {
        let fixture = fixture();
        let mut plan = plan(&fixture, "e01.mkv", b"contents");
        plan.items[0].source = fixture.library.path().join("gone.mkv");

        assert!(matches!(
            fixture
                .staging
                .materialize(&plan, &[MaterializationStrategy::Copy])
                .await,
            Err(StagingError::SourceMissing(_))
        ));
    }

    #[tokio::test]
    async fn inspect_reports_presence_and_discard_removes_it() {
        let fixture = fixture();
        let plan = plan(&fixture, "e01.mkv", b"contents");
        let job_dir = SafeRelativePath::parse("job-1").expect("valid");

        assert_eq!(
            fixture.staging.inspect(&plan).await.expect("inspect"),
            StagingPresence::Absent
        );

        fixture
            .staging
            .materialize(&plan, &[MaterializationStrategy::Copy])
            .await
            .expect("materializes");
        assert_eq!(
            fixture.staging.inspect(&plan).await.expect("inspect"),
            StagingPresence::Complete
        );

        fixture.staging.discard(&job_dir).await.expect("discard");
        assert_eq!(
            fixture.staging.inspect(&plan).await.expect("inspect"),
            StagingPresence::Absent
        );
        assert!(
            fixture.library.path().join("e01.mkv").exists(),
            "discarding staging must never touch the library"
        );
    }

    #[tokio::test]
    async fn discarding_a_directory_that_is_not_there_is_fine() {
        let fixture = fixture();
        let job_dir = SafeRelativePath::parse("job-404").expect("valid");

        fixture.staging.discard(&job_dir).await.expect("idempotent");
    }
}
