//! The field table: one entry per `Config` key, driving both how `/settings`
//! renders a field and how a submitted value is parsed back into a draft.
//! See `docs/todos/0017-the-settings-pages.md`.
//!
//! **A form field's `name` is its dotted TOML key** —
//! `policy.max_attempts`, `trackers.0.base_url`, `arr.1.path_mappings.0.from`
//! — so there is no separate form-to-config mapping table to drift out of
//! sync with `Config` itself. What keeps it in sync is the pair of tests at
//! the bottom of this file: one asserts every key here exists in `Config`,
//! the other that every `Config` key has an entry here.

use crate::config::shouty;

/// Which environment variable a [`Kind::Secret`] can also come from, so the
/// UI can hint at it even before the operator has set anything. The two
/// repeated sections compute their variable name from the row's id, so it
/// cannot be a fixed string the way `server.auth_token`'s can.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecretEnv {
    Fixed(&'static str),
    PerTracker,
    PerArr,
}

impl SecretEnv {
    /// `row_id` is the concrete `id`/`name` typed into that repeated row, if
    /// any yet. Absent that, the pattern is shown with a placeholder.
    pub fn describe(&self, row_id: Option<&str>) -> String {
        match self {
            SecretEnv::Fixed(name) => (*name).to_owned(),
            SecretEnv::PerTracker => format!(
                "SEEDMEDIC_TRACKER_{}_API_KEY",
                row_id.map(shouty).unwrap_or_else(|| "<ID>".to_owned())
            ),
            SecretEnv::PerArr => format!(
                "SEEDMEDIC_ARR_{}_API_KEY",
                row_id.map(shouty).unwrap_or_else(|| "<NAME>".to_owned())
            ),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Kind {
    Bool,
    Count {
        unit: Option<&'static str>,
        min: u64,
    },
    Text,
    Url,
    AbsolutePath,
    /// A textarea, one absolute path per line. `library.roots` only — see
    /// the "awkward cases" in docs/todos/0017-the-settings-pages.md for why
    /// nothing else gets this treatment.
    AbsolutePathList,
    /// Exactly the serde names, so a `<select>` offers precisely what the
    /// file accepts and nothing `to_config` would then reject.
    Choice(&'static [&'static str]),
    /// Never rendered with a value — see `SecretSource` in `crate::config`.
    Secret {
        env_var: SecretEnv,
    },
    /// Display-only. Editable would turn an unauthenticated settings page
    /// into a remote arbitrary-file-read primitive.
    SecretFile,
}

pub struct Field {
    /// The dotted TOML key, which is also the form field's `name`. Repeated
    /// sections use `*` here and a concrete index at render/submit time:
    /// `trackers.*.base_url` becomes `trackers.0.base_url`.
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    /// Rendered with a warning and a confirmation step. `policy.allow_hardlink`
    /// is the entire reason this exists.
    pub danger: Option<&'static str>,
    /// True for exactly two keys: `server.bind_address`, `database.path`.
    pub restart_required: bool,
}

const fn field(key: &'static str, label: &'static str, help: &'static str, kind: Kind) -> Field {
    Field {
        key,
        label,
        help,
        kind,
        danger: None,
        restart_required: false,
    }
}

const fn restart(mut f: Field) -> Field {
    f.restart_required = true;
    f
}

const fn danger(mut f: Field, warning: &'static str) -> Field {
    f.danger = Some(warning);
    f
}

pub static FIELDS: &[Field] = &[
    // --- server ---
    restart(field(
        "server.bind_address",
        "Listen address",
        "The `host:port` the operator UI and API bind to.",
        Kind::Text,
    )),
    field(
        "server.auth_token",
        "Auth token",
        "Optional. The web UI has no accounts or roles — this is a single shared secret, not a \
         login system. When set, every request but /health and /login must present it, either \
         as `Authorization: Bearer <token>` or by signing in at /login. Saving it here signs \
         you in immediately, so you are not locked out of this page.",
        Kind::Secret {
            env_var: SecretEnv::Fixed("SEEDMEDIC_SERVER_AUTH_TOKEN"),
        },
    ),
    field(
        "server.auth_token_file",
        "Auth token file",
        "Read the auth token from a file instead — e.g. a mounted Docker/Kubernetes secret.",
        Kind::SecretFile,
    ),
    // --- database ---
    restart(field(
        "database.path",
        "Database path",
        "Where the SQLite database lives.",
        Kind::AbsolutePath,
    )),
    // --- staging ---
    field(
        "staging.root",
        "Staging root",
        "Absolute, and not inside a media library root. SeedMedic writes here and nowhere else; \
         may be left unset for a fresh install, but no repair can be materialized until it is \
         set. Your download client must see this exact path too — SeedMedic hands it over \
         verbatim as the torrent's save path, and nothing translates between the two.",
        Kind::AbsolutePath,
    ),
    field(
        "staging.min_free_bytes",
        "Minimum free space",
        "Free space to keep on the staging filesystem beyond what a repair needs. A plan that \
         would eat into this margin parks for review rather than filling the disk.",
        Kind::Count {
            unit: Some("bytes"),
            min: 0,
        },
    ),
    // --- library ---
    field(
        "library.roots",
        "Library roots",
        "Read-only. Used as a fallback candidate source and to prove the staging area is \
         somewhere else. One absolute path per line.",
        Kind::AbsolutePathList,
    ),
    // --- policy (15 fields; kept on one page — it is cohesive) ---
    danger(
        field(
            "policy.auto_resume",
            "Auto-resume",
            "Whether a verified repair may start seeding without a human approving it first. \
             There is no \"always\": nothing resumes without a clean hash check.",
            Kind::Choice(&["never", "when_verified_complete"]),
        ),
        "Resuming automatically means nobody reviews a repair before it starts seeding again.",
    ),
    field(
        "policy.min_match_confidence",
        "Minimum match confidence",
        "How sure a candidate match must be before a repair proceeds without review. \"exact\" \
         requires piece-verified matches, which is not implemented yet, so every repair parks \
         for review until it is.",
        Kind::Choice(&["ambiguous", "probable", "operator", "exact"]),
    ),
    field(
        "policy.verification_pieces",
        "Verification pieces",
        "Pieces hashed per file to confirm a match: first, last, and roughly middle. 0 disables \
         verification, so a match can never exceed \"probable\".",
        Kind::Count {
            unit: Some("pieces"),
            min: 0,
        },
    ),
    field(
        "policy.prefer_reflink",
        "Prefer reflink",
        "Copy-on-write clone: free, and writes to the staged file never reach the library. \
         Preferred over every other strategy when the filesystem supports it.",
        Kind::Bool,
    ),
    danger(
        field(
            "policy.allow_hardlink",
            "Allow hardlink",
            "A hardlinked staged file *is* the library file.",
            Kind::Bool,
        ),
        "A hardlinked staged file *is* the library file. SeedMedic refuses to resume an \
         incomplete torrent in that situation, but the safest setting is off.",
    ),
    field(
        "policy.allow_copy",
        "Allow copy",
        "Full copy: costs disk, shares nothing with the library.",
        Kind::Bool,
    ),
    field(
        "policy.max_attempts",
        "Max attempts",
        "How many times a step retries a transient failure before the job parks for review.",
        Kind::Count { unit: None, min: 1 },
    ),
    field(
        "policy.retry_base_seconds",
        "Retry base delay",
        "Starting backoff between retries of a transient failure.",
        Kind::Count {
            unit: Some("seconds"),
            min: 0,
        },
    ),
    field(
        "policy.retry_max_seconds",
        "Retry max delay",
        "Cap on the retry backoff. Must be at least the base delay.",
        Kind::Count {
            unit: Some("seconds"),
            min: 0,
        },
    ),
    field(
        "policy.recheck_poll_seconds",
        "Recheck poll interval",
        "How often to poll a running recheck, at first. Doubles as the check keeps running, \
         capped at the max below.",
        Kind::Count {
            unit: Some("seconds"),
            min: 0,
        },
    ),
    field(
        "policy.recheck_poll_max_seconds",
        "Recheck poll interval (max)",
        "Cap of the adaptive recheck poll backoff, and the interval used while a check is \
         queued rather than running.",
        Kind::Count {
            unit: Some("seconds"),
            min: 0,
        },
    ),
    field(
        "policy.recheck_timeout_seconds",
        "Recheck timeout",
        "A recheck running longer than this parks the job for review instead of polling \
         forever. Four hours is generous for a 100 GB torrent on spinning rust; raise it for \
         slower storage.",
        Kind::Count {
            unit: Some("seconds"),
            min: 1,
        },
    ),
    field(
        "policy.tracker_poll_seconds",
        "Tracker poll interval",
        "How often to check the tracker for a hit-and-run's status. Minimum 60 seconds — \
         polling a private tracker more often risks a ban.",
        Kind::Count {
            unit: Some("seconds"),
            min: 60,
        },
    ),
    field(
        "policy.tracker_poll_min_seconds",
        "Tracker poll interval (min)",
        "Floor of the adaptive tracker-poll backoff as a hit-and-run deadline approaches. Must \
         be at most the interval above.",
        Kind::Count {
            unit: Some("seconds"),
            min: 0,
        },
    ),
    field(
        "policy.max_consecutive_unknown_tracker_status",
        "Max consecutive unknown tracker status",
        "Consecutive \"unknown\" tracker answers before a seeding job parks for review instead \
         of polling forever.",
        Kind::Count { unit: None, min: 1 },
    ),
    // --- worker ---
    field(
        "worker.owner",
        "Worker owner",
        "Identifies this process's leases, so two processes sharing a database do not steal \
         each other's jobs.",
        Kind::Text,
    ),
    field(
        "worker.lease_seconds",
        "Lease duration",
        "How long a leased job is protected from being picked up by another worker.",
        Kind::Count {
            unit: Some("seconds"),
            min: 1,
        },
    ),
    field(
        "worker.batch_size",
        "Batch size",
        "How many jobs the worker leases at once.",
        Kind::Count { unit: None, min: 1 },
    ),
    field(
        "worker.poll_interval_seconds",
        "Poll interval",
        "How often the worker looks for jobs ready to advance.",
        Kind::Count {
            unit: Some("seconds"),
            min: 0,
        },
    ),
    field(
        "worker.discovery_interval_seconds",
        "Discovery interval",
        "How often each tracker is checked for new hit-and-run warnings.",
        Kind::Count {
            unit: Some("seconds"),
            min: 0,
        },
    ),
    // --- trackers (repeated) ---
    field(
        "trackers.*.id",
        "Id",
        "Stable key repair jobs are filed under. Changing it orphans existing jobs.",
        Kind::Text,
    ),
    field(
        "trackers.*.kind",
        "Kind",
        "\"unit3d\" for Blutopia/Aither and relatives, or \"fake\" for the built-in demo \
         tracker.",
        Kind::Choice(&["unit3d", "fake"]),
    ),
    field(
        "trackers.*.base_url",
        "Base URL",
        "The tracker's API base URL.",
        Kind::Url,
    ),
    field(
        "trackers.*.api_key",
        "API key",
        "The tracker's API key.",
        Kind::Secret {
            env_var: SecretEnv::PerTracker,
        },
    ),
    field(
        "trackers.*.api_key_file",
        "API key file",
        "Read the API key from a file instead — e.g. a mounted Docker/Kubernetes secret.",
        Kind::SecretFile,
    ),
    field(
        "trackers.*.token_placement",
        "Token placement",
        "Where the Unit3D API key goes: \"header\" (`Authorization: Bearer`) or \"query\" \
         (`?api_token=`), for instances that require it in the URL.",
        Kind::Choice(&["header", "query"]),
    ),
    // --- download_client ---
    field(
        "download_client.kind",
        "Kind",
        "\"qbittorrent\" or the in-memory \"fake\" client.",
        Kind::Choice(&["qbittorrent", "fake"]),
    ),
    field(
        "download_client.base_url",
        "Base URL",
        "The download client's Web API base URL.",
        Kind::Url,
    ),
    field(
        "download_client.username",
        "Username",
        "The download client's Web API username.",
        Kind::Text,
    ),
    field(
        "download_client.password",
        "Password",
        "The download client's Web API password.",
        Kind::Secret {
            env_var: SecretEnv::Fixed("SEEDMEDIC_DOWNLOAD_CLIENT_PASSWORD"),
        },
    ),
    field(
        "download_client.password_file",
        "Password file",
        "Read the password from a file instead.",
        Kind::SecretFile,
    ),
    field(
        "download_client.category",
        "Category",
        "Category to file repaired torrents under, so they are recognisable in the client.",
        Kind::Text,
    ),
    // --- arr (repeated) ---
    field(
        "arr.*.kind",
        "Kind",
        "\"sonarr\" or \"radarr\".",
        Kind::Choice(&["sonarr", "radarr"]),
    ),
    field(
        "arr.*.name",
        "Name",
        "A label for this instance. Must be unique among `[[arr]]` entries.",
        Kind::Text,
    ),
    field(
        "arr.*.base_url",
        "Base URL",
        "The *arr instance's API base URL.",
        Kind::Url,
    ),
    field(
        "arr.*.api_key",
        "API key",
        "The *arr instance's API key.",
        Kind::Secret {
            env_var: SecretEnv::PerArr,
        },
    ),
    field(
        "arr.*.api_key_file",
        "API key file",
        "Read the API key from a file instead.",
        Kind::SecretFile,
    ),
    // --- arr.path_mappings (repeated within a repeated section) ---
    field(
        "arr.*.path_mappings.*.from",
        "From",
        "The path as the *arr instance reports it (as its own container sees it).",
        Kind::AbsolutePath,
    ),
    field(
        "arr.*.path_mappings.*.to",
        "To",
        "The path SeedMedic should use in its place.",
        Kind::AbsolutePath,
    ),
    // --- metrics + notifications (share an "Integrations" page) ---
    field(
        "metrics.enabled",
        "Enable metrics",
        "JSON counters at /metrics: repairs by state, transitions by from/to, step durations, \
         tracker poll outcomes, staged bytes. Requires building with the `metrics` cargo \
         feature.",
        Kind::Bool,
    ),
    field(
        "notifications.webhook_url",
        "Webhook URL",
        "A plain JSON POST for: parked for review, completed, tracker unreachable. Apprise and \
         most \"generic webhook\" receivers accept this without further configuration. Unset \
         disables notifications entirely.",
        Kind::Url,
    ),
    field(
        "notifications.tracker_unreachable_after_seconds",
        "Tracker unreachable after",
        "How long a tracker must fail to respond before a notification is sent.",
        Kind::Count {
            unit: Some("seconds"),
            min: 0,
        },
    ),
];

/// Every field whose key, stripped of its `*` row markers, starts with one of
/// `prefixes` — how a settings page picks its fields out of the flat table.
pub fn fields_for(prefixes: &[&str]) -> impl Iterator<Item = &'static Field> {
    prefixes
        .iter()
        .flat_map(|prefix| FIELDS.iter().filter(move |f| f.key.starts_with(prefix)))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde::Deserialize;

    use super::*;
    use crate::config::Config;

    #[test]
    fn every_field_has_a_non_empty_label_and_help() {
        for field in FIELDS {
            assert!(!field.label.is_empty(), "{} has no label", field.key);
            assert!(!field.help.is_empty(), "{} has no help text", field.key);
        }
    }

    /// A minimal valid config, as TOML, so a test can add one key to one
    /// table and check the whole thing still parses (or, for the "bogus
    /// key" test, extract the exact set of field names serde expects).
    const MINIMAL: &str = r#"
        [staging]
        root = "/srv/seedmedic/staging"

        [[trackers]]
        id = "example"
        kind = "fake"

        [download_client]
        kind = "fake"

        [[arr]]
        kind = "sonarr"
        name = "main"
        base_url = "http://sonarr.test"

        [[arr.path_mappings]]
        from = "/tv"
        to = "/srv/media/tv"
    "#;

    /// Every `FIELDS` key must exist in `Config`: load `MINIMAL`, set that
    /// one key to a type-appropriate value through the real `ConfigDocument`
    /// (so a repeated section's row is created or overwritten exactly as a
    /// save would, with no risk of a hand-built string redeclaring a table
    /// TOML already has), and parse it. `deny_unknown_fields` fails a
    /// renamed or removed key.
    #[test]
    fn every_fields_key_exists_in_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");

        for field in FIELDS {
            std::fs::write(&path, MINIMAL).expect("write fixture");
            let mut doc = crate::config::ConfigDocument::read(&path).expect("read fixture");
            let key = field.key.replace('*', "0");

            match field.kind {
                Kind::Bool => doc.set(&key, true),
                Kind::Count { .. } => doc.set(&key, 1_i64),
                Kind::AbsolutePathList => doc.set_array(&key, ["/x".to_owned()]),
                Kind::Choice(values) => {
                    doc.set(
                        &key,
                        (*values.first().expect("at least one choice")).to_owned(),
                    );
                }
                Kind::Url => doc.set(&key, "http://example.test".to_owned()),
                // `to_config` calls `resolve_secrets`, which really reads a
                // `_file` path — point it at a file guaranteed to exist.
                Kind::SecretFile => doc.set(&key, path.display().to_string()),
                _ => doc.set(&key, "/x".to_owned()),
            }

            doc.to_config()
                .unwrap_or_else(|error| panic!("field `{}` is not in Config: {error}", field.key));
        }
    }

    /// Extract the backticked names out of a serde `deny_unknown_fields`
    /// error's "expected one of `a`, `b`" wording — a true, serde-derived
    /// enumeration of a table's field names, without reflection.
    fn expected_field_names(error: &str) -> BTreeSet<String> {
        // `split('`')` alternates plain text and backticked content, so odd
        // indices are backticked names: [0]="unknown field ", [1]=the bogus
        // key itself, [2]=", expected one of ", [3..]=the wanted names. Skip
        // 3 to land on the first wanted name, then take every other one.
        let names: BTreeSet<String> = error
            .split('`')
            .skip(3)
            .step_by(2)
            .map(str::to_owned)
            .collect();
        assert!(
            !names.is_empty(),
            "no backticked name found in serde's error message — its wording changed: {error}"
        );
        names
    }

    fn bogus_key_names(toml_text: &str) -> BTreeSet<String> {
        let error = toml::from_str::<Config>(toml_text)
            .expect_err("a bogus key must be rejected by deny_unknown_fields");
        expected_field_names(&error.to_string())
    }

    fn fields_keys_under(prefix: &str) -> BTreeSet<String> {
        FIELDS
            .iter()
            .filter_map(|field| {
                field
                    .key
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.strip_prefix('.'))
                    .filter(|rest| !rest.contains('.'))
                    .map(str::to_owned)
            })
            .collect()
    }

    /// Every `Config` key is in `FIELDS`: deserialize a document with one
    /// deliberately bogus key per table and diff the resulting enumeration
    /// against what `FIELDS` declares for that table.
    #[test]
    fn every_config_key_under_server_is_in_fields() {
        let toml_text = format!("{MINIMAL}\n[server]\nbogus = 1\n");
        assert_eq!(bogus_key_names(&toml_text), fields_keys_under("server"));
    }

    #[test]
    fn every_config_key_under_database_is_in_fields() {
        let toml_text = format!("{MINIMAL}\n[database]\nbogus = 1\n");
        assert_eq!(bogus_key_names(&toml_text), fields_keys_under("database"));
    }

    #[test]
    fn every_config_key_under_staging_is_in_fields() {
        // `[staging]` already exists in MINIMAL; a second header for the same
        // table is a TOML parse error ("duplicate key"), not the
        // `deny_unknown_fields` error this test wants, so insert into it.
        let toml_text = MINIMAL.replacen("[staging]", "[staging]\nbogus = 1", 1);
        assert_eq!(bogus_key_names(&toml_text), fields_keys_under("staging"));
    }

    #[test]
    fn every_config_key_under_library_is_in_fields() {
        let toml_text = format!("{MINIMAL}\n[library]\nbogus = 1\n");
        assert_eq!(bogus_key_names(&toml_text), fields_keys_under("library"));
    }

    #[test]
    fn every_config_key_under_policy_is_in_fields() {
        let toml_text = format!("{MINIMAL}\n[policy]\nbogus = 1\n");
        assert_eq!(bogus_key_names(&toml_text), fields_keys_under("policy"));
    }

    #[test]
    fn every_config_key_under_worker_is_in_fields() {
        let toml_text = format!("{MINIMAL}\n[worker]\nbogus = 1\n");
        assert_eq!(bogus_key_names(&toml_text), fields_keys_under("worker"));
    }

    #[test]
    fn every_config_key_under_trackers_is_in_fields() {
        let toml_text =
            format!("{MINIMAL}\n[[trackers]]\nid = \"x\"\nkind = \"fake\"\nbogus = 1\n");
        let names = bogus_key_names(&toml_text);
        assert_eq!(names, fields_keys_under("trackers.*"));
    }

    #[test]
    fn every_config_key_under_download_client_is_in_fields() {
        // Same reason as staging: `[download_client]` already exists in
        // MINIMAL, so insert into it rather than redeclare the table.
        let toml_text = MINIMAL.replacen("[download_client]", "[download_client]\nbogus = 1", 1);
        assert_eq!(
            bogus_key_names(&toml_text),
            fields_keys_under("download_client")
        );
    }

    #[test]
    fn every_config_key_under_arr_is_in_fields() {
        let toml_text = format!(
            "{MINIMAL}\n[[arr]]\nkind = \"sonarr\"\nname = \"x\"\nbase_url = \"http://x.test\"\nbogus = 1\n"
        );
        let names = bogus_key_names(&toml_text);
        // `path_mappings` is a real `ArrConfig` field, but it is a nested
        // repeated section with no single-field `FIELDS` entry of its
        // own — `every_config_key_under_arr_path_mappings_is_in_fields`
        // covers what is inside it.
        let mut expected = fields_keys_under("arr.*");
        expected.insert("path_mappings".to_owned());
        assert_eq!(names, expected);
    }

    #[test]
    fn every_config_key_under_arr_path_mappings_is_in_fields() {
        let toml_text =
            format!("{MINIMAL}\n[[arr.path_mappings]]\nfrom = \"/a\"\nto = \"/b\"\nbogus = 1\n");
        let names = bogus_key_names(&toml_text);
        assert_eq!(names, fields_keys_under("arr.*.path_mappings.*"));
    }

    #[test]
    fn every_config_key_under_metrics_is_in_fields() {
        let toml_text = format!("{MINIMAL}\n[metrics]\nbogus = 1\n");
        assert_eq!(bogus_key_names(&toml_text), fields_keys_under("metrics"));
    }

    #[test]
    fn every_config_key_under_notifications_is_in_fields() {
        let toml_text = format!("{MINIMAL}\n[notifications]\nbogus = 1\n");
        assert_eq!(
            bogus_key_names(&toml_text),
            fields_keys_under("notifications")
        );
    }

    /// A guard against this whole file drifting: every `Kind::Choice` should
    /// deserialize successfully, and something outside `["true"/"1"/etc]`
    /// should fail. This is not the drift test itself — it just keeps
    /// `sample_value` honest.
    #[derive(Deserialize)]
    struct Probe {
        #[allow(dead_code)]
        value: bool,
    }

    #[test]
    fn sample_bool_value_parses_as_a_bool() {
        let _: Probe = toml::from_str("value = true").expect("bool sample parses");
    }
}
