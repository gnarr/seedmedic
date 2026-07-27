//! Configuration and policy.
//!
//! One TOML file, validated once at startup, turned into plain values the rest
//! of the system can rely on. Anything that could make SeedMedic unsafe is
//! rejected here rather than defended against everywhere else.
//!
//! Secrets handling is deliberately minimal in the bootstrap — see
//! `docs/todos/0011-configuration-and-secrets.md` for `*_file` support, env
//! overrides, and redaction of the config dump.

use std::{
    collections::BTreeSet,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::{
    library::MatchConfidence,
    repair::{AutoResume, MaterializationPolicy, SafetyPolicy, WorkerConfig},
};

/// Where to look when `SEEDMEDIC_CONFIG` is not set.
const DEFAULT_PATH: &str = "config.toml";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// A value that must not end up in a log line.
#[derive(Clone, Default, Deserialize, Eq, PartialEq)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() {
            "Secret(unset)"
        } else {
            "Secret(***)"
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub staging: StagingConfig,
    pub library: LibraryConfig,
    pub policy: PolicyConfig,
    pub worker: WorkerSettings,
    pub trackers: Vec<TrackerConfig>,
    pub download_client: DownloadClientConfig,
    pub arr: Vec<ArrConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind_address: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0:9899".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    pub path: PathBuf,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("data/seedmedic.db"),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StagingConfig {
    /// Must be absolute, and must not overlap any library root.
    pub root: PathBuf,
    /// Free space to keep on the staging filesystem beyond what a plan needs.
    /// A plan that would eat into this margin parks for review instead of
    /// writing.
    pub min_free_bytes: u64,
}

impl Default for StagingConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::new(),
            min_free_bytes: 1 << 30, // 1 GiB
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LibraryConfig {
    /// Media library roots, scanned as a fallback candidate source and used to
    /// prove the staging area is somewhere else.
    pub roots: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    pub auto_resume: AutoResume,
    pub min_match_confidence: MatchConfidence,
    /// Pieces hashed per file to confirm a match. `0` disables verification.
    pub verification_pieces: usize,
    pub prefer_reflink: bool,
    /// Hardlinks make the staged file *be* the library file. Off by default.
    pub allow_hardlink: bool,
    pub allow_copy: bool,
    pub max_attempts: u32,
    pub retry_base_seconds: u64,
    pub retry_max_seconds: u64,
    pub recheck_poll_seconds: u64,
    pub tracker_poll_seconds: u64,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        let policy = SafetyPolicy::default();
        Self {
            auto_resume: policy.auto_resume,
            min_match_confidence: policy.min_match_confidence,
            verification_pieces: policy.verification_pieces,
            prefer_reflink: policy.materialization.prefer_reflink,
            allow_hardlink: policy.materialization.allow_hardlink,
            allow_copy: policy.materialization.allow_copy,
            max_attempts: policy.max_attempts,
            retry_base_seconds: policy.retry_base_delay.as_secs(),
            retry_max_seconds: policy.retry_max_delay.as_secs(),
            recheck_poll_seconds: policy.recheck_poll_interval.as_secs(),
            tracker_poll_seconds: policy.tracker_poll_interval.as_secs(),
        }
    }
}

impl PolicyConfig {
    pub fn to_policy(&self) -> SafetyPolicy {
        SafetyPolicy {
            auto_resume: self.auto_resume,
            min_match_confidence: self.min_match_confidence,
            verification_pieces: self.verification_pieces,
            materialization: MaterializationPolicy {
                prefer_reflink: self.prefer_reflink,
                allow_hardlink: self.allow_hardlink,
                allow_copy: self.allow_copy,
            },
            max_attempts: self.max_attempts,
            retry_base_delay: Duration::from_secs(self.retry_base_seconds),
            retry_max_delay: Duration::from_secs(self.retry_max_seconds),
            recheck_poll_interval: Duration::from_secs(self.recheck_poll_seconds),
            tracker_poll_interval: Duration::from_secs(self.tracker_poll_seconds),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkerSettings {
    pub owner: String,
    pub lease_seconds: u64,
    pub batch_size: i64,
    pub poll_interval_seconds: u64,
    pub discovery_interval_seconds: u64,
}

impl Default for WorkerSettings {
    fn default() -> Self {
        let worker = WorkerConfig::default();
        Self {
            owner: worker.owner,
            lease_seconds: worker.lease.as_secs(),
            batch_size: worker.batch_size,
            poll_interval_seconds: worker.poll_interval.as_secs(),
            discovery_interval_seconds: worker.discovery_interval.as_secs(),
        }
    }
}

impl WorkerSettings {
    pub fn to_worker_config(&self) -> WorkerConfig {
        WorkerConfig {
            owner: self.owner.clone(),
            lease: Duration::from_secs(self.lease_seconds),
            batch_size: self.batch_size,
            poll_interval: Duration::from_secs(self.poll_interval_seconds),
            discovery_interval: Duration::from_secs(self.discovery_interval_seconds),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TrackerKind {
    Unit3d,
    /// In-memory tracker with two demo warnings. Requires the `fakes` feature.
    Fake,
}

/// Where the Unit3D API key goes. Instances in the family disagree.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TokenPlacement {
    /// `Authorization: Bearer <token>`.
    #[default]
    Header,
    /// `?api_token=<token>`, appended to every request.
    Query,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerConfig {
    /// Stable key repair jobs are filed under. Changing it orphans existing jobs.
    pub id: String,
    pub kind: TrackerKind,
    #[serde(default = "placeholder_url")]
    pub base_url: Url,
    #[serde(default)]
    pub api_key: Secret,
    #[serde(default)]
    pub token_placement: TokenPlacement,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DownloadClientKind {
    #[default]
    QBittorrent,
    /// In-memory client. Requires the `fakes` feature.
    Fake,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownloadClientConfig {
    pub kind: DownloadClientKind,
    pub base_url: Url,
    pub username: String,
    pub password: Secret,
    /// Category to file repaired torrents under, so they are recognisable.
    pub category: Option<String>,
}

impl Default for DownloadClientConfig {
    fn default() -> Self {
        Self {
            kind: DownloadClientKind::default(),
            base_url: placeholder_url(),
            username: String::new(),
            password: Secret::default(),
            category: Some("seedmedic".to_owned()),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ArrKind {
    Sonarr,
    Radarr,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArrConfig {
    pub kind: ArrKind,
    pub name: String,
    pub base_url: Url,
    #[serde(default)]
    pub api_key: Secret,
    /// Rewrites paths the *arr reports (as its container sees them) to the
    /// paths SeedMedic sees. Per-instance, since two instances can run in
    /// containers with different mounts.
    #[serde(default)]
    pub path_mappings: Vec<PathMappingConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathMappingConfig {
    pub from: PathBuf,
    pub to: PathBuf,
}

fn placeholder_url() -> Url {
    Url::parse("http://localhost").expect("literal URL")
}

impl Config {
    /// Load from `SEEDMEDIC_CONFIG`, or `./config.toml`.
    pub fn load() -> Result<Self, ConfigError> {
        let path = std::env::var_os("SEEDMEDIC_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_PATH));
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let config: Self = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Reject anything that would make the system unsafe or useless. Cheap
    /// checks only — anything needing the filesystem happens when the staging
    /// root is built.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: String| Err(ConfigError::Invalid(message));

        self.server
            .bind_address
            .parse::<SocketAddr>()
            .map_err(|error| {
                ConfigError::Invalid(format!(
                    "server.bind_address `{}` is not a socket address: {error}",
                    self.server.bind_address
                ))
            })?;

        if self.trackers.is_empty() {
            return invalid("at least one [[trackers]] entry is required".to_owned());
        }

        let mut seen = BTreeSet::new();
        for tracker in &self.trackers {
            if tracker.id.trim().is_empty() {
                return invalid("every tracker needs a non-empty `id`".to_owned());
            }
            if !seen.insert(tracker.id.as_str()) {
                return invalid(format!("tracker id `{}` is used twice", tracker.id));
            }
        }

        if self.staging.root.as_os_str().is_empty() {
            return invalid("staging.root is required".to_owned());
        }
        if !self.staging.root.is_absolute() {
            return invalid(format!(
                "staging.root `{}` must be an absolute path",
                self.staging.root.display()
            ));
        }

        for root in &self.library.roots {
            if !root.is_absolute() {
                return invalid(format!(
                    "library root `{}` must be an absolute path",
                    root.display()
                ));
            }
        }

        // Permitting nothing is not a safety setting, it is a repair that can
        // never happen. Say so at startup rather than on the first job.
        if !(self.policy.prefer_reflink || self.policy.allow_hardlink || self.policy.allow_copy) {
            return invalid(
                "policy permits no materialization strategy; enable at least one of \
                 prefer_reflink, allow_copy, allow_hardlink"
                    .to_owned(),
            );
        }

        if self.policy.max_attempts == 0 {
            return invalid("policy.max_attempts must be at least 1".to_owned());
        }
        if self.worker.batch_size < 1 {
            return invalid("worker.batch_size must be at least 1".to_owned());
        }
        if self.worker.lease_seconds == 0 {
            return invalid("worker.lease_seconds must be at least 1".to_owned());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [staging]
        root = "/srv/seedmedic/staging"

        [[trackers]]
        id = "example"
        kind = "fake"
    "#;

    fn parse(toml_text: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(toml_text).expect("test config parses");
        config.validate().map(|()| config)
    }

    #[test]
    fn a_minimal_config_is_valid_and_conservative() {
        let config = parse(MINIMAL).expect("valid");

        let policy = config.policy.to_policy();
        assert_eq!(policy.auto_resume, AutoResume::Never);
        assert_eq!(policy.min_match_confidence, MatchConfidence::Probable);
        assert!(!policy.materialization.allow_hardlink);
        assert!(policy.materialization.prefer_reflink);
    }

    #[test]
    fn secrets_do_not_print_themselves() {
        let secret = Secret("hunter2".to_owned());
        assert_eq!(format!("{secret:?}"), "Secret(***)");
        assert!(!format!("{secret:?}").contains("hunter2"));
    }

    #[test]
    fn a_relative_staging_root_is_rejected() {
        let config = MINIMAL.replace("/srv/seedmedic/staging", "staging");
        assert!(parse(&config).is_err());
    }

    #[test]
    fn duplicate_tracker_ids_are_rejected() {
        let config = format!("{MINIMAL}\n[[trackers]]\nid = \"example\"\nkind = \"fake\"\n");
        assert!(parse(&config).is_err());
    }

    #[test]
    fn no_trackers_is_rejected() {
        assert!(parse("[staging]\nroot = \"/srv/staging\"\n").is_err());
    }

    #[test]
    fn permitting_no_materialization_strategy_is_rejected() {
        let config = format!(
            "{MINIMAL}\n[policy]\nprefer_reflink = false\nallow_hardlink = false\nallow_copy = false\n"
        );
        assert!(parse(&config).is_err());
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_silently_ignored() {
        let config = format!("{MINIMAL}\n[policy]\nauto_resum = \"never\"\n");
        assert!(toml::from_str::<Config>(&config).is_err());
    }

    #[test]
    fn the_example_config_is_valid() {
        let example = include_str!("../config.example.toml");
        let config: Config = toml::from_str(example).expect("example config parses");
        config.validate().expect("example config is valid");
    }
}
