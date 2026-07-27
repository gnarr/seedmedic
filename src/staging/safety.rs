//! Filesystem-level escape checks.
//!
//! [`SafeRelativePath`](crate::torrent::SafeRelativePath) guarantees a path is
//! syntactically contained. That is not enough on a real filesystem: a symlink
//! anywhere along the way turns a contained path into an arbitrary write. Every
//! staging operation resolves its destination through [`resolve_under`] first.

use std::path::{Path, PathBuf};

use crate::torrent::SafeRelativePath;

use super::ports::StagingError;

/// Resolve `path` under `root`, refusing if any component that already exists
/// is a symlink.
///
/// Checked outermost-in, so a symlink is caught before anything is created
/// beneath it. Components that do not exist yet are fine — they will be created
/// as real directories by [`create_directories`].
pub fn resolve_under(root: &Path, path: &SafeRelativePath) -> Result<PathBuf, StagingError> {
    let mut current = root.to_path_buf();
    reject_if_symlink(&current)?;

    for component in path.parent_components() {
        current.push(component);
        reject_if_symlink(&current)?;
    }

    Ok(path.join_onto(root))
}

/// Create every parent directory of `path`, checking each level for symlinks as
/// it goes. Not `create_dir_all`: that would happily descend through a symlink
/// planted between the check and the create.
pub fn create_directories(root: &Path, path: &SafeRelativePath) -> Result<(), StagingError> {
    let mut current = root.to_path_buf();
    for component in path.parent_components() {
        current.push(component);
        match std::fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(StagingError::Io(format!(
                    "cannot create {}: {error}",
                    current.display()
                )));
            }
        }
        reject_if_symlink(&current)?;
    }
    Ok(())
}

fn reject_if_symlink(path: &Path) -> Result<(), StagingError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_symlink() => Err(StagingError::UnsafePath {
            path: path.to_path_buf(),
            reason: "path component is a symlink",
        }),
        Ok(_) => Ok(()),
        // Does not exist yet: nothing to escape through.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StagingError::Io(format!(
            "cannot stat {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_an_ordinary_path() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = SafeRelativePath::parse("job-1/Show/e01.mkv").expect("valid");

        assert_eq!(
            resolve_under(root.path(), &path).expect("resolves"),
            root.path().join("job-1/Show/e01.mkv")
        );
    }

    #[test]
    fn refuses_to_resolve_through_a_symlinked_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let elsewhere = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("job-1")).expect("symlink");

        let path = SafeRelativePath::parse("job-1/e01.mkv").expect("valid");

        assert!(matches!(
            resolve_under(root.path(), &path),
            Err(StagingError::UnsafePath { .. })
        ));
    }

    #[test]
    fn creating_directories_stops_at_a_symlink() {
        let root = tempfile::tempdir().expect("tempdir");
        let elsewhere = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("job-1")).expect("symlink");

        let path = SafeRelativePath::parse("job-1/Show/e01.mkv").expect("valid");

        assert!(matches!(
            create_directories(root.path(), &path),
            Err(StagingError::UnsafePath { .. })
        ));
        assert!(
            !elsewhere.path().join("Show").exists(),
            "nothing may be created outside the staging root"
        );
    }

    #[test]
    fn creating_directories_is_idempotent() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = SafeRelativePath::parse("job-1/Show/e01.mkv").expect("valid");

        create_directories(root.path(), &path).expect("first call");
        create_directories(root.path(), &path).expect("second call");

        assert!(root.path().join("job-1/Show").is_dir());
    }
}
