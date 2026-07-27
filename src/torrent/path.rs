//! The path-traversal boundary.
//!
//! Torrent files contain attacker-influenced path components. Nothing in
//! SeedMedic joins a torrent-supplied path onto a real directory except through
//! [`SafeRelativePath`], whose only constructor rejects anything that could
//! escape, alias, or confuse the staging root.
//!
//! This type is *purely* syntactic. It cannot know about symlinks, so the
//! filesystem-level escape check lives next to the code that touches the disk:
//! `staging::domain::resolve_under`.

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// Longest single component we will create. Common Linux filesystems cap names
/// at 255 bytes; failing early gives a better error than `ENAMETOOLONG`.
const MAX_COMPONENT_BYTES: usize = 255;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum PathRejection {
    #[error("path has no components")]
    Empty,
    #[error("path component is empty")]
    EmptyComponent,
    #[error("absolute paths are not allowed")]
    Absolute,
    #[error("`..` components are not allowed")]
    ParentTraversal,
    #[error("`.` components are not allowed")]
    CurrentDirectory,
    #[error("component contains a path separator: {component:?}")]
    SeparatorInComponent { component: String },
    #[error("component contains a NUL or control byte: {component:?}")]
    ControlCharacter { component: String },
    #[error("component is {len} bytes, limit is {MAX_COMPONENT_BYTES}")]
    ComponentTooLong { len: usize },
}

/// A relative path that is safe to join onto a directory we own.
///
/// Stored `/`-separated. Ordering is deterministic so plans and audit records
/// compare stably.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SafeRelativePath(String);

impl SafeRelativePath {
    /// Build from the component list a `.torrent` gives us (`info.files[].path`).
    pub fn from_components<I, S>(components: I) -> Result<Self, PathRejection>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut parts = Vec::new();
        for component in components {
            let component = component.as_ref();
            validate_component(component)?;
            parts.push(component.to_owned());
        }

        if parts.is_empty() {
            return Err(PathRejection::Empty);
        }

        Ok(Self(parts.join("/")))
    }

    /// Build from an already-joined relative path such as `Season 01/ep.mkv`.
    pub fn parse(path: &str) -> Result<Self, PathRejection> {
        if path.starts_with('/') || path.starts_with('\\') {
            return Err(PathRejection::Absolute);
        }
        Self::from_components(path.split('/'))
    }

    /// Prefix this path with `component`, e.g. a torrent's root directory name.
    pub fn prefixed_with(&self, component: &str) -> Result<Self, PathRejection> {
        validate_component(component)?;
        Ok(Self(format!("{component}/{}", self.0)))
    }

    /// Nest `self` under `prefix`. Total, because both sides are already valid.
    pub fn under(&self, prefix: &Self) -> Self {
        Self(format!("{}/{}", prefix.0, self.0))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Join onto a directory. Safe by construction: every component was checked
    /// to be a plain name, so the result cannot leave `root` syntactically.
    pub fn join_onto(&self, root: &Path) -> PathBuf {
        root.join(&self.0)
    }

    /// Directory components, outermost first. Used to create parents one level
    /// at a time so each can be symlink-checked.
    pub fn parent_components(&self) -> impl Iterator<Item = &str> {
        let mut parts: Vec<&str> = self.0.split('/').collect();
        parts.pop();
        parts.into_iter()
    }
}

impl std::fmt::Display for SafeRelativePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl serde::Serialize for SafeRelativePath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Deserialisation re-runs validation. A path that reaches us through JSON —
/// an audit payload, a fake fixture, a future API request — gets exactly the
/// same scrutiny as one that came out of a `.torrent`.
impl<'de> serde::Deserialize<'de> for SafeRelativePath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

fn validate_component(component: &str) -> Result<(), PathRejection> {
    if component.is_empty() {
        return Err(PathRejection::EmptyComponent);
    }
    if component.len() > MAX_COMPONENT_BYTES {
        return Err(PathRejection::ComponentTooLong {
            len: component.len(),
        });
    }
    if component == ".." {
        return Err(PathRejection::ParentTraversal);
    }
    if component == "." {
        return Err(PathRejection::CurrentDirectory);
    }
    // Checked before the general separator rule so a rooted component reports
    // the more accurate reason.
    if component.starts_with('/') || component.starts_with('\\') {
        return Err(PathRejection::Absolute);
    }
    if component.contains('/') || component.contains('\\') {
        return Err(PathRejection::SeparatorInComponent {
            component: component.to_owned(),
        });
    }
    if component.chars().any(char::is_control) {
        return Err(PathRejection::ControlCharacter {
            component: component.to_owned(),
        });
    }

    // Belt and braces: whatever the string looked like, the OS must agree that
    // it is exactly one ordinary component.
    let mut components = Path::new(component).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => Ok(()),
        (Some(Component::RootDir), _) | (Some(Component::Prefix(_)), _) => {
            Err(PathRejection::Absolute)
        }
        (Some(Component::ParentDir), _) => Err(PathRejection::ParentTraversal),
        (Some(Component::CurDir), _) => Err(PathRejection::CurrentDirectory),
        _ => Err(PathRejection::SeparatorInComponent {
            component: component.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_nested_path() {
        let path = SafeRelativePath::from_components(["Season 01", "ep01.mkv"])
            .expect("ordinary path is accepted");
        assert_eq!(path.as_str(), "Season 01/ep01.mkv");
        assert_eq!(
            path.join_onto(Path::new("/staging/job-1")),
            PathBuf::from("/staging/job-1/Season 01/ep01.mkv")
        );
    }

    #[test]
    fn rejects_parent_traversal() {
        assert_eq!(
            SafeRelativePath::from_components(["..", "etc", "passwd"]),
            Err(PathRejection::ParentTraversal)
        );
        assert_eq!(
            SafeRelativePath::parse("media/../../etc/passwd"),
            Err(PathRejection::ParentTraversal)
        );
    }

    #[test]
    fn rejects_absolute_paths() {
        assert_eq!(
            SafeRelativePath::parse("/etc/passwd"),
            Err(PathRejection::Absolute)
        );
        assert_eq!(
            SafeRelativePath::from_components(["/etc"]),
            Err(PathRejection::Absolute)
        );
    }

    #[test]
    fn rejects_current_directory_component() {
        assert_eq!(
            SafeRelativePath::parse("./ep01.mkv"),
            Err(PathRejection::CurrentDirectory)
        );
    }

    #[test]
    fn rejects_empty_and_empty_components() {
        assert_eq!(
            SafeRelativePath::parse(""),
            Err(PathRejection::EmptyComponent)
        );
        assert_eq!(
            SafeRelativePath::parse("season//ep.mkv"),
            Err(PathRejection::EmptyComponent)
        );
        assert_eq!(
            SafeRelativePath::from_components(Vec::<String>::new()),
            Err(PathRejection::Empty)
        );
    }

    #[test]
    fn rejects_nul_and_control_bytes() {
        assert!(matches!(
            SafeRelativePath::from_components(["ep\0.mkv"]),
            Err(PathRejection::ControlCharacter { .. })
        ));
        assert!(matches!(
            SafeRelativePath::from_components(["ep\n.mkv"]),
            Err(PathRejection::ControlCharacter { .. })
        ));
    }

    #[test]
    fn rejects_separators_hidden_in_a_component() {
        // A tracker could send a single "component" that is really a subpath.
        assert!(matches!(
            SafeRelativePath::from_components(["season/../.."]),
            Err(PathRejection::SeparatorInComponent { .. })
        ));
        assert!(matches!(
            SafeRelativePath::from_components(["season\\ep.mkv"]),
            Err(PathRejection::SeparatorInComponent { .. })
        ));
    }

    #[test]
    fn rejects_overlong_components() {
        let long = "a".repeat(MAX_COMPONENT_BYTES + 1);
        assert_eq!(
            SafeRelativePath::from_components([long]),
            Err(PathRejection::ComponentTooLong {
                len: MAX_COMPONENT_BYTES + 1
            })
        );
    }

    #[test]
    fn prefixing_validates_the_new_component() {
        let path = SafeRelativePath::parse("ep01.mkv").expect("valid");
        assert_eq!(
            path.prefixed_with("Show S01")
                .expect("valid prefix")
                .as_str(),
            "Show S01/ep01.mkv"
        );
        assert_eq!(
            path.prefixed_with(".."),
            Err(PathRejection::ParentTraversal)
        );
    }

    #[test]
    fn parent_components_are_outermost_first() {
        let path = SafeRelativePath::parse("a/b/c.mkv").expect("valid");
        assert_eq!(path.parent_components().collect::<Vec<_>>(), vec!["a", "b"]);
    }
}
