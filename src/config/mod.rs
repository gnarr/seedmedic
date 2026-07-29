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
    collections::{BTreeMap, BTreeSet},
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

mod write;

pub use write::{ConfigDocument, DocumentError, SaveOutcome};

/// Where to look when `SEEDMEDIC_CONFIG` is not set.
const DEFAULT_PATH: &str = "config.toml";

/// Where an operator is pointed to configure an unconfigured setting — see
/// `docs/todos/0017-the-settings-pages.md`. Named as a constant so every
/// warning that mentions it stays consistent and only needs updating in one
/// place.
pub const SETTINGS_URL: &str = "/settings";

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Severity {
    Error,
    Warning,
}

/// One thing wrong with a configuration, attributed to the key that is
/// wrong, so a settings form can put the message next to the field and
/// `--check-config` can print all of them at once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Problem {
    /// Dotted key with concrete indices — `trackers.1.api_key`, not
    /// `trackers`. `None` only for a problem about the configuration as a
    /// whole, or one that spans more than one key.
    pub key: Option<String>,
    pub severity: Severity,
    pub message: String,
}

impl Problem {
    fn error(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            key: Some(key.into()),
            severity: Severity::Error,
            message: message.into(),
        }
    }

    fn warning(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            key: Some(key.into()),
            severity: Severity::Warning,
            message: message.into(),
        }
    }

    fn global_error(message: impl Into<String>) -> Self {
        Self {
            key: None,
            severity: Severity::Error,
            message: message.into(),
        }
    }

    fn global_warning(message: impl Into<String>) -> Self {
        Self {
            key: None,
            severity: Severity::Warning,
            message: message.into(),
        }
    }
}

/// Every error-severity problem, joined into the single message
/// `ConfigError::Invalid` needs. Warnings never fail validation.
fn as_result(problems: &[Problem]) -> Result<(), ConfigError> {
    let messages: Vec<&str> = problems
        .iter()
        .filter(|problem| problem.severity == Severity::Error)
        .map(|problem| problem.message.as_str())
        .collect();
    if messages.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Invalid(messages.join("\n")))
    }
}

/// Every name in a repeated section must be non-empty and unique, and no two
/// may collide once `shouty()` turns them into an environment variable
/// suffix — a collision would feed two entries from one variable and leave
/// two indistinguishable audit rows. Shared by `[[trackers]]` `id` and
/// `[[arr]]` `name`.
fn repeated_name_problems(section: &str, field: &str, names: &[String]) -> Vec<Problem> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    let mut seen_shouty: BTreeMap<String, &str> = BTreeMap::new();

    for (index, name) in names.iter().enumerate() {
        let key = format!("{section}.{index}.{field}");
        if name.trim().is_empty() {
            problems.push(Problem::error(
                key,
                format!("every {section} needs a non-empty `{field}`"),
            ));
            continue;
        }
        if !seen.insert(name.as_str()) {
            problems.push(Problem::error(
                key.clone(),
                format!("{section} {field} `{name}` is used twice"),
            ));
        }
        match seen_shouty.get(shouty(name).as_str()) {
            Some(&other) if other != name.as_str() => {
                problems.push(Problem::error(
                    key,
                    format!(
                        "{section} {field} `{name}` and `{other}` both become `{}` as an \
                         environment variable suffix, which collides",
                        shouty(name)
                    ),
                ));
            }
            _ => {
                seen_shouty.insert(shouty(name), name.as_str());
            }
        }
    }
    problems
}

/// What the settings UI is allowed to know about a [`Secret`]. Deliberately
/// has no variant carrying a value, so there is nothing to render by
/// accident — see `docs/todos/0017-the-settings-pages.md`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum SecretSource {
    #[default]
    Unset,
    /// Wins over everything else, so the UI shows it read-only.
    Environment { var: String },
    /// Also wins over an inline value, so the UI shows it read-only.
    File { path: PathBuf },
    /// The only source the UI can change.
    Inline,
}

/// A value that must not end up in a log line.
///
/// `Eq`/`PartialEq` are deliberately not derived: nothing outside this module
/// compares two secrets, and "does equality compare sources too" is a
/// question this type should not have to answer.
#[derive(Clone, Default)]
pub struct Secret {
    value: String,
    source: SecretSource,
}

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let source = if value.is_empty() {
            SecretSource::Unset
        } else {
            SecretSource::Inline
        };
        Self { value, source }
    }

    fn with_source(value: impl Into<String>, source: SecretSource) -> Self {
        Self {
            value: value.into(),
            source,
        }
    }

    pub fn expose(&self) -> &str {
        &self.value
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub fn source(&self) -> &SecretSource {
        &self.source
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.value.is_empty() {
            "Secret(unset)"
        } else {
            "Secret(***)"
        })
    }
}

/// `#[serde(transparent)]` cannot coexist with the `source` field above, so
/// this is hand-written: a secret loaded straight from TOML is `Inline`
/// (or `Unset`, if empty) until `resolve_secrets` runs and may replace that
/// with `Environment` or `File`.
impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::new(String::deserialize(deserializer)?))
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
        return Ok(Secret::with_source(
            value,
            SecretSource::Environment {
                var: env_var.to_owned(),
            },
        ));
    }
    if let Some(path) = file {
        let contents = std::fs::read_to_string(path).map_err(|source| {
            ConfigError::Invalid(format!(
                "cannot read secret file {} (set via a `_file` setting): {source}",
                path.display()
            ))
        })?;
        return Ok(Secret::with_source(
            contents.trim_end_matches(['\n', '\r']),
            SecretSource::File {
                path: path.to_owned(),
            },
        ));
    }
    Ok(inline.clone())
}

/// Turn an operator-chosen id into the shouty-snake-case fragment of an
/// environment variable name: uppercased, every non-alphanumeric character
/// replaced with `_`.
pub(crate) fn shouty(id: &str) -> String {
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
    /// `None` means no download client is configured yet — a fresh install,
    /// not a mistake. `Some` must be complete: see `problems_on_disk`.
    pub download_client: Option<DownloadClientConfig>,
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
    #[serde(rename = "qbittorrent")]
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
    /// Where `load`/`load_unvalidated` read from, absent a path argument:
    /// `SEEDMEDIC_CONFIG`, or `./config.toml`. Exposed so the caller can show
    /// an operator which file SeedMedic is reading, without duplicating this
    /// lookup.
    pub fn default_path() -> PathBuf {
        std::env::var_os("SEEDMEDIC_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_PATH))
    }

    /// Load from `SEEDMEDIC_CONFIG`, or `./config.toml`.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(&Self::default_path())
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let config = match Self::parse_from(path) {
            Ok(config) => config,
            Err(ConfigError::Read { path, source })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                let absolute = std::path::absolute(&path).unwrap_or(path);
                tracing::warn!(
                    path = %absolute.display(),
                    settings = SETTINGS_URL,
                    "no configuration file found; starting unconfigured"
                );
                let mut config = Self::default();
                config.resolve_secrets()?;
                config
            }
            Err(other) => return Err(other),
        };
        config.validate()?;
        // `problems()` needs no I/O and is checked by `validate()` above, but
        // only for errors — its warnings must still reach the log, on every
        // load (today that means startup; once 0016 lands, every reload too).
        for warning in config
            .problems()
            .into_iter()
            .filter(|problem| problem.severity == Severity::Warning)
            .map(|problem| problem.message)
        {
            tracing::warn!("{warning}");
        }
        for warning in config.validate_runtime()? {
            tracing::warn!("{warning}");
        }
        Ok(config)
    }

    /// Load from `SEEDMEDIC_CONFIG`, or `./config.toml`, without validating.
    /// For `--check-config`, which needs every `Problem` in one pass rather
    /// than the first `Err`.
    pub fn load_unvalidated() -> Result<Self, ConfigError> {
        Self::parse_from(&Self::default_path())
    }

    fn parse_from(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let mut config: Self = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        config.resolve_secrets()?;
        Ok(config)
    }

    /// Resolve every secret from environment, `_file`, or inline TOML, in
    /// that precedence order.
    fn resolve_secrets(&mut self) -> Result<(), ConfigError> {
        if let Some(download_client) = &mut self.download_client {
            download_client.password = resolve_secret(
                &download_client.password,
                download_client.password_file.as_deref(),
                "SEEDMEDIC_DOWNLOAD_CLIENT_PASSWORD",
            )?;
        }
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

    /// Reject anything that would make the system unsafe or useless.
    pub fn validate(&self) -> Result<(), ConfigError> {
        as_result(&self.problems())
    }

    /// Deeper checks that need the filesystem but must never write to it,
    /// touch the network, or open the database — safe to run via
    /// `--check-config` against a production config on a laptop.
    ///
    /// Returns non-fatal warnings on success; a configuration that cannot
    /// work at all is an `Err`.
    pub fn validate_runtime(&self) -> Result<Vec<String>, ConfigError> {
        let problems = self.problems_on_disk();
        as_result(&problems)?;
        Ok(problems
            .into_iter()
            .filter(|problem| problem.severity == Severity::Warning)
            .map(|problem| problem.message)
            .collect())
    }

    /// Every problem findable without I/O of any kind.
    pub fn problems(&self) -> Vec<Problem> {
        let mut problems = Vec::new();

        if let Err(error) = self.server.bind_address.parse::<SocketAddr>() {
            problems.push(Problem::error(
                "server.bind_address",
                format!(
                    "server.bind_address `{}` is not a socket address: {error}",
                    self.server.bind_address
                ),
            ));
        }
        if self.server.auth_token.is_empty() {
            problems.push(Problem::warning(
                "server.auth_token",
                format!(
                    "server.auth_token is unset; anyone who can reach this port can change \
                     where SeedMedic writes — see {SETTINGS_URL}"
                ),
            ));
        }

        if self.trackers.is_empty() {
            problems.push(Problem::global_warning(
                "no [[trackers]] entry is configured; correct for a fresh install, but \
                 discovery will find nothing until at least one is set",
            ));
        }
        let tracker_ids: Vec<String> = self.trackers.iter().map(|t| t.id.clone()).collect();
        problems.extend(repeated_name_problems("trackers", "id", &tracker_ids));

        let has_fake_tracker = self.trackers.iter().any(|t| t.kind == TrackerKind::Fake);
        let has_real_tracker = self.trackers.iter().any(|t| t.kind != TrackerKind::Fake);
        if has_fake_tracker && has_real_tracker {
            problems.push(Problem::global_error(
                "trackers mix a `fake` tracker with a real one; the torrent decoder is chosen \
                 for the whole build, so the fake tracker's demo torrents will fail to parse",
            ));
        }
        for (index, tracker) in self.trackers.iter().enumerate() {
            if tracker.kind == TrackerKind::Fake && !cfg!(feature = "fakes") {
                problems.push(Problem::error(
                    format!("trackers.{index}.kind"),
                    format!(
                        "tracker `{}` is configured as `fake`, but this build has the `fakes` \
                         feature disabled",
                        tracker.id
                    ),
                ));
            }
        }

        if self.staging.root.as_os_str().is_empty() {
            problems.push(Problem::warning(
                "staging.root",
                format!(
                    "staging.root is unset; no repair can be materialized until it is set — \
                     see {SETTINGS_URL}"
                ),
            ));
        } else if !self.staging.root.is_absolute() {
            problems.push(Problem::error(
                "staging.root",
                format!(
                    "staging.root `{}` must be an absolute path",
                    self.staging.root.display()
                ),
            ));
        }
        if self.staging.min_free_bytes == 0 {
            problems.push(Problem::warning(
                "staging.min_free_bytes",
                "staging.min_free_bytes is 0, disabling the free-space margin it exists for",
            ));
        }

        for (index, root) in self.library.roots.iter().enumerate() {
            if !root.is_absolute() {
                problems.push(Problem::error(
                    format!("library.roots.{index}"),
                    format!("library root `{}` must be an absolute path", root.display()),
                ));
            }
        }
        if self.arr.is_empty() && self.library.roots.is_empty() {
            problems.push(Problem::global_warning(
                "no candidate source is configured (`[[arr]]` or `library.roots`); correct for \
                 a fresh install, but SeedMedic will not find anything to repair until one is \
                 set",
            ));
        }

        // Permitting nothing is not a safety setting, it is a repair that can
        // never happen. Say so at startup rather than on the first job.
        if !(self.policy.prefer_reflink || self.policy.allow_hardlink || self.policy.allow_copy) {
            problems.push(Problem::global_error(
                "policy permits no materialization strategy; enable at least one of \
                 prefer_reflink, allow_copy, allow_hardlink",
            ));
        }
        if self.policy.max_attempts == 0 {
            problems.push(Problem::error(
                "policy.max_attempts",
                "policy.max_attempts must be at least 1",
            ));
        }
        if self.policy.retry_max_seconds < self.policy.retry_base_seconds {
            problems.push(Problem::error(
                "policy.retry_max_seconds",
                "policy.retry_max_seconds must be at least retry_base_seconds",
            ));
        }
        if self.policy.recheck_poll_max_seconds < self.policy.recheck_poll_seconds {
            problems.push(Problem::error(
                "policy.recheck_poll_max_seconds",
                "policy.recheck_poll_max_seconds must be at least recheck_poll_seconds",
            ));
        }
        if self.policy.recheck_timeout_seconds == 0 {
            problems.push(Problem::error(
                "policy.recheck_timeout_seconds",
                "policy.recheck_timeout_seconds must be at least 1",
            ));
        } else if self.policy.recheck_timeout_seconds < self.policy.recheck_poll_max_seconds {
            problems.push(Problem::warning(
                "policy.recheck_timeout_seconds",
                format!(
                    "policy.recheck_timeout_seconds ({}) is less than recheck_poll_max_seconds \
                     ({}); a recheck times out before it can be polled a second time",
                    self.policy.recheck_timeout_seconds, self.policy.recheck_poll_max_seconds
                ),
            ));
        }
        if self.policy.verification_pieces == 0 {
            problems.push(Problem::warning(
                "policy.verification_pieces",
                "policy.verification_pieces is 0, disabling piece verification; no match can \
                 exceed `probable` confidence",
            ));
        }
        if self.policy.min_match_confidence == MatchConfidence::Exact {
            problems.push(Problem::warning(
                "policy.min_match_confidence",
                "policy.min_match_confidence = \"exact\" requires piece-verified matches; \
                 until docs/todos/0005-media-matching.md lands, no candidate can reach that \
                 confidence, so every repair will park for review",
            ));
        }
        if self.policy.max_consecutive_unknown_tracker_status == 0 {
            problems.push(Problem::error(
                "policy.max_consecutive_unknown_tracker_status",
                "policy.max_consecutive_unknown_tracker_status must be at least 1",
            ));
        }
        if self.policy.tracker_poll_min_seconds > self.policy.tracker_poll_seconds {
            problems.push(Problem::error(
                "policy.tracker_poll_min_seconds",
                "policy.tracker_poll_min_seconds must be at most tracker_poll_seconds",
            ));
        }
        // A private tracker still bans for hammering, regardless of how the
        // interval got set this low.
        const MIN_TRACKER_POLL_SECONDS: u64 = 60;
        if self.policy.tracker_poll_seconds < MIN_TRACKER_POLL_SECONDS {
            problems.push(Problem::error(
                "policy.tracker_poll_seconds",
                format!(
                    "policy.tracker_poll_seconds ({}) is below the minimum of \
                     {MIN_TRACKER_POLL_SECONDS}s; polling a private tracker this often risks a \
                     ban",
                    self.policy.tracker_poll_seconds
                ),
            ));
        }

        if self.worker.batch_size < 1 {
            problems.push(Problem::error(
                "worker.batch_size",
                "worker.batch_size must be at least 1",
            ));
        }
        if self.worker.lease_seconds == 0 {
            problems.push(Problem::error(
                "worker.lease_seconds",
                "worker.lease_seconds must be at least 1",
            ));
        }
        if self.worker.owner.trim().is_empty() {
            problems.push(Problem::error(
                "worker.owner",
                "worker.owner must not be empty",
            ));
        }

        match &self.download_client {
            None => problems.push(Problem::global_warning(format!(
                "no download_client is configured; correct for a fresh install, but no repair \
                 can be seeded until one is set — see {SETTINGS_URL}"
            ))),
            Some(download_client) => {
                if download_client.base_url == placeholder_url() {
                    problems.push(Problem::warning(
                        "download_client.base_url",
                        "download_client.base_url is still the http://localhost placeholder",
                    ));
                }
                if download_client.kind == DownloadClientKind::Fake && !cfg!(feature = "fakes") {
                    problems.push(Problem::error(
                        "download_client.kind",
                        "download_client is configured as `fake`, but this build has the \
                         `fakes` feature disabled",
                    ));
                }
            }
        }

        let arr_names: Vec<String> = self.arr.iter().map(|a| a.name.clone()).collect();
        problems.extend(repeated_name_problems("arr", "name", &arr_names));

        if let Some(url) = &self.notifications.webhook_url
            && url.scheme() != "http"
            && url.scheme() != "https"
        {
            problems.push(Problem::error(
                "notifications.webhook_url",
                format!(
                    "notifications.webhook_url `{url}` must be http or https, not `{}`",
                    url.scheme()
                ),
            ));
        }

        problems
    }

    /// Every problem that needs the filesystem. Never writes to it, never
    /// touches the network, never opens the database.
    pub fn problems_on_disk(&self) -> Vec<Problem> {
        let mut problems = Vec::new();

        for (index, tracker) in self.trackers.iter().enumerate() {
            if tracker.kind == TrackerKind::Unit3d && tracker.api_key.is_empty() {
                problems.push(Problem::error(
                    format!("trackers.{index}.api_key"),
                    format!(
                        "tracker `{}` is a unit3d tracker and needs an api_key, set inline, via \
                         api_key_file, or via SEEDMEDIC_TRACKER_{}_API_KEY",
                        tracker.id,
                        shouty(&tracker.id)
                    ),
                ));
            }
        }

        if let Some(download_client) = &self.download_client
            && download_client.kind == DownloadClientKind::QBittorrent
            && (download_client.username.is_empty() || download_client.password.is_empty())
        {
            problems.push(Problem::error(
                "download_client",
                "download_client.kind = \"qbittorrent\" needs both username and password; \
                 password may be set inline, via password_file, or via \
                 SEEDMEDIC_DOWNLOAD_CLIENT_PASSWORD",
            ));
        }

        for (index, arr) in self.arr.iter().enumerate() {
            if arr.api_key.is_empty() {
                problems.push(Problem::error(
                    format!("arr.{index}.api_key"),
                    format!(
                        "arr instance `{}` needs an api_key, set inline, via api_key_file, or \
                         via SEEDMEDIC_ARR_{}_API_KEY",
                        arr.name,
                        shouty(&arr.name)
                    ),
                ));
            }
        }

        for (index, root) in self.library.roots.iter().enumerate() {
            if let Err(error) = std::fs::read_dir(root) {
                problems.push(Problem::error(
                    format!("library.roots.{index}"),
                    format!(
                        "library root `{}` is not a readable directory: {error}",
                        root.display()
                    ),
                ));
            }
        }

        if !self.staging.root.as_os_str().is_empty() {
            if let Err(error) =
                crate::staging::StagingRoot::check_overlap(&self.staging.root, &self.library.roots)
            {
                problems.push(Problem::error("staging.root", error.to_string()));
            }

            let ancestor = nearest_existing_ancestor(&self.staging.root);
            if !directory_is_writable(&ancestor) {
                problems.push(Problem::error(
                    "staging.root",
                    format!(
                        "staging.root `{}` is not writable: `{}` denies write access",
                        self.staging.root.display(),
                        ancestor.display()
                    ),
                ));
            }
        }

        if self.metrics.enabled && !cfg!(feature = "metrics") {
            problems.push(Problem::warning(
                "metrics.enabled",
                "metrics.enabled = true, but this build does not have the `metrics` feature; \
                 no metrics will be collected",
            ));
        }

        problems
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

        match &self.download_client {
            None => writeln!(out, "download_client = unconfigured").unwrap(),
            Some(download_client) => writeln!(
                out,
                "download_client kind={:?} base_url={} username={:?} password={} category={:?}",
                download_client.kind,
                download_client.base_url,
                download_client.username,
                secret_state(&download_client.password),
                download_client.category
            )
            .unwrap(),
        }

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

    /// Does `problems()` report an error attributed to exactly this key?
    fn has_error(toml_text: &str, key: &str) -> bool {
        let config: Config = toml::from_str(toml_text).expect("test config parses");
        config.problems().iter().any(|problem| {
            problem.severity == Severity::Error && problem.key.as_deref() == Some(key)
        })
    }

    /// Does `problems()` report a warning attributed to exactly this key?
    fn has_warning(toml_text: &str, key: &str) -> bool {
        let config: Config = toml::from_str(toml_text).expect("test config parses");
        config.problems().iter().any(|problem| {
            problem.severity == Severity::Warning && problem.key.as_deref() == Some(key)
        })
    }

    /// Does `problems()` report an error about the configuration as a whole
    /// (no single key), whose message contains this fragment?
    fn has_global_error(toml_text: &str, message_fragment: &str) -> bool {
        let config: Config = toml::from_str(toml_text).expect("test config parses");
        config.problems().iter().any(|problem| {
            problem.severity == Severity::Error
                && problem.key.is_none()
                && problem.message.contains(message_fragment)
        })
    }

    /// Does `problems()` report a warning about the configuration as a whole
    /// (no single key), whose message contains this fragment?
    fn has_global_warning(toml_text: &str, message_fragment: &str) -> bool {
        let config: Config = toml::from_str(toml_text).expect("test config parses");
        config.problems().iter().any(|problem| {
            problem.severity == Severity::Warning
                && problem.key.is_none()
                && problem.message.contains(message_fragment)
        })
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
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "Secret(***)");
        assert!(!format!("{secret:?}").contains("hunter2"));
    }

    #[test]
    fn a_relative_staging_root_is_rejected() {
        let config = MINIMAL.replace("/srv/seedmedic/staging", "staging");
        assert!(parse(&config).is_err());
        assert!(has_error(&config, "staging.root"));
    }

    #[test]
    fn duplicate_tracker_ids_are_rejected() {
        let config = format!("{MINIMAL}\n[[trackers]]\nid = \"example\"\nkind = \"fake\"\n");
        assert!(parse(&config).is_err());
        assert!(has_error(&config, "trackers.1.id"));
    }

    #[test]
    fn no_trackers_is_a_warning_not_an_error() {
        let config = "[staging]\nroot = \"/srv/staging\"\n";
        assert!(parse(config).is_ok(), "a warning must not fail validate()");
        assert!(has_global_warning(
            config,
            "no [[trackers]] entry is configured"
        ));
    }

    #[test]
    fn a_poll_cap_below_the_base_interval_is_rejected() {
        let config = format!(
            "{MINIMAL}\n[policy]\nrecheck_poll_seconds = 60\nrecheck_poll_max_seconds = 30\n"
        );
        assert!(parse(&config).is_err());
        assert!(has_error(&config, "policy.recheck_poll_max_seconds"));
    }

    #[test]
    fn a_tracker_poll_floor_above_the_ceiling_is_rejected() {
        let config = format!(
            "{MINIMAL}\n[policy]\ntracker_poll_seconds = 60\ntracker_poll_min_seconds = 120\n"
        );
        assert!(parse(&config).is_err());
        assert!(has_error(&config, "policy.tracker_poll_min_seconds"));
    }

    #[test]
    fn a_zero_unknown_tracker_status_threshold_is_rejected() {
        let config = format!("{MINIMAL}\n[policy]\nmax_consecutive_unknown_tracker_status = 0\n");
        assert!(parse(&config).is_err());
        assert!(has_error(
            &config,
            "policy.max_consecutive_unknown_tracker_status"
        ));
    }

    #[test]
    fn a_zero_recheck_timeout_is_rejected() {
        let config = format!("{MINIMAL}\n[policy]\nrecheck_timeout_seconds = 0\n");
        assert!(parse(&config).is_err());
        assert!(has_error(&config, "policy.recheck_timeout_seconds"));
    }

    #[test]
    fn permitting_no_materialization_strategy_is_rejected() {
        let config = format!(
            "{MINIMAL}\n[policy]\nprefer_reflink = false\nallow_hardlink = false\nallow_copy = false\n"
        );
        assert!(parse(&config).is_err());
        assert!(has_global_error(&config, "materialization strategy"));
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_silently_ignored() {
        let config = format!("{MINIMAL}\n[policy]\nauto_resum = \"never\"\n");
        assert!(toml::from_str::<Config>(&config).is_err());
    }

    #[test]
    fn the_example_config_is_valid() {
        let example = include_str!("../../config.example.toml");
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
    fn min_match_confidence_exact_is_a_warning_not_an_error() {
        let config = format!("{MINIMAL}\n[policy]\nmin_match_confidence = \"exact\"\n");
        assert!(parse(&config).is_ok(), "a warning must not fail validate()");
        assert!(has_warning(&config, "policy.min_match_confidence"));
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

    #[test]
    fn a_warning_does_not_make_validate_fail() {
        let config: Config = toml::from_str(MINIMAL).expect("parses");
        assert!(
            !config.problems().is_empty(),
            "MINIMAL should carry warnings"
        );
        assert!(config.validate().is_ok());
    }

    #[test]
    fn problems_does_no_io_and_ignores_a_nonexistent_library_root() {
        let toml_text = format!("{MINIMAL}\n[library]\nroots = [\"/nonexistent/library/root\"]\n");
        let config: Config = toml::from_str(&toml_text).expect("parses");
        assert!(
            config
                .problems()
                .iter()
                .all(|problem| !problem.message.contains("/nonexistent"))
        );
    }

    #[test]
    fn three_independent_mistakes_produce_three_problems_not_one() {
        let toml_text = format!(
            "{MINIMAL}\n[policy]\nmax_attempts = 0\nrecheck_timeout_seconds = 0\n\
             [worker]\nbatch_size = 0\n"
        );
        let config: Config = toml::from_str(&toml_text).expect("parses");
        let errors: Vec<_> = config
            .problems()
            .into_iter()
            .filter(|problem| problem.severity == Severity::Error)
            .collect();
        assert_eq!(errors.len(), 3, "{errors:?}");
    }

    #[test]
    fn a_missing_tracker_api_key_names_the_indexed_key() {
        let toml_text = r#"
            [staging]
            root = "/srv/seedmedic/staging"

            [[trackers]]
            id = "demo"
            kind = "fake"

            [[trackers]]
            id = "aither"
            kind = "unit3d"
            base_url = "http://example.test"
        "#;
        let config: Config = toml::from_str(toml_text).expect("parses");
        let problem = config
            .problems_on_disk()
            .into_iter()
            .find(|problem| problem.key.as_deref() == Some("trackers.1.api_key"))
            .expect("missing api_key is a problem attributed to the second tracker");
        assert_eq!(problem.severity, Severity::Error);
    }

    #[test]
    fn two_errors_and_one_warning_are_all_reported() {
        let toml_text = r#"
            [server]
            auth_token = "shh"

            [staging]
            root = "/srv/seedmedic/staging"
            min_free_bytes = 0

            [library]
            roots = ["/srv/media"]

            [[trackers]]
            id = "example"
            kind = "fake"

            [download_client]
            kind = "fake"
            base_url = "http://qbittorrent:8080"

            [policy]
            max_attempts = 0

            [worker]
            batch_size = 0
        "#;
        let config: Config = toml::from_str(toml_text).expect("parses");
        let problems = config.problems();

        let errors = problems
            .iter()
            .filter(|problem| problem.severity == Severity::Error)
            .count();
        let warnings = problems
            .iter()
            .filter(|problem| problem.severity == Severity::Warning)
            .count();
        assert_eq!(errors, 2, "{problems:?}");
        assert_eq!(warnings, 1, "{problems:?}");
    }

    #[test]
    fn an_unparseable_config_file_still_fails_hard() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not valid toml =====").expect("write");

        let error = Config::load_from(&path).expect_err("unparseable config is an error");
        assert!(matches!(error, ConfigError::Parse { .. }));
    }

    #[test]
    fn a_config_path_unreadable_for_another_reason_still_fails_hard() {
        // A directory is never a valid config file, and reading it fails for a
        // reason other than "not found" — the same bucket a permission-denied
        // file would fall into.
        let dir = tempfile::tempdir().expect("tempdir");

        let error = Config::load_from(dir.path()).expect_err("a directory is not readable");
        assert!(matches!(error, ConfigError::Read { .. }));
    }

    // --- Gaps closed by docs/todos/0014-configuration-problems-as-data.md ---

    #[test]
    fn an_empty_arr_name_is_rejected() {
        let config = format!(
            "{MINIMAL}\n[[arr]]\nkind = \"sonarr\"\nname = \"\"\nbase_url = \"http://sonarr.test\"\n"
        );
        assert!(has_error(&config, "arr.0.name"));
    }

    #[test]
    fn duplicate_arr_names_are_rejected() {
        let config = format!(
            "{MINIMAL}\n\
             [[arr]]\nkind = \"sonarr\"\nname = \"main\"\nbase_url = \"http://sonarr.test\"\n\
             [[arr]]\nkind = \"radarr\"\nname = \"main\"\nbase_url = \"http://radarr.test\"\n"
        );
        assert!(has_error(&config, "arr.1.name"));
    }

    #[test]
    fn arr_names_colliding_only_after_shouty_are_rejected() {
        let config = format!(
            "{MINIMAL}\n\
             [[arr]]\nkind = \"sonarr\"\nname = \"main-arr\"\nbase_url = \"http://sonarr.test\"\n\
             [[arr]]\nkind = \"radarr\"\nname = \"main_arr\"\nbase_url = \"http://radarr.test\"\n"
        );
        assert!(has_error(&config, "arr.1.name"));
    }

    #[test]
    fn tracker_ids_colliding_only_after_shouty_are_rejected() {
        // `exa-mple` and `exa_mple` are different ids, but both become the
        // environment variable suffix `EXA_MPLE`.
        let toml_text = MINIMAL.replace("\"example\"", "\"exa-mple\"");
        let toml_text = format!("{toml_text}\n[[trackers]]\nid = \"exa_mple\"\nkind = \"fake\"\n");
        assert!(has_error(&toml_text, "trackers.1.id"));
    }

    #[test]
    fn retry_max_below_retry_base_is_rejected() {
        let config =
            format!("{MINIMAL}\n[policy]\nretry_base_seconds = 120\nretry_max_seconds = 60\n");
        assert!(has_error(&config, "policy.retry_max_seconds"));
    }

    #[test]
    fn a_placeholder_download_client_base_url_is_a_warning() {
        let config = format!("{MINIMAL}\n[download_client]\n");
        assert!(has_warning(&config, "download_client.base_url"));
    }

    #[test]
    fn no_candidate_source_is_a_warning() {
        assert!(has_global_warning(
            MINIMAL,
            "no candidate source is configured"
        ));
    }

    #[test]
    fn a_candidate_source_silences_the_no_candidate_source_warning() {
        let config = format!("{MINIMAL}\n[library]\nroots = [\"/srv/media\"]\n");
        assert!(!has_global_warning(
            &config,
            "no candidate source is configured"
        ));
    }

    #[test]
    fn a_non_http_webhook_scheme_is_rejected() {
        let config =
            format!("{MINIMAL}\n[notifications]\nwebhook_url = \"mailto:ops@example.test\"\n");
        assert!(has_error(&config, "notifications.webhook_url"));
    }

    #[test]
    fn an_unset_auth_token_is_a_warning() {
        assert!(has_warning(MINIMAL, "server.auth_token"));
    }

    #[test]
    fn a_zero_min_free_bytes_is_a_warning() {
        let config = "[staging]\nroot = \"/srv/seedmedic/staging\"\nmin_free_bytes = 0\n\n\
             [[trackers]]\nid = \"example\"\nkind = \"fake\"\n";
        assert!(has_warning(config, "staging.min_free_bytes"));
    }

    #[test]
    fn zero_verification_pieces_is_a_warning() {
        let config = format!("{MINIMAL}\n[policy]\nverification_pieces = 0\n");
        assert!(has_warning(&config, "policy.verification_pieces"));
    }

    #[test]
    fn a_recheck_timeout_below_the_poll_ceiling_is_a_warning() {
        let config = format!(
            "{MINIMAL}\n[policy]\nrecheck_poll_max_seconds = 600\nrecheck_timeout_seconds = 300\n"
        );
        assert!(has_warning(&config, "policy.recheck_timeout_seconds"));
    }

    #[test]
    fn mixing_a_fake_tracker_with_a_real_one_is_rejected() {
        let config = format!(
            "{MINIMAL}\n[[trackers]]\nid = \"aither\"\nkind = \"unit3d\"\nbase_url = \"http://example.test\"\n"
        );
        assert!(has_global_error(
            &config,
            "mix a `fake` tracker with a real one"
        ));
    }

    // --- Gaps closed by docs/todos/0015-start-without-a-configuration-file.md ---

    #[test]
    fn an_unset_staging_root_is_a_warning_not_an_error() {
        let config = "[[trackers]]\nid = \"example\"\nkind = \"fake\"\n";
        assert!(parse(config).is_ok(), "a warning must not fail validate()");
        assert!(has_warning(config, "staging.root"));
    }

    #[test]
    fn an_absent_download_client_is_a_warning_not_an_error() {
        assert!(parse(MINIMAL).is_ok(), "a warning must not fail validate()");
        assert!(has_global_warning(
            MINIMAL,
            "no download_client is configured"
        ));
    }

    #[test]
    fn a_qbittorrent_client_without_credentials_is_rejected() {
        let toml_text = format!(
            "{MINIMAL}\n[download_client]\nkind = \"qbittorrent\"\nbase_url = \"http://qbit.test\"\n"
        );
        let config: Config = toml::from_str(&toml_text).expect("parses");
        let error = config
            .validate_runtime()
            .expect_err("missing qbittorrent credentials are rejected");
        assert!(error.to_string().contains("download_client"));
    }

    #[test]
    fn a_qbittorrent_client_with_credentials_has_no_credential_problem() {
        let toml_text = format!(
            "{MINIMAL}\n[download_client]\nkind = \"qbittorrent\"\nbase_url = \
             \"http://qbit.test\"\nusername = \"admin\"\npassword = \"hunter2\"\n"
        );
        let config: Config = toml::from_str(&toml_text).expect("parses");
        assert!(
            config
                .problems_on_disk()
                .iter()
                .all(|problem| problem.key.as_deref() != Some("download_client"))
        );
    }

    /// The premise of this whole document: an operator who starts SeedMedic
    /// with no configuration file at all must not be met with a hard error.
    #[test]
    fn the_default_configuration_is_startable() {
        let config = Config::default();
        let errors: Vec<_> = config
            .problems()
            .into_iter()
            .filter(|problem| problem.severity == Severity::Error)
            .collect();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn a_missing_config_file_starts_unconfigured_with_defaults() {
        let config = Config::load_from(Path::new("/nonexistent/config.toml"))
            .expect("a missing file falls back to defaults, not an error");
        assert!(config.trackers.is_empty());
        assert!(config.staging.root.as_os_str().is_empty());
        assert!(config.download_client.is_none());
    }

    /// `load_from` must log every warning `problems()` finds, not only the
    /// on-disk ones — otherwise a synchronous warning like this one is
    /// silently dropped on every startup, which defeats the entire point of
    /// downgrading these settings to warnings instead of hard errors.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn an_unset_auth_token_logs_a_loud_warning_on_load() {
        let staging = tempfile::tempdir().expect("tempdir");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            format!(
                "[staging]\nroot = \"{}\"\n\n[[trackers]]\nid = \"example\"\nkind = \"fake\"\n",
                staging.path().display()
            ),
        )
        .expect("write");

        Config::load_from(&path).expect("a writable staging root is startable");

        assert!(logs_contain("anyone who can reach this port"));
    }
}
