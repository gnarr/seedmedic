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

use crate::config::{ConfigDocument, Severity};

use super::fields::{Field, Kind};

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
