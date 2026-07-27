//! Staging on the local filesystem.
//!
//! Implements reflink, hardlink, and copy. Reflink and hardlink both depend on
//! source and staging sharing a device, and reflink additionally needs the
//! filesystem to support `FICLONE`-style cloning; both are probed once per
//! (source device, staging device) pair and cached for the process lifetime,
//! rather than discovered file by file.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;

use crate::{
    staging::{
        domain::{
            MaterializationPlan, MaterializationStrategy, PlanItem, ReflinkSupport, StagedFile,
            StagedLayout, StagingPresence, StagingRoot,
        },
        ports::{StagingError, StagingFilesystem},
        safety::{create_directories, resolve_under},
    },
    torrent::SafeRelativePath,
};

/// What was learned, once, about a (source device, staging device) pair.
///
/// Computing this touches the filesystem (a hardlink needs nothing beyond the
/// device comparison already made here, but reflink support is confirmed with
/// a real clone attempt), so it is cached rather than repeated per file.
#[derive(Clone, Debug, Eq, PartialEq)]
struct DeviceCapabilities {
    can_hardlink: bool,
    reflink: ReflinkSupport,
}

pub struct LocalStaging {
    root: StagingRoot,
    /// Free space to keep on the staging filesystem beyond what a plan needs.
    min_free_bytes: u64,
    device_probes: Arc<Mutex<HashMap<(u64, u64), DeviceCapabilities>>>,
    /// How many times a probe actually touched the filesystem, rather than
    /// answering from cache. Exists so tests can prove a many-file plan only
    /// probes once per device pair.
    probe_attempts: Arc<AtomicUsize>,
}

impl LocalStaging {
    pub fn new(root: StagingRoot, min_free_bytes: u64) -> Self {
        Self {
            root,
            min_free_bytes,
            device_probes: Arc::new(Mutex::new(HashMap::new())),
            probe_attempts: Arc::new(AtomicUsize::new(0)),
        }
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
        let min_free_bytes = self.min_free_bytes;
        let probes = Arc::clone(&self.device_probes);
        let probe_attempts = Arc::clone(&self.probe_attempts);

        blocking(move || {
            let staging_dev = device_id(&root)?;
            let reflink = plan_reflink_probe(
                &root,
                &items,
                &preference,
                staging_dev,
                &probes,
                &probe_attempts,
            );

            let needed = required_new_bytes(
                &root,
                &items,
                &preference,
                staging_dev,
                &probes,
                &probe_attempts,
            )?;
            if needed > 0 {
                let available = available_bytes(&root)?;
                if needed > available.saturating_sub(min_free_bytes) {
                    return Err(StagingError::InsufficientSpace {
                        needed,
                        available,
                        margin: min_free_bytes,
                    });
                }
            }

            let mut files = Vec::with_capacity(items.len());
            for item in &items {
                files.push(materialize_one(
                    &root,
                    item,
                    &preference,
                    staging_dev,
                    &probes,
                    &probe_attempts,
                )?);
            }
            Ok(StagedLayout { files, reflink })
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
    staging_dev: u64,
    probes: &Mutex<HashMap<(u64, u64), DeviceCapabilities>>,
    probe_attempts: &AtomicUsize,
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

    let capabilities = probe_devices(
        root,
        device_of(&source),
        staging_dev,
        probes,
        probe_attempts,
    );

    let mut last_error = None;
    for strategy in preference {
        match attempt(*strategy, &item.source, &destination, &capabilities) {
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
    capabilities: &DeviceCapabilities,
) -> Result<(), StagingError> {
    match strategy {
        MaterializationStrategy::Reflink => match &capabilities.reflink {
            ReflinkSupport::Unsupported { reason } => Err(StagingError::StrategyUnavailable {
                strategy,
                reason: reason.clone(),
            }),
            ReflinkSupport::Supported => {
                reflink_copy::reflink(source, destination).map_err(|error| {
                    StagingError::Io(format!(
                        "cannot reflink {} to {}: {error}",
                        source.display(),
                        destination.display()
                    ))
                })
            }
        },
        MaterializationStrategy::Hardlink => {
            if !capabilities.can_hardlink {
                // Known up front from the device probe: do not even try the
                // syscall, because it is guaranteed to fail with EXDEV.
                return Err(StagingError::StrategyUnavailable {
                    strategy,
                    reason: "source and staging are on different devices".to_owned(),
                });
            }
            std::fs::hard_link(source, destination).map_err(|error| {
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

/// Bytes this plan would newly write: a file already staged at the right size
/// costs nothing (idempotent skip), and a file that would reflink or hardlink
/// costs nothing either. Only a predicted copy counts, so the check that uses
/// this can run before anything is written.
fn required_new_bytes(
    root: &Path,
    items: &[PlanItem],
    preference: &[MaterializationStrategy],
    staging_dev: u64,
    probes: &Mutex<HashMap<(u64, u64), DeviceCapabilities>>,
    probe_attempts: &AtomicUsize,
) -> Result<u64, StagingError> {
    let mut total = 0u64;
    for item in items {
        let destination = resolve_under(root, &item.destination)?;
        if already_staged(&destination, item.length) {
            continue;
        }

        // Unknown source device: assume the worst (a full copy) rather than
        // fail the space check early. materialize_one reports the real
        // problem — missing or changed source — when it gets there.
        let source_dev = std::fs::metadata(&item.source).ok().map(|m| device_of(&m));
        let cost = match source_dev {
            Some(source_dev) => predicted_cost(
                item.length,
                preference,
                source_dev,
                staging_dev,
                root,
                probes,
                probe_attempts,
            ),
            None => item.length,
        };
        total = total.saturating_add(cost);
    }
    Ok(total)
}

fn already_staged(destination: &Path, expected_length: u64) -> bool {
    std::fs::symlink_metadata(destination)
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == expected_length)
}

/// What a file of `length` bytes would cost to materialise, given the first
/// strategy in `preference` that the device probe says will work. Mirrors the
/// order [`attempt`] tries strategies in, without doing any I/O beyond the
/// probe itself.
fn predicted_cost(
    length: u64,
    preference: &[MaterializationStrategy],
    source_dev: u64,
    staging_dev: u64,
    root: &Path,
    probes: &Mutex<HashMap<(u64, u64), DeviceCapabilities>>,
    probe_attempts: &AtomicUsize,
) -> u64 {
    let capabilities = probe_devices(root, source_dev, staging_dev, probes, probe_attempts);
    for &strategy in preference {
        let viable = match strategy {
            MaterializationStrategy::Reflink => capabilities.reflink == ReflinkSupport::Supported,
            MaterializationStrategy::Hardlink => capabilities.can_hardlink,
            MaterializationStrategy::Copy => return length,
        };
        if viable {
            return 0;
        }
    }
    // Nothing in preference would work: materialize_one will fail on its own
    // with a clearer reason. Assume the worst so this check stays a safe
    // upper bound rather than a false negative.
    length
}

/// Bytes free for an unprivileged writer on the filesystem that holds `path`.
fn available_bytes(path: &Path) -> Result<u64, StagingError> {
    use std::os::unix::ffi::OsStrExt;

    let cstr = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|error| StagingError::Io(format!("cannot stat {}: {error}", path.display())))?;

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `cstr` is a valid, nul-terminated C string for the lifetime of
    // this call, and `stat` is a valid, appropriately sized out-parameter.
    let result = unsafe { libc::statvfs(cstr.as_ptr(), &mut stat) };
    if result != 0 {
        return Err(StagingError::Io(format!(
            "cannot statvfs {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        )));
    }

    Ok(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// The reflink probe outcome for the plan's dominant (first item's) device
/// pair, for the audit trail. `None` when nothing in the plan would use
/// reflink or the first item's source cannot be inspected — informational
/// only, so it fails soft rather than blocking the plan.
fn plan_reflink_probe(
    root: &Path,
    items: &[PlanItem],
    preference: &[MaterializationStrategy],
    staging_dev: u64,
    probes: &Mutex<HashMap<(u64, u64), DeviceCapabilities>>,
    probe_attempts: &AtomicUsize,
) -> Option<ReflinkSupport> {
    if !preference.contains(&MaterializationStrategy::Reflink) {
        return None;
    }
    let source_dev = items
        .first()
        .and_then(|item| std::fs::symlink_metadata(&item.source).ok())
        .map(|metadata| device_of(&metadata))?;
    Some(probe_devices(root, source_dev, staging_dev, probes, probe_attempts).reflink)
}

/// Look up (or, on a cache miss, establish) whether a hardlink and a reflink
/// each work between `source_dev` and `staging_dev`.
fn probe_devices(
    root: &Path,
    source_dev: u64,
    staging_dev: u64,
    probes: &Mutex<HashMap<(u64, u64), DeviceCapabilities>>,
    probe_attempts: &AtomicUsize,
) -> DeviceCapabilities {
    let key = (source_dev, staging_dev);
    let mut cache = probes.lock().expect("device probe cache lock");
    if let Some(cached) = cache.get(&key) {
        return cached.clone();
    }

    probe_attempts.fetch_add(1, Ordering::Relaxed);
    let can_hardlink = source_dev == staging_dev;
    let reflink = if can_hardlink {
        probe_reflink_capability(root)
    } else {
        ReflinkSupport::Unsupported {
            reason: "source and staging are on different devices".to_owned(),
        }
    };

    let capabilities = DeviceCapabilities {
        can_hardlink,
        reflink,
    };
    cache.insert(key, capabilities.clone());
    capabilities
}

/// Confirm reflink support with a real, zero-length clone confined to the
/// staging root — the pragmatic way to find out without touching the library.
fn probe_reflink_capability(root: &Path) -> ReflinkSupport {
    let probe_src = root.join(".seedmedic-reflink-probe-src");
    let probe_dst = root.join(".seedmedic-reflink-probe-dst");
    let _ = std::fs::remove_file(&probe_dst);

    let result = std::fs::write(&probe_src, []).and_then(|()| {
        let _ = std::fs::remove_file(&probe_dst);
        reflink_copy::reflink(&probe_src, &probe_dst)
    });

    let _ = std::fs::remove_file(&probe_src);
    let _ = std::fs::remove_file(&probe_dst);

    match result {
        Ok(()) => ReflinkSupport::Supported,
        Err(error) => ReflinkSupport::Unsupported {
            reason: format!("reflink probe failed: {error}"),
        },
    }
}

fn device_id(path: &Path) -> Result<u64, StagingError> {
    std::fs::metadata(path)
        .map(|metadata| device_of(&metadata))
        .map_err(|error| StagingError::Io(format!("cannot stat {}: {error}", path.display())))
}

fn device_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
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
            staging: LocalStaging::new(root, 0),
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
    async fn reflinking_succeeds_when_the_filesystem_supports_it() {
        let fixture = fixture();
        let plan = plan(&fixture, "e01.mkv", b"contents");

        match fixture
            .staging
            .materialize(&plan, &[MaterializationStrategy::Reflink])
            .await
        {
            Ok(layout) => {
                assert_eq!(layout.files[0].strategy, MaterializationStrategy::Reflink);
                assert!(!layout.aliases_library_files());
                assert_eq!(layout.reflink, Some(ReflinkSupport::Supported));
            }
            Err(StagingError::StrategyUnavailable { .. }) => {
                eprintln!("skipping: this filesystem does not support reflink");
            }
            Err(error) => panic!("unexpected error: {error}"),
        }
    }

    #[tokio::test]
    async fn reflink_falls_through_to_copy_when_unsupported() {
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
            .expect("reflinks outright, or falls through to copy");

        match layout.files[0].strategy {
            MaterializationStrategy::Copy => {}
            MaterializationStrategy::Reflink => {
                eprintln!(
                    "skipping: this filesystem supports reflink, so there is nothing to fall through from"
                );
            }
            other => panic!("unexpected strategy {other:?}"),
        }
    }

    #[test]
    fn hardlink_is_never_attempted_across_devices() {
        let staging_dir = tempfile::tempdir().expect("tempdir");
        let source = staging_dir.path().join("src.bin");
        std::fs::write(&source, b"contents").expect("write source");
        let destination = staging_dir.path().join("dst.bin");
        let capabilities = DeviceCapabilities {
            can_hardlink: false,
            reflink: ReflinkSupport::Unsupported {
                reason: "different devices".to_owned(),
            },
        };

        assert!(matches!(
            attempt(
                MaterializationStrategy::Hardlink,
                &source,
                &destination,
                &capabilities
            ),
            Err(StagingError::StrategyUnavailable { .. })
        ));
        assert!(
            !destination.exists(),
            "a cross-device hardlink must never be attempted, let alone produced"
        );
    }

    #[tokio::test]
    async fn the_device_probe_runs_once_for_a_many_file_plan() {
        let fixture = fixture();
        let items = (0..5)
            .map(|index| {
                let name = format!("e0{index}.mkv");
                let source = fixture.library.path().join(&name);
                std::fs::write(&source, b"contents").expect("write library file");
                PlanItem {
                    source,
                    destination: SafeRelativePath::parse(&format!("job-1/Show/{name}"))
                        .expect("valid destination"),
                    length: 8,
                }
            })
            .collect();
        let plan = MaterializationPlan { items };

        fixture
            .staging
            .materialize(
                &plan,
                &[
                    MaterializationStrategy::Reflink,
                    MaterializationStrategy::Copy,
                ],
            )
            .await
            .expect("materializes");

        assert_eq!(
            fixture.staging.probe_attempts.load(Ordering::Relaxed),
            1,
            "every file shares one (source device, staging device) pair; the probe must run once"
        );
    }

    #[tokio::test]
    async fn a_plan_larger_than_available_space_parks_and_writes_nothing() {
        let staging_dir = tempfile::tempdir().expect("tempdir");
        let library = tempfile::tempdir().expect("tempdir");
        let root = StagingRoot::new(
            staging_dir.path().to_path_buf(),
            &[library.path().to_path_buf()],
        )
        .expect("valid staging root");
        // An impossible margin forces the check to fail regardless of how
        // much space this machine actually has free.
        let staging = LocalStaging::new(root, u64::MAX);

        let source = library.path().join("e01.mkv");
        std::fs::write(&source, b"contents").expect("write library file");
        let plan = MaterializationPlan {
            items: vec![PlanItem {
                source,
                destination: SafeRelativePath::parse("job-1/Show/e01.mkv").expect("valid"),
                length: 8,
            }],
        };

        assert!(matches!(
            staging
                .materialize(&plan, &[MaterializationStrategy::Copy])
                .await,
            Err(StagingError::InsufficientSpace { .. })
        ));
        assert!(
            !plan.items[0]
                .destination
                .join_onto(staging_dir.path())
                .exists(),
            "an insufficient-space plan must not write anything"
        );
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
