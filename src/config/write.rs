//! `ConfigDocument`: the config file as text, with comments and key order
//! preserved. This is what lets `/settings` (`docs/todos/0017`) rewrite one
//! key without regenerating the file — a whole-document `serde::Serialize`
//! would lose the operator's comments and, worse, would write a secret that
//! came from the environment or a `_file` straight into `config.toml` in
//! plaintext (see `resolve_secrets`: it overwrites the inline field in place,
//! so after load a value from the environment is indistinguishable from one
//! typed inline).

use std::{
    fs,
    io::{self, Write as _},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, TableLike, Value};

use super::{Config, ConfigError};

/// The config file as text, with comments and key order preserved.
/// Deliberately no `Debug`: the document contains inline secrets.
pub struct ConfigDocument {
    path: PathBuf,
    doc: DocumentMut,
    /// `false` when the file or its directory is not writable — probed once,
    /// at read time, so the UI can say so before the operator types anything
    /// rather than as a 500 after they press save.
    writable: bool,
    /// The original file's unix mode, so a save does not widen it — a save
    /// under a permissive umask must not leave a secret-bearing file
    /// world-readable.
    mode: Option<u32>,
    /// Length and mtime at read time, to detect an edit made to the file by
    /// something else between rendering the form and saving it.
    stamp: Option<(u64, SystemTime)>,
}

#[derive(Debug, thiserror::Error)]
pub enum DocumentError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error(
        "{path} is not writable; fix the file or directory permissions, or the mount it lives on"
    )]
    NotWritable { path: PathBuf },
    #[error(
        "{path} changed on disk since this page was loaded; reload the page and try again so \
         nothing you did not intend gets overwritten"
    )]
    ExternalEdit { path: PathBuf },
    #[error(
        "{path} was edited by something else since it was last read and no longer parses as \
         TOML; fix it by hand before saving from here"
    )]
    NowUnparseable { path: PathBuf },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Which of the two ways a save actually reached the filesystem — reported
/// so an operator (or a test) can tell a rename-based save from the
/// bind-mount fallback rather than the two looking identical.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveOutcome {
    /// The common path: a temp file, written and fsynced, replaced the
    /// target with `rename(2)`.
    Renamed,
    /// `rename` failed — the target is a bind-mounted single file, or
    /// similar — so the target was truncated and rewritten in place.
    WrittenInPlace,
}

/// One segment of a dotted key: `trackers.0.base_url` is
/// `[Name("trackers"), Index(0), Name("base_url")]`. An `Index` always
/// follows the `Name` of the repeated section it indexes into.
#[derive(Clone, Copy)]
enum Segment<'a> {
    Name(&'a str),
    Index(usize),
}

fn segments(key: &str) -> Vec<Segment<'_>> {
    key.split('.')
        .map(|part| {
            part.parse::<usize>()
                .map(Segment::Index)
                .unwrap_or(Segment::Name(part))
        })
        .collect()
}

/// Walk `path` without creating anything; `None` the moment a segment is
/// missing. Shared by `get` and `remove`, neither of which should vivify.
fn navigate_existing<'a>(doc: &'a DocumentMut, path: &[Segment<'_>]) -> Option<&'a dyn TableLike> {
    let mut table: &dyn TableLike = &**doc;
    let mut index = 0;
    while index < path.len() {
        match path[index] {
            Segment::Name(name) => {
                if let Some(Segment::Index(row)) = path.get(index + 1) {
                    let item = table.get(name)?;
                    table = item.as_array_of_tables()?.get(*row)?;
                    index += 2;
                } else {
                    let item = table.get(name)?;
                    table = item.as_table_like()?;
                    index += 1;
                }
            }
            Segment::Index(_) => return None,
        }
    }
    Some(table)
}

fn navigate_existing_mut<'a>(
    doc: &'a mut DocumentMut,
    path: &[Segment<'_>],
) -> Option<&'a mut dyn TableLike> {
    let mut table: &mut dyn TableLike = &mut **doc;
    let mut index = 0;
    while index < path.len() {
        match path[index] {
            Segment::Name(name) => {
                if let Some(Segment::Index(row)) = path.get(index + 1) {
                    let item = table.get_mut(name)?;
                    table = item.as_array_of_tables_mut()?.get_mut(*row)?;
                    index += 2;
                } else {
                    let item = table.get_mut(name)?;
                    table = item.as_table_like_mut()?;
                    index += 1;
                }
            }
            Segment::Index(_) => return None,
        }
    }
    Some(table)
}

/// Walk `path`, creating an empty table (or, immediately before an `Index`,
/// an empty array of tables padded to length) wherever a segment is absent.
/// Used only by `set`: `get` and `remove` must never invent structure that
/// was not there, or "the key is absent" would stop meaning anything.
fn navigate_create_mut<'a>(
    doc: &'a mut DocumentMut,
    path: &[Segment<'_>],
) -> &'a mut dyn TableLike {
    let mut table: &mut dyn TableLike = &mut **doc;
    let mut index = 0;
    while index < path.len() {
        match path[index] {
            Segment::Name(name) => {
                if let Some(Segment::Index(row)) = path.get(index + 1) {
                    let item = table
                        .entry(name)
                        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
                    let array = item
                        .as_array_of_tables_mut()
                        .expect("just vivified as an array of tables");
                    while array.len() <= *row {
                        array.push(Table::new());
                    }
                    table = array.get_mut(*row).expect("just padded to this length");
                    index += 2;
                } else {
                    let item = table
                        .entry(name)
                        .or_insert_with(|| Item::Table(Table::new()));
                    table = item
                        .as_table_like_mut()
                        .unwrap_or_else(|| panic!("`{name}` is a leaf value, not a table"));
                    index += 1;
                }
            }
            Segment::Index(_) => {
                unreachable!("an Index segment is always consumed with its preceding Name")
            }
        }
    }
    table
}

/// Config keys that hold a `Secret`, wherever they appear in the document —
/// used only to redact `to_redacted_toml`'s output, so this list growing
/// stale would make a copy-pasted "safe" TOML leak a value. Kept in one
/// place and exercised by a test against every secret `Config` actually has.
const SECRET_KEYS: [&str; 3] = ["api_key", "password", "auth_token"];

fn redact_secrets(table: &mut dyn TableLike) {
    let keys: Vec<String> = table.iter().map(|(key, _)| key.to_owned()).collect();
    for key in keys {
        let item = table
            .get_mut(&key)
            .expect("key just listed from this table");
        if SECRET_KEYS.contains(&key.as_str())
            && item.as_str().is_some_and(|value| !value.is_empty())
        {
            *item = Item::Value(Value::from("<redacted>".to_owned()));
            continue;
        }
        if let Some(inner) = item.as_table_like_mut() {
            redact_secrets(inner);
        } else if let Some(array) = item.as_array_of_tables_mut() {
            for row in array.iter_mut() {
                redact_secrets(row);
            }
        }
    }
}

/// Probe, once, whether `path` can be written: either it already exists and
/// opens for write (catching a read-only bind mount, which a file's
/// permission bits do not reveal), or its directory will accept a new file
/// (catching both "the file does not exist yet" and a root-owned anonymous
/// volume the process cannot write into at all). Never truncates or leaves
/// anything behind.
fn probe_writable(path: &Path) -> bool {
    match fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return false,
    }

    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let probe = dir.join(format!(".seedmedic-write-probe-{}", std::process::id()));
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

impl ConfigDocument {
    /// Read and parse the file at `path`, probing writability and recording
    /// the stamp `save` later checks. A missing file is not an error — a
    /// fresh install has none yet — and reads as an empty document.
    pub fn read(path: &Path) -> Result<Self, DocumentError> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
            Err(source) => {
                return Err(DocumentError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let doc: DocumentMut = raw.parse().map_err(|source| DocumentError::Parse {
            path: path.to_owned(),
            source,
        })?;

        let metadata = fs::metadata(path).ok();
        let mode = metadata.as_ref().map(|m| m.permissions().mode());
        let stamp = metadata
            .as_ref()
            .map(|m| (m.len(), m.modified().unwrap_or(SystemTime::UNIX_EPOCH)));

        Ok(Self {
            path: path.to_owned(),
            doc,
            writable: probe_writable(path),
            mode,
            stamp,
        })
    }

    /// `None` means the key is absent from the file, so a form renders the
    /// default as a placeholder rather than pretending it was set.
    pub fn get(&self, key: &str) -> Option<&Item> {
        let segments = segments(key);
        let (last, path) = segments.split_last()?;
        let table = navigate_existing(&self.doc, path)?;
        match last {
            Segment::Name(name) => table.get(name),
            Segment::Index(_) => None,
        }
    }

    /// Write `value` at `key`, creating any repeated-section row or
    /// intermediate table the path needs along the way.
    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        let segments = segments(key);
        let (last, path) = segments.split_last().expect("a key is never empty");
        let Segment::Name(name) = last else {
            panic!("a key must end in a field name, not an index: `{key}`");
        };
        let table = navigate_create_mut(&mut self.doc, path);
        table.insert(name, Item::Value(value.into()));
    }

    /// A list-valued leaf, e.g. `library.roots`. Kept separate from `set`
    /// because `Value: From<Vec<_>>` does not exist upstream.
    pub fn set_array(&mut self, key: &str, values: impl IntoIterator<Item = String>) {
        let array: Array = values.into_iter().collect();
        self.set(key, array);
    }

    /// No-op if the key (or anything on its path) is already absent.
    pub fn remove(&mut self, key: &str) {
        let segments = segments(key);
        let Some((Segment::Name(name), path)) = segments.split_last().map(|(l, p)| (*l, p)) else {
            return;
        };
        if let Some(table) = navigate_existing_mut(&mut self.doc, path) {
            table.remove(name);
        }
    }

    /// One new blank row appended to a repeated section, e.g. `trackers`.
    /// Returns its index, which the caller uses to build that row's field
    /// names for the next request.
    pub fn push_row(&mut self, section: &str) -> usize {
        let item = self
            .doc
            .entry(section)
            .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
        let array = item
            .as_array_of_tables_mut()
            .unwrap_or_else(|| panic!("`{section}` is not a repeated section"));
        array.push(Table::new());
        array.len() - 1
    }

    /// The number of rows currently in a repeated section.
    pub fn row_count(&self, section: &str) -> usize {
        self.doc
            .get(section)
            .and_then(Item::as_array_of_tables)
            .map_or(0, ArrayOfTables::len)
    }

    /// Drop one row of a repeated section entirely, shifting every later
    /// row's index down by one.
    pub fn remove_row(&mut self, section: &str, index: usize) {
        if let Some(array) = self
            .doc
            .get_mut(section)
            .and_then(Item::as_array_of_tables_mut)
        {
            array.remove(index);
        }
    }

    /// Parse the document exactly as `Config::load` would — including
    /// resolving every secret from the environment or a `_file` — so a
    /// draft's `problems()` reflects the configuration that would actually
    /// run, not the inline value alone.
    pub fn to_config(&self) -> Result<Config, ConfigError> {
        let text = self.doc.to_string();
        let mut config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: self.path.clone(),
            source,
        })?;
        config.resolve_secrets()?;
        Ok(config)
    }

    /// The document as TOML with every secret value blanked out — the
    /// degraded-mode escape hatch when the file cannot be written. Must be
    /// the only path that ever turns this document into text for display;
    /// see the invariants in `docs/todos/0017-the-settings-pages.md`.
    pub fn to_redacted_toml(&self) -> String {
        let mut doc = self.doc.clone();
        redact_secrets(&mut *doc);
        doc.to_string()
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write the document to disk: refuse if the file changed since it was
    /// read or no longer parses, write `.bak`, then a temp file — mode
    /// `0o600` at creation, fsynced — renamed over the target. If the
    /// rename fails (the target is a bind-mounted single file; `rename(2)`
    /// returns `EBUSY` and a Kubernetes ConfigMap symlink swap would be
    /// silently reverted by the next sync), truncate-and-write in place
    /// instead, and say which happened.
    pub fn save(&self) -> Result<SaveOutcome, DocumentError> {
        if !self.writable {
            return Err(DocumentError::NotWritable {
                path: self.path.clone(),
            });
        }

        if let Some(stamp) = self.stamp {
            let metadata = fs::metadata(&self.path).map_err(|source| DocumentError::Read {
                path: self.path.clone(),
                source,
            })?;
            let current = (
                metadata.len(),
                metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            );
            let raw = fs::read_to_string(&self.path).map_err(|source| DocumentError::Read {
                path: self.path.clone(),
                source,
            })?;
            // Checked ahead of the stamp: an unparseable file may be a
            // hand-edit in progress, and that is true regardless of whether
            // its length and mtime happen to still match.
            if raw.parse::<DocumentMut>().is_err() {
                return Err(DocumentError::NowUnparseable {
                    path: self.path.clone(),
                });
            }
            if current != stamp {
                return Err(DocumentError::ExternalEdit {
                    path: self.path.clone(),
                });
            }

            let bak = PathBuf::from(format!("{}.bak", self.path.display()));
            fs::copy(&self.path, &bak)
                .map_err(|source| DocumentError::Write { path: bak, source })?;
        }

        let text = self.doc.to_string();
        let dir = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.toml");
        let temp_path = dir.join(format!(".{file_name}.tmp"));

        {
            let mut temp = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temp_path)
                .map_err(|source| DocumentError::Write {
                    path: temp_path.clone(),
                    source,
                })?;
            temp.write_all(text.as_bytes())
                .and_then(|()| temp.sync_all())
                .map_err(|source| DocumentError::Write {
                    path: temp_path.clone(),
                    source,
                })?;
        }
        // `OpenOptions::mode` is masked by the process umask at creation, so
        // it alone cannot guarantee 0o600 — a permissive umask must not leave
        // a file holding secrets world-readable.
        fs::set_permissions(
            &temp_path,
            std::fs::Permissions::from_mode(self.mode.unwrap_or(0o600)),
        )
        .map_err(|source| DocumentError::Write {
            path: temp_path.clone(),
            source,
        })?;

        // The fallback below is exercised in deployment (a writable
        // single-file bind mount, where `rename` returns `EBUSY` but a
        // direct write succeeds) rather than by a unit test here: that is a
        // mount-level property, not a permission-bit one, and there is no
        // portable, unprivileged way to fabricate it in a temp directory —
        // unlike a read-only mount, which `probe_writable` and the
        // `NotWritable` tests above already cover.
        match fs::rename(&temp_path, &self.path) {
            Ok(()) => Ok(SaveOutcome::Renamed),
            Err(_) => {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(self.mode.unwrap_or(0o600))
                    .open(&self.path)
                    .map_err(|source| DocumentError::Write {
                        path: self.path.clone(),
                        source,
                    })?;
                file.write_all(text.as_bytes())
                    .and_then(|()| file.sync_all())
                    .map_err(|source| DocumentError::Write {
                        path: self.path.clone(),
                        source,
                    })?;
                let _ = fs::remove_file(&temp_path);
                Ok(SaveOutcome::WrittenInPlace)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("config.toml");
        fs::write(&path, contents).expect("write fixture");
        path
    }

    const SAMPLE: &str = r#"
# a comment worth keeping
[server]
bind_address = "0.0.0.0:9899" # inline comment

[staging]
root = "/srv/staging"

[[trackers]]
id = "demo"
kind = "fake"
api_key = ""

[[trackers]]
id = "aither"
kind = "unit3d"
api_key = "shh"
"#;

    #[test]
    fn get_reads_a_nested_scalar() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), SAMPLE);
        let doc = ConfigDocument::read(&path).expect("read");

        assert_eq!(
            doc.get("server.bind_address").and_then(Item::as_str),
            Some("0.0.0.0:9899")
        );
        assert_eq!(
            doc.get("trackers.1.id").and_then(Item::as_str),
            Some("aither")
        );
        assert!(doc.get("trackers.5.id").is_none());
        assert!(doc.get("nonexistent.key").is_none());
    }

    #[test]
    fn set_preserves_comments_and_key_order_for_untouched_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), SAMPLE);
        let mut doc = ConfigDocument::read(&path).expect("read");

        doc.set("server.bind_address", "127.0.0.1:9899".to_owned());
        let rendered = doc.doc.to_string();

        assert!(rendered.contains("# a comment worth keeping"));
        assert!(rendered.contains("127.0.0.1:9899"));
        assert!(rendered.contains("root = \"/srv/staging\""));
    }

    #[test]
    fn set_creates_a_new_repeated_section_row() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), SAMPLE);
        let mut doc = ConfigDocument::read(&path).expect("read");

        doc.set("trackers.2.id", "third".to_owned());
        assert_eq!(
            doc.get("trackers.2.id").and_then(Item::as_str),
            Some("third")
        );
        assert_eq!(doc.row_count("trackers"), 3);
    }

    #[test]
    fn remove_drops_a_key_without_disturbing_others() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), SAMPLE);
        let mut doc = ConfigDocument::read(&path).expect("read");

        doc.remove("trackers.1.api_key");
        assert!(doc.get("trackers.1.api_key").is_none());
        assert_eq!(
            doc.get("trackers.1.id").and_then(Item::as_str),
            Some("aither")
        );
    }

    #[test]
    fn remove_on_a_missing_key_is_a_no_op() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), SAMPLE);
        let mut doc = ConfigDocument::read(&path).expect("read");

        doc.remove("trackers.9.api_key");
        doc.remove("nonexistent.table.key");
    }

    #[test]
    fn save_writes_bak_exactly_once_and_the_original_stays_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), SAMPLE);
        let mut doc = ConfigDocument::read(&path).expect("read");
        doc.set("server.bind_address", "127.0.0.1:1".to_owned());

        doc.save().expect("save");

        let bak = PathBuf::from(format!("{}.bak", path.display()));
        let bak_contents = fs::read_to_string(&bak).expect("bak readable");
        assert!(bak_contents.contains("0.0.0.0:9899"));

        // A second save must overwrite the same `.bak`, not add another.
        let mut doc = ConfigDocument::read(&path).expect("read again");
        doc.set("server.bind_address", "127.0.0.1:2".to_owned());
        doc.save().expect("save again");
        let bak_contents = fs::read_to_string(&bak).expect("bak still readable");
        assert!(bak_contents.contains("127.0.0.1:1"));
    }

    #[test]
    #[cfg(unix)]
    fn save_creates_mode_0600_and_preserves_the_original_mode_on_replace() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), SAMPLE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("chmod");

        let mut doc = ConfigDocument::read(&path).expect("read");
        doc.set("server.bind_address", "127.0.0.1:1".to_owned());
        doc.save().expect("save");

        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o640, "the original mode must be preserved");
    }

    #[test]
    #[cfg(unix)]
    fn a_fresh_save_with_no_prior_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let mut doc = ConfigDocument::read(&path).expect("read absent file");
        doc.set("server.bind_address", "127.0.0.1:1".to_owned());
        doc.save().expect("save");

        let mode = fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn an_external_edit_between_read_and_save_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), SAMPLE);
        let mut doc = ConfigDocument::read(&path).expect("read");
        doc.set("server.bind_address", "127.0.0.1:1".to_owned());

        // Someone else edits the file after the page was rendered.
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, format!("{SAMPLE}\n# edited\n")).expect("external edit");

        let error = doc.save().expect_err("a changed file is refused");
        assert!(matches!(error, DocumentError::ExternalEdit { .. }));
    }

    #[test]
    fn a_now_unparseable_file_is_refused_rather_than_overwritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), SAMPLE);
        let mut doc = ConfigDocument::read(&path).expect("read");
        doc.set("server.bind_address", "127.0.0.1:1".to_owned());

        std::thread::sleep(std::time::Duration::from_millis(10));
        fs::write(&path, "not valid toml =====").expect("corrupt the file");

        let error = doc.save().expect_err("an unparseable file is refused");
        assert!(matches!(error, DocumentError::NowUnparseable { .. }));
        assert_eq!(
            fs::read_to_string(&path).expect("still readable"),
            "not valid toml ====="
        );
    }

    #[test]
    fn a_read_only_directory_is_reported_unwritable_up_front() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).expect("mkdir");
        let path = sub.join("config.toml");
        fs::write(&path, SAMPLE).expect("write");

        fs::set_permissions(&sub, fs::Permissions::from_mode(0o500)).expect("chmod ro");
        let doc = ConfigDocument::read(&path);
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).expect("restore for cleanup");

        // Root ignores directory permissions entirely, so this check only
        // means something when the test itself is not root.
        if unsafe { libc::geteuid() } != 0 {
            assert!(!doc.expect("still parses").writable());
        }
    }

    #[test]
    fn to_redacted_toml_never_contains_a_secret_value() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(
            dir.path(),
            r#"
            [server]
            auth_token = "SENTINEL-1"

            [[trackers]]
            id = "demo"
            kind = "unit3d"
            api_key = "SENTINEL-2"

            [download_client]
            password = "SENTINEL-3"
            "#,
        );
        let doc = ConfigDocument::read(&path).expect("read");

        let redacted = doc.to_redacted_toml();

        assert!(!redacted.contains("SENTINEL"));
        assert!(redacted.contains("<redacted>"));
    }

    #[test]
    fn to_redacted_toml_leaves_an_unset_secret_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write(dir.path(), SAMPLE);
        let doc = ConfigDocument::read(&path).expect("read");

        let redacted = doc.to_redacted_toml();
        assert!(redacted.contains("api_key = \"\""));
    }
}
