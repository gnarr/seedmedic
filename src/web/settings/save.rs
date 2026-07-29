//! Submit -> typed config: one pipeline, shared by every settings page.
//!
//! 1. `Form<Vec<(String, String)>>` — the pair list, in document order. A
//!    `Form<T>` with real fields cannot decode this: a field-level `Vec`
//!    forwards to `deserialize_any`, which only ever calls `visit_str` (see
//!    `src/web/AGENTS.md`), and a `bool` field would take the *first* value
//!    of a duplicated key rather than the last — the wrong half of the
//!    hidden-input convention below.
//! 2. Look each key up in `FIELDS`. Unknown keys are rejected.
//! 3. Per-`Kind` lexical parse into a clone of the current document.
//! 4. `draft.to_config()` — serde's authoritative check.
//! 5. `problems()` + `problems_on_disk()`. Errors block; warnings do not.
//! 6. Only then `save()`, then `RuntimeHandle::reload()`.

use std::{collections::BTreeMap, path::Path};

use toml_edit::Value;

use crate::config::{Config, ConfigDocument, SecretSource, Severity};

use super::fields::{Field, Kind};

/// The four `Kind::Secret` keys, resolved against the config already on
/// disk — `Unset` for anything else, including a repeated-section row that
/// does not exist yet.
///
/// Needed so `apply_pairs` can refuse to *write* an inline value over a
/// secret whose source is `Environment` or `File`: the UI never renders an
/// input for one (see `render::secret_input`), but nothing stops a crafted
/// `POST` from supplying `key=value` directly, and "managed outside
/// SeedMedic" must mean that regardless of how the request was made.
fn current_secret_source(before: &Config, key: &str) -> SecretSource {
    let parts: Vec<&str> = key.split('.').collect();
    let secret = match parts.as_slice() {
        ["server", "auth_token"] => Some(&before.server.auth_token),
        ["download_client", "password"] => before.download_client.as_ref().map(|c| &c.password),
        ["trackers", index, "api_key"] => index
            .parse::<usize>()
            .ok()
            .and_then(|i| before.trackers.get(i))
            .map(|t| &t.api_key),
        ["arr", index, "api_key"] => index
            .parse::<usize>()
            .ok()
            .and_then(|i| before.arr.get(i))
            .map(|a| &a.api_key),
        _ => None,
    };
    secret.map_or(SecretSource::Unset, |s| s.source().clone())
}

/// One field-level problem, ready to attach next to its input.
pub type FieldErrors = BTreeMap<String, String>;

/// At least one field (or the configuration as a whole) is invalid, so
/// nothing was written.
pub struct Invalid {
    pub errors: FieldErrors,
    /// Errors that do not belong to one field — surfaced once, above the
    /// form, rather than attached anywhere specific.
    pub general: Vec<String>,
}

/// Take every submitted value for a duplicated key; per the hidden-input
/// bool convention (a hidden `value="false"` immediately before a checkbox
/// of the same name), the checked state is whichever arrives last.
fn last_value_wins(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut deduped: Vec<(String, String)> = Vec::new();
    for (key, value) in pairs {
        match deduped.iter_mut().find(|(k, _)| *k == key) {
            Some(existing) => existing.1 = value,
            None => deduped.push((key, value)),
        }
    }
    deduped
}

/// Does `key` (concrete, e.g. `trackers.0.base_url`) match `template` (e.g.
/// `trackers.*.base_url`)? Segment-for-segment, `*` matching anything.
fn matches_template(template: &str, key: &str) -> bool {
    let template_parts = template.split('.');
    let key_parts = key.split('.');
    template_parts.clone().count() == key_parts.clone().count()
        && template_parts
            .zip(key_parts)
            .all(|(t, k)| t == "*" || t == k)
}

fn find_field<'a>(templates: &[&'a Field], key: &str) -> Option<&'a Field> {
    templates
        .iter()
        .find(|field| matches_template(field.key, key))
        .copied()
}

/// The seven keys where an empty submission means "unset", not `Some("")`
/// — see the "awkward cases" in docs/todos/0017-the-settings-pages.md.
/// `staging.root` is included: `Config` represents "unset" as an empty
/// `PathBuf`, which is exactly what removing the key also yields once
/// `#[serde(default)]` fills it back in.
const EMPTY_MEANS_ABSENT: &[&str] = &[
    "server.auth_token_file",
    "download_client.password_file",
    "download_client.category",
    "trackers.*.api_key_file",
    "arr.*.api_key_file",
    "notifications.webhook_url",
    "staging.root",
];

#[cfg_attr(test, derive(Debug))]
enum Parsed {
    Remove,
    Set(Value),
    SetArray(Vec<String>),
    /// A secret left blank with no `.clear` checkbox: leave the stored
    /// value alone. The only rule that is safe with an always-empty
    /// `value` — see `docs/todos/0017-the-settings-pages.md`.
    Unchanged,
}

fn parse_field(template_key: &str, kind: Kind, value: &str, clear: bool) -> Result<Parsed, String> {
    if EMPTY_MEANS_ABSENT.contains(&template_key) && value.trim().is_empty() {
        return Ok(Parsed::Remove);
    }

    match kind {
        Kind::Bool => Ok(Parsed::Set(Value::from(value == "true"))),
        Kind::Count { min, .. } => {
            let parsed: u64 = value
                .trim()
                .parse()
                .map_err(|_| format!("`{value}` is not a whole number"))?;
            if parsed < min {
                return Err(format!("must be at least {min}"));
            }
            Ok(Parsed::Set(Value::from(
                i64::try_from(parsed).map_err(|_| "is too large".to_owned())?,
            )))
        }
        Kind::Text => Ok(Parsed::Set(Value::from(value.to_owned()))),
        Kind::Url => {
            url::Url::parse(value).map_err(|error| format!("not a valid URL: {error}"))?;
            Ok(Parsed::Set(Value::from(value.to_owned())))
        }
        Kind::AbsolutePath => {
            if !Path::new(value).is_absolute() {
                return Err(format!("`{value}` must be an absolute path"));
            }
            Ok(Parsed::Set(Value::from(value.to_owned())))
        }
        Kind::AbsolutePathList => {
            let mut paths = Vec::new();
            for (number, line) in value.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if !Path::new(line).is_absolute() {
                    return Err(format!(
                        "line {}: `{line}` must be an absolute path",
                        number + 1
                    ));
                }
                paths.push(line.to_owned());
            }
            Ok(Parsed::SetArray(paths))
        }
        Kind::Choice(options) => {
            if !options.contains(&value) {
                return Err(format!("`{value}` is not one of the allowed values"));
            }
            Ok(Parsed::Set(Value::from(value.to_owned())))
        }
        Kind::Secret { .. } => {
            if clear {
                return Ok(Parsed::Remove);
            }
            if value.is_empty() {
                return Ok(Parsed::Unchanged);
            }
            Ok(Parsed::Set(Value::from(value.to_owned())))
        }
        // Display-only: never written, regardless of what a crafted POST
        // sends. This is what keeps `*_file` from becoming a remote
        // arbitrary-file-read primitive.
        Kind::SecretFile => Ok(Parsed::Unchanged),
    }
}

/// Everything but the row bookkeeping: dedup, split off `.clear` flags, look
/// every remaining key up in `templates`, and either apply it to `doc` or
/// record its field-level error. Returns the field-level errors.
///
/// `templates` may contain `*` wildcards; a page with a repeated section
/// should call [`gate_new_rows`] first and pass its result in `skip`, so a
/// blank "add another" block is never turned into a half-empty row.
pub fn apply_pairs(
    doc: &mut ConfigDocument,
    before: &Config,
    templates: &[&'static Field],
    pairs: Vec<(String, String)>,
    skip: &dyn Fn(&str) -> bool,
) -> Result<FieldErrors, String> {
    let pairs = last_value_wins(pairs);

    let mut clears = std::collections::HashSet::new();
    let mut confirms = std::collections::HashSet::new();
    let mut values = Vec::new();
    for (key, value) in pairs {
        if let Some(base) = key.strip_suffix(".clear") {
            if value == "true" {
                clears.insert(base.to_owned());
            }
        } else if let Some(base) = key.strip_suffix(".confirm") {
            if value == "true" {
                confirms.insert(base.to_owned());
            }
        } else {
            values.push((key, value));
        }
    }

    let mut errors = FieldErrors::new();
    for (key, value) in values {
        if skip(&key) {
            continue;
        }
        let Some(field) = find_field(templates, &key) else {
            return Err(format!("unknown settings field `{key}`"));
        };

        if field.danger.is_some()
            && current_as_string(doc, &key, field.kind) != value
            && !confirms.contains(&key)
        {
            errors.insert(
                key,
                "This is a dangerous change — check the confirmation box to save it.".to_owned(),
            );
            continue;
        }

        // Managed outside SeedMedic: the UI never renders an input for this,
        // but a crafted POST must not be able to write over it either.
        if matches!(field.kind, Kind::Secret { .. })
            && matches!(
                current_secret_source(before, &key),
                SecretSource::Environment { .. } | SecretSource::File { .. }
            )
        {
            continue;
        }

        let clear = clears.contains(&key);
        match parse_field(field.key, field.kind, &value, clear) {
            Ok(Parsed::Remove) => doc.remove(&key),
            Ok(Parsed::Set(v)) => doc.set(&key, v),
            Ok(Parsed::SetArray(items)) => doc.set_array(&key, items),
            Ok(Parsed::Unchanged) => {}
            Err(message) => {
                errors.insert(key, message);
            }
        }
    }
    Ok(errors)
}

/// The current value at `key`, as the same textual form a form field submits
/// — used only to detect whether a dangerous field is actually changing, so
/// resubmitting a page untouched never demands its confirmation checkbox.
fn current_as_string(doc: &ConfigDocument, key: &str, kind: Kind) -> String {
    if let Kind::Bool = kind {
        return doc
            .get(key)
            .and_then(toml_edit::Item::as_bool)
            .unwrap_or(false)
            .to_string();
    }
    match doc.get(key) {
        Some(toml_edit::Item::Value(value)) => {
            value.as_str().map(str::to_owned).unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// A submitted key's own row: `trackers.0.base_url` is row `("trackers", 0)`;
/// `arr.2.path_mappings.0.from` is row `("arr.2.path_mappings", 0)`. `None`
/// for a key with no repeated-section index at all.
fn row_scope(key: &str) -> Option<(&str, usize)> {
    let (row_path, _field) = key.rsplit_once('.')?;
    let (prefix, index) = row_path.rsplit_once('.').unwrap_or(("", row_path));
    let index: usize = index.parse().ok()?;
    Some((prefix.trim_end_matches('.'), index))
}

/// Does `key` belong to one of `section`'s gated-out new rows? Used by a
/// page's submit handler to build the `skip` predicate [`apply_pairs`] takes.
pub fn in_skipped_row(key: &str, section: &str, skip: &std::collections::HashSet<usize>) -> bool {
    matches!(row_scope(key), Some((prefix, index)) if prefix == section && skip.contains(&index))
}

/// Which new-row indices under `section` should be skipped entirely because
/// nothing meaningful was typed into them — the blank "add another" block
/// every repeated section renders one of. Only rows at or past `existing`
/// (the section's current length) are ever gated: an existing row is always
/// applied, even if that now makes it invalid, because blanking a required
/// field on a real row is the operator's mistake to be told about, not
/// something to silently ignore.
pub fn gate_new_rows(
    section: &str,
    existing: usize,
    templates: &[&'static Field],
    pairs: &[(String, String)],
) -> std::collections::HashSet<usize> {
    let mut meaningful: BTreeMap<usize, bool> = BTreeMap::new();

    for (key, value) in pairs {
        let Some((prefix, index)) = row_scope(key) else {
            continue;
        };
        if prefix != section || index < existing {
            continue;
        }
        let Some(field) = find_field(templates, key) else {
            continue;
        };
        let is_meaningful = !matches!(field.kind, Kind::Bool | Kind::Choice(_) | Kind::SecretFile)
            && !value.trim().is_empty();
        *meaningful.entry(index).or_insert(false) |= is_meaningful;
    }

    meaningful
        .into_iter()
        .filter(|(_, filled)| !filled)
        .map(|(index, _)| index)
        .collect()
}

/// Run steps 4-6 of the pipeline against an already-mutated draft: validate,
/// and only if nothing is wrong, save and report what to do next.
pub fn validate(draft: &ConfigDocument) -> Result<crate::config::Config, Invalid> {
    let config = match draft.to_config() {
        Ok(config) => config,
        Err(error) => {
            return Err(Invalid {
                errors: FieldErrors::new(),
                general: vec![error.to_string()],
            });
        }
    };

    let mut problems = config.problems();
    problems.extend(config.problems_on_disk());
    let errors: FieldErrors = problems
        .iter()
        .filter(|p| p.severity == Severity::Error)
        .filter_map(|p| p.key.clone().map(|key| (key, p.message.clone())))
        .collect();
    let general: Vec<String> = problems
        .iter()
        .filter(|p| p.severity == Severity::Error && p.key.is_none())
        .map(|p| p.message.clone())
        .collect();

    if errors.is_empty() && general.is_empty() {
        Ok(config)
    } else {
        Err(Invalid { errors, general })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::settings::fields::FIELDS;

    /// The `TempDir` must outlive the `ConfigDocument` returned alongside
    /// it — dropping it deletes the directory a later `save()` writes into.
    fn empty_doc() -> (tempfile::TempDir, ConfigDocument) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let doc = ConfigDocument::read(&path).expect("read a fresh path");
        (dir, doc)
    }

    fn field(key: &str) -> &'static Field {
        FIELDS
            .iter()
            .find(|f| f.key == key)
            .unwrap_or_else(|| panic!("no such field `{key}`"))
    }

    #[test]
    fn last_value_wins_implements_the_hidden_input_bool_convention() {
        let (_dir, mut doc) = empty_doc();
        let templates = [field("policy.prefer_reflink")];

        // Checked: the hidden `false` arrives first, the checkbox's `true`
        // arrives last.
        let errors = apply_pairs(
            &mut doc,
            &Config::default(),
            &templates,
            vec![
                ("policy.prefer_reflink".to_owned(), "false".to_owned()),
                ("policy.prefer_reflink".to_owned(), "true".to_owned()),
            ],
            &|_| false,
        )
        .expect("no unknown keys");
        assert!(errors.is_empty());
        assert_eq!(
            doc.get("policy.prefer_reflink").and_then(|i| i.as_bool()),
            Some(true)
        );

        // Unchecked: only the hidden `false` is submitted at all.
        let (_dir, mut doc) = empty_doc();
        apply_pairs(
            &mut doc,
            &Config::default(),
            &templates,
            vec![("policy.prefer_reflink".to_owned(), "false".to_owned())],
            &|_| false,
        )
        .expect("no unknown keys");
        assert_eq!(
            doc.get("policy.prefer_reflink").and_then(|i| i.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn an_unknown_form_key_is_rejected() {
        let (_dir, mut doc) = empty_doc();
        let templates = [field("policy.prefer_reflink")];
        let result = apply_pairs(
            &mut doc,
            &Config::default(),
            &templates,
            vec![("policy.not_a_real_field".to_owned(), "x".to_owned())],
            &|_| false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn an_empty_secret_submission_leaves_the_document_unchanged() {
        let (_dir, mut doc) = empty_doc();
        doc.set("server.auth_token", "already-set".to_owned());
        doc.save().expect("seed the file");

        let mut doc = ConfigDocument::read(doc.path()).expect("reread");
        let templates = [field("server.auth_token")];
        apply_pairs(
            &mut doc,
            &Config::default(),
            &templates,
            vec![("server.auth_token".to_owned(), String::new())],
            &|_| false,
        )
        .expect("applies");

        assert_eq!(
            doc.get("server.auth_token").and_then(|i| i.as_str()),
            Some("already-set"),
            "an empty secret submission must not clear the stored value"
        );
    }

    #[test]
    fn clearing_a_secret_removes_it() {
        let (_dir, mut doc) = empty_doc();
        doc.set("server.auth_token", "already-set".to_owned());
        doc.save().expect("seed the file");

        let mut doc = ConfigDocument::read(doc.path()).expect("reread");
        let templates = [field("server.auth_token")];
        apply_pairs(
            &mut doc,
            &Config::default(),
            &templates,
            vec![
                ("server.auth_token".to_owned(), String::new()),
                ("server.auth_token.clear".to_owned(), "true".to_owned()),
            ],
            &|_| false,
        )
        .expect("applies");

        assert!(doc.get("server.auth_token").is_none());
    }

    #[test]
    fn a_secret_file_field_is_never_written_even_if_submitted() {
        let (_dir, mut doc) = empty_doc();
        let templates = [field("server.auth_token_file")];
        apply_pairs(
            &mut doc,
            &Config::default(),
            &templates,
            vec![(
                "server.auth_token_file".to_owned(),
                "/etc/shadow".to_owned(),
            )],
            &|_| false,
        )
        .expect("applies");

        assert!(
            doc.get("server.auth_token_file").is_none(),
            "a SecretFile field must be display-only"
        );
    }

    /// A crafted `POST` must not be able to write over a secret that is
    /// managed outside SeedMedic, even though the UI never renders an input
    /// for one — the same "managed outside SeedMedic" property `SecretFile`
    /// already gets, extended to the env/file-sourced case of `Secret`
    /// itself.
    #[test]
    fn a_secret_sourced_from_the_environment_is_never_overwritten_by_a_submission() {
        // SAFETY: this test does not spawn other threads that read the
        // environment concurrently.
        unsafe { std::env::set_var("SEEDMEDIC_SERVER_AUTH_TOKEN", "from-env") };
        // `load_from` resolves secrets even for a nonexistent path (the
        // fresh-install case); `resolve_secrets` itself is private to
        // `config` and not reachable from here.
        let before = Config::load_from(std::path::Path::new("/nonexistent/config.toml"))
            .expect("a missing file still resolves secrets");
        unsafe { std::env::remove_var("SEEDMEDIC_SERVER_AUTH_TOKEN") };
        assert!(matches!(
            before.server.auth_token.source(),
            crate::config::SecretSource::Environment { .. }
        ));

        let (_dir, mut doc) = empty_doc();
        let templates = [field("server.auth_token")];
        apply_pairs(
            &mut doc,
            &before,
            &templates,
            vec![(
                "server.auth_token".to_owned(),
                "attacker-supplied".to_owned(),
            )],
            &|_| false,
        )
        .expect("applies");

        assert!(
            doc.get("server.auth_token").is_none(),
            "an env-sourced secret must never be written inline"
        );
    }

    #[test]
    fn absolute_path_list_parses_lines_ignoring_blanks_and_trimming_whitespace() {
        match parse_field(
            "library.roots",
            Kind::AbsolutePathList,
            "/a  \n\n/b\t\n  /c\n",
            false,
        )
        .expect("parses")
        {
            Parsed::SetArray(items) => assert_eq!(items, vec!["/a", "/b", "/c"]),
            _ => panic!("expected SetArray"),
        }
    }

    #[test]
    fn absolute_path_list_rejects_a_relative_line_naming_it() {
        let error = parse_field(
            "library.roots",
            Kind::AbsolutePathList,
            "/a\nrelative/path\n/c\n",
            false,
        )
        .expect_err("a relative line is rejected");
        assert!(error.contains("line 2"));
        assert!(error.contains("relative/path"));
    }

    /// Step 3 (the per-`Kind` lexical parse) must catch every bad value on
    /// its own, so step 4's `to_config` error is a last resort that a
    /// well-behaved page never actually reaches.
    #[test]
    fn a_bad_value_produces_a_field_level_error_for_every_field_that_can_have_one() {
        for f in FIELDS {
            let bad = match f.kind {
                Kind::Bool | Kind::Secret { .. } | Kind::SecretFile | Kind::Text => continue,
                Kind::Count { .. } => "not-a-number",
                Kind::Url => "not a url",
                Kind::AbsolutePath => "relative/path",
                Kind::AbsolutePathList => "relative/path",
                Kind::Choice(_) => "not-a-valid-choice",
            };
            assert!(
                parse_field(f.key, f.kind, bad, false).is_err(),
                "{} accepted the bad value `{bad}`",
                f.key
            );
        }
    }

    #[test]
    fn a_dangerous_field_needs_its_confirmation_box_checked_to_change() {
        let (_dir, mut doc) = empty_doc();
        let templates = [field("policy.allow_hardlink")];

        // Changing it without the confirmation box is a field-level error,
        // and the document stays untouched.
        let errors = apply_pairs(
            &mut doc,
            &Config::default(),
            &templates,
            vec![
                ("policy.allow_hardlink".to_owned(), "false".to_owned()),
                ("policy.allow_hardlink".to_owned(), "true".to_owned()),
            ],
            &|_| false,
        )
        .expect("no unknown keys");
        assert!(errors.contains_key("policy.allow_hardlink"));
        assert!(doc.get("policy.allow_hardlink").is_none());

        // With it checked, the change applies.
        let errors = apply_pairs(
            &mut doc,
            &Config::default(),
            &templates,
            vec![
                ("policy.allow_hardlink".to_owned(), "false".to_owned()),
                ("policy.allow_hardlink".to_owned(), "true".to_owned()),
                (
                    "policy.allow_hardlink.confirm".to_owned(),
                    "true".to_owned(),
                ),
            ],
            &|_| false,
        )
        .expect("no unknown keys");
        assert!(errors.is_empty());
        assert_eq!(
            doc.get("policy.allow_hardlink").and_then(|i| i.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn resubmitting_a_page_untouched_never_demands_the_confirmation_box() {
        let (_dir, mut doc) = empty_doc();
        let templates = [field("policy.allow_hardlink")];
        // The current (absent = false) value, submitted unchanged.
        let errors = apply_pairs(
            &mut doc,
            &Config::default(),
            &templates,
            vec![("policy.allow_hardlink".to_owned(), "false".to_owned())],
            &|_| false,
        )
        .expect("no unknown keys");
        assert!(errors.is_empty());
    }

    #[test]
    fn gate_new_rows_skips_a_blank_add_block_but_not_a_filled_one() {
        let templates = [field("trackers.*.id"), field("trackers.*.kind")];

        let blank = vec![
            ("trackers.0.id".to_owned(), String::new()),
            ("trackers.0.kind".to_owned(), "fake".to_owned()),
        ];
        let skip = gate_new_rows("trackers", 0, &templates, &blank);
        assert!(skip.contains(&0), "an all-blank add block must be skipped");

        let filled = vec![
            ("trackers.0.id".to_owned(), "demo".to_owned()),
            ("trackers.0.kind".to_owned(), "fake".to_owned()),
        ];
        let skip = gate_new_rows("trackers", 0, &templates, &filled);
        assert!(
            !skip.contains(&0),
            "a row with a meaningful field must not be skipped"
        );
    }

    #[test]
    fn gate_new_rows_never_skips_an_existing_row() {
        let templates = [field("trackers.*.id")];
        let blank = vec![("trackers.0.id".to_owned(), String::new())];
        // `existing = 1` means row 0 already exists on disk.
        let skip = gate_new_rows("trackers", 1, &templates, &blank);
        assert!(skip.is_empty());
    }
}
