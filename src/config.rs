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

/// Resolve one secret from its three possible sources.
///
/// Precedence: the environment variable, then `_file`, then the inline TOML
/// value. A `_file` secret is trimmed of trailing newlines — the classic
/// footgun from writing it with `echo` instead of `printf`.
fn resolve_secret(
    inline: &Secret,
    file: Option<&Path>,
    env_var: &str,
) -> Result<Secret, ConfigError> {
    if let Ok(value) = std::env::var(env_var) {
        return Ok(Secret::new(value));
    }
    if let Some(path) = file {
        let contents = std::fs::read_to_string(path).map_err(|source| {
            ConfigError::Invalid(format!(
                "cannot read secret file {} (set via a `_file` setting): {source}",
                path.display()
            ))
        })?;
        return Ok(Secret::new(contents.trim_end_matches(['\n', '\r'])));
    }
    Ok(inline.clone())
}

/// Turn an operator-chosen id into the shouty-snake-case fragment of an
/// environment variable name: uppercased, every non-alphanumeric character
/// replaced with `_`.
fn shouty(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
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
    pub metrics: MetricsConfig,
    pub notifications: NotificationsConfig,
}

/// Off by default: most self-hosted users never scrape this. Also requires
/// the crate's `metrics` feature — see `docs/todos/0012-observability.md`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    pub enabled: bool,
}

/// A generic webhook (Apprise-compatible, or plain JSON POST) for: parked for
/// review, completed, tracker unreachable for a while. Off by default: unset
/// `webhook_url` disables notifications entirely, and every send is
/// fire-and-forget — a failure is logged, never retried, and never changes
/// what the worker does next.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationsConfig {
    pub webhook_url: Option<Url>,
    pub tracker_unreachable_after_seconds: u64,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            webhook_url: None,
            tracker_unreachable_after_seconds: 1800,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub bind_address: String,
    /// If set, every request must present it as `Authorization: Bearer
    /// <token>` or be rejected. The web UI has no accounts or roles — this is
    /// a single shared secret, not a login system. Overridden by
    /// `SEEDMEDIC_SERVER_AUTH_TOKEN`, if set.
    #[serde(default)]
    pub auth_token: Secret,
    #[serde(default)]
    pub auth_token_file: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0:9899".to_owned(),
            auth_token: Secret::default(),
            auth_token_file: None,
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
    /// Floor of the adaptive recheck poll backoff.
    pub recheck_poll_seconds: u64,
    /// Cap of the adaptive recheck poll backoff, and the interval used while
    /// a check is queued rather than running.
    pub recheck_poll_max_seconds: u64,
    /// A recheck running longer than this parks the job for review instead
    /// of polling forever. Four hours by default — generous for a 100 GB
    /// torrent on spinning rust, short enough that a genuinely stuck check
    /// does not sit unnoticed for days.
    pub recheck_timeout_seconds: u64,
    pub tracker_poll_seconds: u64,
    /// Floor of the adaptive tracker-poll backoff as a hit-and-run deadline
    /// approaches — a private tracker still bans for hammering.
    pub tracker_poll_min_seconds: u64,
    /// Consecutive `Unknown` tracker answers before a seeding job parks for
    /// review instead of polling forever.
    pub max_consecutive_unknown_tracker_status: u32,
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
            recheck_poll_max_seconds: policy.recheck_poll_max_interval.as_secs(),
            recheck_timeout_seconds: policy.recheck_timeout.as_secs(),
            tracker_poll_seconds: policy.tracker_poll_interval.as_secs(),
            tracker_poll_min_seconds: policy.tracker_poll_min_interval.as_secs(),
            max_consecutive_unknown_tracker_status: policy.max_consecutive_unknown_tracker_status,
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
            recheck_poll_max_interval: Duration::from_secs(self.recheck_poll_max_seconds),
            recheck_timeout: Duration::from_secs(self.recheck_timeout_seconds),
            tracker_poll_interval: Duration::from_secs(self.tracker_poll_seconds),
            tracker_poll_min_interval: Duration::from_secs(self.tracker_poll_min_seconds),
            max_consecutive_unknown_tracker_status: self.max_consecutive_unknown_tracker_status,
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
    /// Read `api_key` from a file instead — e.g. a mounted Docker/Kubernetes
    /// secret. Overridden by `SEEDMEDIC_TRACKER_<ID>_API_KEY`, if set.
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
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
    /// Read `password` from a file instead. Overridden by
    /// `SEEDMEDIC_DOWNLOAD_CLIENT_PASSWORD`, if set.
    pub password_file: Option<PathBuf>,
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
            password_file: None,
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
    /// Read `api_key` from a file instead. Overridden by
    /// `SEEDMEDIC_ARR_<NAME>_API_KEY`, if set.
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
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
        let mut config: Self = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.resolve_secrets()?;
        config.validate()?;
        for warning in config.validate_runtime()? {
            tracing::warn!("{warning}");
        }
        Ok(config)
    }

    /// Resolve every secret from environment, `_file`, or inline TOML, in
    /// that precedence order.
    fn resolve_secrets(&mut self) -> Result<(), ConfigError> {
        self.download_client.password = resolve_secret(
            &self.download_client.password,
            self.download_client.password_file.as_deref(),
            "SEEDMEDIC_DOWNLOAD_CLIENT_PASSWORD",
        )?;
        self.server.auth_token = resolve_secret(
            &self.server.auth_token,
            self.server.auth_token_file.as_deref(),
            "SEEDMEDIC_SERVER_AUTH_TOKEN",
        )?;
        for tracker in &mut self.trackers {
            let env_var = format!("SEEDMEDIC_TRACKER_{}_API_KEY", shouty(&tracker.id));
            tracker.api_key =
                resolve_secret(&tracker.api_key, tracker.api_key_file.as_deref(), &env_var)?;
        }
        for arr in &mut self.arr {
            let env_var = format!("SEEDMEDIC_ARR_{}_API_KEY", shouty(&arr.name));
            arr.api_key = resolve_secret(&arr.api_key, arr.api_key_file.as_deref(), &env_var)?;
        }
        Ok(())
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
        if self.policy.recheck_poll_max_seconds < self.policy.recheck_poll_seconds {
            return invalid(
                "policy.recheck_poll_max_seconds must be at least recheck_poll_seconds".to_owned(),
            );
        }
        if self.policy.recheck_timeout_seconds == 0 {
            return invalid("policy.recheck_timeout_seconds must be at least 1".to_owned());
        }
        if self.policy.max_consecutive_unknown_tracker_status == 0 {
            return invalid(
                "policy.max_consecutive_unknown_tracker_status must be at least 1".to_owned(),
            );
        }
        if self.policy.tracker_poll_min_seconds > self.policy.tracker_poll_seconds {
            return invalid(
                "policy.tracker_poll_min_seconds must be at most tracker_poll_seconds".to_owned(),
            );
        }
        if self.worker.batch_size < 1 {
            return invalid("worker.batch_size must be at least 1".to_owned());
        }
        if self.worker.lease_seconds == 0 {
            return invalid("worker.lease_seconds must be at least 1".to_owned());
        }
        if self.worker.owner.trim().is_empty() {
            return invalid("worker.owner must not be empty".to_owned());
        }

        // A private tracker still bans for hammering, regardless of how the
        // interval got set this low.
        const MIN_TRACKER_POLL_SECONDS: u64 = 60;
        if self.policy.tracker_poll_seconds < MIN_TRACKER_POLL_SECONDS {
            return invalid(format!(
                "policy.tracker_poll_seconds ({}) is below the minimum of \
                 {MIN_TRACKER_POLL_SECONDS}s; polling a private tracker this often risks a ban",
                self.policy.tracker_poll_seconds
            ));
        }

        Ok(())
    }

    /// Deeper checks that need the filesystem but must never write to it,
    /// touch the network, or open the database — safe to run via
    /// `--check-config` against a production config on a laptop.
    ///
    /// Returns non-fatal warnings on success; a configuration that cannot
    /// work at all is an `Err`.
    pub fn validate_runtime(&self) -> Result<Vec<String>, ConfigError> {
        let invalid = |message: String| Err(ConfigError::Invalid(message));
        let mut warnings = Vec::new();

        for tracker in &self.trackers {
            if tracker.kind == TrackerKind::Unit3d && tracker.api_key.is_empty() {
                return invalid(format!(
                    "tracker `{}` is a unit3d tracker and needs an api_key, set inline, via \
                     api_key_file, or via SEEDMEDIC_TRACKER_{}_API_KEY",
                    tracker.id,
                    shouty(&tracker.id)
                ));
            }
        }

        for arr in &self.arr {
            if arr.api_key.is_empty() {
                return invalid(format!(
                    "arr instance `{}` needs an api_key, set inline, via api_key_file, or via \
                     SEEDMEDIC_ARR_{}_API_KEY",
                    arr.name,
                    shouty(&arr.name)
                ));
            }
        }

        for root in &self.library.roots {
            if let Err(error) = std::fs::read_dir(root) {
                return invalid(format!(
                    "library root `{}` is not a readable directory: {error}",
                    root.display()
                ));
            }
        }

        if !self.staging.root.as_os_str().is_empty() {
            crate::staging::StagingRoot::check_overlap(&self.staging.root, &self.library.roots)
                .map_err(|error| ConfigError::Invalid(error.to_string()))?;

            let ancestor = nearest_existing_ancestor(&self.staging.root);
            if !directory_is_writable(&ancestor) {
                return invalid(format!(
                    "staging.root `{}` is not writable: `{}` denies write access",
                    self.staging.root.display(),
                    ancestor.display()
                ));
            }
        }

        if self.policy.min_match_confidence == MatchConfidence::Exact {
            warnings.push(
                "policy.min_match_confidence = \"exact\" requires piece-verified matches; \
                 until docs/todos/0005-media-matching.md lands, no candidate can reach that \
                 confidence, so every repair will park for review"
                    .to_owned(),
            );
        }

        if self.metrics.enabled && !cfg!(feature = "metrics") {
            warnings.push(
                "metrics.enabled = true, but this build does not have the `metrics` feature; \
                 no metrics will be collected"
                    .to_owned(),
            );
        }

        Ok(warnings)
    }

    /// A human-readable rendering of the effective configuration for
    /// `--check-config`, with every secret replaced by whether it is set.
    /// Never calls `Secret::expose`.
    pub fn redacted_summary(&self) -> String {
        use std::fmt::Write;

        fn secret_state(secret: &Secret) -> &'static str {
            if secret.is_empty() { "unset" } else { "set" }
        }

        let mut out = String::new();
        let roots = self
            .library
            .roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");

        writeln!(out, "server.bind_address = {}", self.server.bind_address).unwrap();
        writeln!(
            out,
            "server.auth_token = {}",
            secret_state(&self.server.auth_token)
        )
        .unwrap();
        writeln!(out, "database.path = {}", self.database.path.display()).unwrap();
        writeln!(out, "staging.root = {}", self.staging.root.display()).unwrap();
        writeln!(
            out,
            "staging.min_free_bytes = {}",
            self.staging.min_free_bytes
        )
        .unwrap();
        writeln!(out, "library.roots = [{roots}]").unwrap();
        writeln!(out, "policy.auto_resume = {:?}", self.policy.auto_resume).unwrap();
        writeln!(
            out,
            "policy.min_match_confidence = {:?}",
            self.policy.min_match_confidence
        )
        .unwrap();
        writeln!(
            out,
            "policy.verification_pieces = {}",
            self.policy.verification_pieces
        )
        .unwrap();
        writeln!(
            out,
            "policy.prefer_reflink = {}, allow_hardlink = {}, allow_copy = {}",
            self.policy.prefer_reflink, self.policy.allow_hardlink, self.policy.allow_copy
        )
        .unwrap();
        writeln!(
            out,
            "policy.tracker_poll_seconds = {}",
            self.policy.tracker_poll_seconds
        )
        .unwrap();
        writeln!(out, "worker.owner = {}", self.worker.owner).unwrap();
        writeln!(out, "worker.batch_size = {}", self.worker.batch_size).unwrap();

        for tracker in &self.trackers {
            writeln!(
                out,
                "[[trackers]] id={} kind={:?} base_url={} token_placement={:?} api_key={}",
                tracker.id,
                tracker.kind,
                tracker.base_url,
                tracker.token_placement,
                secret_state(&tracker.api_key)
            )
            .unwrap();
        }

        writeln!(
            out,
            "download_client kind={:?} base_url={} username={:?} password={} category={:?}",
            self.download_client.kind,
            self.download_client.base_url,
            self.download_client.username,
            secret_state(&self.download_client.password),
            self.download_client.category
        )
        .unwrap();

        for arr in &self.arr {
            writeln!(
                out,
                "[[arr]] kind={:?} name={} base_url={} api_key={}",
                arr.kind,
                arr.name,
                arr.base_url,
                secret_state(&arr.api_key)
            )
            .unwrap();
        }

        writeln!(out, "metrics.enabled = {}", self.metrics.enabled).unwrap();
        writeln!(
            out,
            "notifications.webhook_url = {}",
            if self.notifications.webhook_url.is_some() {
                "set"
            } else {
                "unset"
            }
        )
        .unwrap();

        out
    }
}

/// Walk upward until an existing path is found. `staging.root` itself if it
/// already exists, otherwise the nearest ancestor `create_dir_all` would need
/// permission on.
fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path;
    loop {
        if current.exists() {
            return current.to_path_buf();
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return current.to_path_buf(),
        }
    }
}

#[cfg(unix)]
fn directory_is_writable(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let mode = metadata.mode();
    // SAFETY: geteuid/getegid read process credentials; they do not touch
    // the filesystem or take any argument that could be invalid.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };

    if uid == 0 {
        return true;
    }
    if metadata.uid() == uid {
        return mode & 0o200 != 0;
    }
    if metadata.gid() == gid {
        return mode & 0o020 != 0;
    }
    mode & 0o002 != 0
}

#[cfg(not(unix))]
fn directory_is_writable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| !metadata.permissions().readonly())
        .unwrap_or(false)
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
    fn a_poll_cap_below_the_base_interval_is_rejected() {
        let config = format!(
            "{MINIMAL}\n[policy]\nrecheck_poll_seconds = 60\nrecheck_poll_max_seconds = 30\n"
        );
        assert!(parse(&config).is_err());
    }

    #[test]
    fn a_tracker_poll_floor_above_the_ceiling_is_rejected() {
        let config = format!(
            "{MINIMAL}\n[policy]\ntracker_poll_seconds = 60\ntracker_poll_min_seconds = 120\n"
        );
        assert!(parse(&config).is_err());
    }

    #[test]
    fn a_zero_unknown_tracker_status_threshold_is_rejected() {
        let config = format!("{MINIMAL}\n[policy]\nmax_consecutive_unknown_tracker_status = 0\n");
        assert!(parse(&config).is_err());
    }

    #[test]
    fn a_zero_recheck_timeout_is_rejected() {
        let config = format!("{MINIMAL}\n[policy]\nrecheck_timeout_seconds = 0\n");
        assert!(parse(&config).is_err());
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

    fn config_with_tracker(id: &str, extra_tracker_fields: &str) -> String {
        format!(
            r#"
            [staging]
            root = "/srv/seedmedic/staging"

            [[trackers]]
            id = "{id}"
            kind = "fake"
            {extra_tracker_fields}
            "#
        )
    }

    #[test]
    fn a_secret_env_var_wins_over_file_and_inline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("api_key");
        std::fs::write(&file, "from-file").expect("write secret file");

        let toml_text = config_with_tracker(
            "env-wins",
            &format!(
                "api_key = \"from-inline\"\napi_key_file = \"{}\"",
                file.display()
            ),
        );
        let mut config: Config = toml::from_str(&toml_text).expect("parses");

        // SAFETY: this test does not spawn other threads that read the
        // environment concurrently.
        unsafe { std::env::set_var("SEEDMEDIC_TRACKER_ENV_WINS_API_KEY", "from-env") };
        let result = config.resolve_secrets();
        unsafe { std::env::remove_var("SEEDMEDIC_TRACKER_ENV_WINS_API_KEY") };
        result.expect("resolves");

        assert_eq!(config.trackers[0].api_key.expose(), "from-env");
    }

    #[test]
    fn a_secret_file_wins_over_inline_and_is_trimmed_of_trailing_newlines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("api_key");
        std::fs::write(&file, "from-file\n").expect("write secret file");

        let toml_text = config_with_tracker(
            "file-wins",
            &format!(
                "api_key = \"from-inline\"\napi_key_file = \"{}\"",
                file.display()
            ),
        );
        let mut config: Config = toml::from_str(&toml_text).expect("parses");
        config.resolve_secrets().expect("resolves");

        assert_eq!(config.trackers[0].api_key.expose(), "from-file");
    }

    #[test]
    fn a_missing_secret_file_is_a_clear_error_naming_the_path() {
        let toml_text = config_with_tracker(
            "missing-file",
            "api_key_file = \"/nonexistent/path/to/api-key\"",
        );
        let mut config: Config = toml::from_str(&toml_text).expect("parses");

        let error = config
            .resolve_secrets()
            .expect_err("a missing secret file is an error");
        assert!(error.to_string().contains("/nonexistent/path/to/api-key"));
    }

    #[test]
    fn validate_runtime_rejects_a_unit3d_tracker_without_credentials() {
        let staging = tempfile::tempdir().expect("tempdir");
        let toml_text = format!(
            r#"
            [staging]
            root = "{}"

            [[trackers]]
            id = "aither"
            kind = "unit3d"
            base_url = "http://example.test"
            "#,
            staging.path().display()
        );
        let config: Config = toml::from_str(&toml_text).expect("parses");

        assert!(config.validate_runtime().is_err());
    }

    #[test]
    fn validate_runtime_rejects_a_non_existent_library_root() {
        let staging = tempfile::tempdir().expect("tempdir");
        let toml_text = format!(
            r#"
            [staging]
            root = "{}"

            [library]
            roots = ["/nonexistent/library/root"]

            [[trackers]]
            id = "example"
            kind = "fake"
            "#,
            staging.path().display()
        );
        let config: Config = toml::from_str(&toml_text).expect("parses");

        let error = config
            .validate_runtime()
            .expect_err("a missing library root is rejected");
        assert!(error.to_string().contains("/nonexistent/library/root"));
    }

    #[test]
    fn validate_runtime_rejects_a_staging_root_inside_a_library_root() {
        let library = tempfile::tempdir().expect("tempdir");
        let staging_root = library.path().join("staging");
        let toml_text = format!(
            r#"
            [staging]
            root = "{}"

            [library]
            roots = ["{}"]

            [[trackers]]
            id = "example"
            kind = "fake"
            "#,
            staging_root.display(),
            library.path().display()
        );
        let config: Config = toml::from_str(&toml_text).expect("parses");

        assert!(config.validate_runtime().is_err());
    }

    #[test]
    fn validate_runtime_warns_but_starts_when_min_match_confidence_is_exact() {
        let staging = tempfile::tempdir().expect("tempdir");
        let toml_text = format!(
            r#"
            [staging]
            root = "{}"

            [[trackers]]
            id = "example"
            kind = "fake"

            [policy]
            min_match_confidence = "exact"
            "#,
            staging.path().display()
        );
        let config: Config = toml::from_str(&toml_text).expect("parses");

        let warnings = config
            .validate_runtime()
            .expect("an exact confidence floor is a warning, not a failure");
        assert!(warnings.iter().any(|warning| warning.contains("exact")));
    }

    #[test]
    fn redacted_summary_never_contains_a_secret_value() {
        let toml_text = r#"
            [staging]
            root = "/srv/seedmedic/staging"

            [[trackers]]
            id = "example"
            kind = "unit3d"
            base_url = "http://example.test"
            api_key = "tr4ck3r-secret"

            [download_client]
            password = "qbit-secret"
        "#;
        let config: Config = toml::from_str(toml_text).expect("parses");

        let summary = config.redacted_summary();

        assert!(!summary.contains("tr4ck3r-secret"));
        assert!(!summary.contains("qbit-secret"));
        assert!(summary.contains("api_key=set"));
        assert!(summary.contains("password=set"));
    }
}
