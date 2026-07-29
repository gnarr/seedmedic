//! Turning `FIELDS` plus a [`ConfigDocument`] into HTML forms.
//!
//! Nothing here decides whether a value is valid — that is `Config`'s job,
//! run by `save.rs` before anything is written. This module only shows what
//! is on disk (or, absent that, an empty field — never a fabricated
//! default, so nobody mistakes a placeholder for a saved value) and, for a
//! secret, what `Config` resolved it to.

use std::collections::BTreeMap;

use maud::{Markup, html};
use toml_edit::Item;

use crate::config::{ConfigDocument, Secret, SecretSource};

use super::fields::{Field, Kind, SecretEnv};

/// What a field should show: the operator's own unsaved typing when a save
/// just failed (so a mistake elsewhere on the page does not erase it), else
/// whatever is on disk, else blank — never a fabricated default, so a blank
/// field cannot be mistaken for a saved value.
pub type Overrides = BTreeMap<String, String>;

fn text_value(doc: &ConfigDocument, overrides: &Overrides, key: &str) -> String {
    if let Some(value) = overrides.get(key) {
        return value.clone();
    }
    match doc.get(key) {
        Some(Item::Value(value)) => match value.as_str() {
            Some(s) => s.to_owned(),
            None => value
                .as_integer()
                .map(|n| n.to_string())
                .unwrap_or_default(),
        },
        _ => String::new(),
    }
}

fn bool_value(doc: &ConfigDocument, overrides: &Overrides, key: &str) -> bool {
    if let Some(value) = overrides.get(key) {
        return value == "true";
    }
    doc.get(key).and_then(Item::as_bool).unwrap_or(false)
}

fn lines_value(doc: &ConfigDocument, overrides: &Overrides, key: &str) -> String {
    if let Some(value) = overrides.get(key) {
        return value.clone();
    }
    match doc.get(key).and_then(Item::as_array) {
        Some(array) => array
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

/// One field's label, help text, input, and (if present) error message.
/// `secret` must be `Some` for `Kind::Secret` fields — the actual resolved
/// value, with its source, since that cannot be read off `doc` alone (an
/// environment- or file-sourced secret may have no inline value at all).
pub fn field_row(
    doc: &ConfigDocument,
    overrides: &Overrides,
    field: &Field,
    key: &str,
    secret: Option<&Secret>,
    error: Option<&str>,
) -> Markup {
    html! {
        div.field {
            label for=(key) { (field.label) }
            @if let Some(warning) = field.danger {
                p.notice.danger { (warning) }
            }
            (input(doc, overrides, field, key, secret))
            @if field.danger.is_some() {
                label.confirm {
                    input type="checkbox" name={ (key) ".confirm" } value="true";
                    " I understand, and want to change this"
                }
            }
            @if field.restart_required {
                p.help { "Restart required to take effect." }
            }
            p.help { (field.help) }
            @if let Some(message) = error {
                p.error { (message) }
            }
        }
    }
}

fn input(
    doc: &ConfigDocument,
    overrides: &Overrides,
    field: &Field,
    key: &str,
    secret: Option<&Secret>,
) -> Markup {
    match field.kind {
        Kind::Bool => {
            let value = bool_value(doc, overrides, key);
            // On a build without the `metrics` feature this checkbox would
            // enable nothing — /metrics is `#[cfg]`-gated — so say that
            // instead of letting an operator toggle a setting with no effect.
            if key == "metrics.enabled" && !cfg!(feature = "metrics") {
                html! {
                    // Carries the unchanged current value: a disabled
                    // checkbox is not submitted at all, and resubmitting this
                    // page must not silently flip a setting the operator did
                    // not touch just because this build cannot act on it.
                    input type="hidden" name=(key) value=(value.to_string());
                    input type="checkbox" disabled checked[value];
                    p.help { "This build was not compiled with the `metrics` feature, so this \
                              setting has no effect." }
                }
            } else {
                html! {
                    input type="hidden" name=(key) value="false";
                    input type="checkbox" id=(key) name=(key) value="true" checked[value];
                }
            }
        }
        Kind::Count { unit, min } => {
            let value = text_value(doc, overrides, key);
            html! {
                input type="number" id=(key) name=(key) min=(min) value=(value);
                @if let Some(unit) = unit { span.unit { (unit) } }
            }
        }
        Kind::Text => {
            let value = text_value(doc, overrides, key);
            html! { input type="text" id=(key) name=(key) value=(value); }
        }
        Kind::Url => {
            let value = text_value(doc, overrides, key);
            html! { input type="text" id=(key) name=(key) value=(value) placeholder="https://…"; }
        }
        Kind::AbsolutePath => {
            let value = text_value(doc, overrides, key);
            html! { input type="text" id=(key) name=(key) value=(value) placeholder="/…"; }
        }
        Kind::AbsolutePathList => {
            let value = lines_value(doc, overrides, key);
            html! { textarea id=(key) name=(key) rows="4" { (value) } }
        }
        Kind::Choice(options) => {
            let value = text_value(doc, overrides, key);
            html! {
                select id=(key) name=(key) {
                    @for option in options {
                        option value=(option) selected[*option == value] { (option) }
                    }
                }
            }
        }
        Kind::Secret { env_var } => secret_input(key, secret, &env_var),
        Kind::SecretFile => {
            let value = text_value(doc, overrides, key);
            html! {
                @if value.is_empty() {
                    span.muted { "not set" }
                } @else {
                    code { (value) }
                }
            }
        }
    }
}

fn secret_input(key: &str, secret: Option<&Secret>, env_var: &SecretEnv) -> Markup {
    let secret = secret.expect("Kind::Secret fields are always rendered with a resolved Secret");
    html! {
        p.secret-status {
            @match secret.source() {
                SecretSource::Unset => "Not set",
                SecretSource::Environment { var } => (format!("Set — from {var}")),
                SecretSource::File { path } => (format!("Set — from {}", path.display())),
                SecretSource::Inline => "Set — in config.toml",
            }
        }
        @match secret.source() {
            SecretSource::Environment { .. } | SecretSource::File { .. } => {
                p.help {
                    "Managed outside SeedMedic. Unset the variable or clear the `_file` setting \
                     to edit it here."
                }
            }
            SecretSource::Inline | SecretSource::Unset => {
                input type="password" id=(key) name=(key) value="" autocomplete="new-password"
                    placeholder=(if secret.is_empty() { "" } else { "••••••••" });
                label.clear {
                    input type="checkbox" name={ (key) ".clear" } value="true";
                    " Clear"
                }
                p.help { "Can also be set via " code { (env_var.describe(None)) } "." }
            }
        }
    }
}

/// A whole non-repeated page: every field in `fields`, in order, as one
/// form. `errors` is keyed by the concrete field key.
pub fn simple_form(
    doc: &ConfigDocument,
    overrides: &Overrides,
    fields: &[(&'static Field, Option<&Secret>)],
    errors: &BTreeMap<String, String>,
) -> Markup {
    html! {
        form method="post" {
            @for (field, secret) in fields {
                (field_row(doc, overrides, field, field.key, *secret, errors.get(field.key).map(String::as_str)))
            }
            div.actions { button type="submit" { "Save" } }
        }
    }
}

/// Substitute each `*` in a template key, in order, with the given indices:
/// `concrete_key("arr.*.path_mappings.*.from", &[2, 0])` is
/// `"arr.2.path_mappings.0.from"`.
pub fn concrete_key(template: &str, indices: &[usize]) -> String {
    let mut indices = indices.iter();
    template
        .split('.')
        .map(|part| {
            if part == "*" {
                indices
                    .next()
                    .expect("as many indices as wildcards in this template")
                    .to_string()
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// One row of a repeated section: every `row_fields` field, addressed at
/// `indices`, plus a remove link (never for the trailing blank "add
/// another" row).
pub fn repeated_row(
    doc: &ConfigDocument,
    overrides: &Overrides,
    row_fields: &[(&'static Field, Option<&Secret>)],
    indices: &[usize],
    errors: &BTreeMap<String, String>,
    remove_href: Option<&str>,
) -> Markup {
    html! {
        fieldset.row {
            @for (field, secret) in row_fields {
                @let key = concrete_key(field.key, indices);
                (field_row(doc, overrides, field, &key, *secret, errors.get(&key).map(String::as_str)))
            }
            @if let Some(href) = remove_href {
                p { a.danger href=(href) { "Remove" } }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, web::settings::fields::FIELDS};

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

    /// Render every non-repeated, non-`SecretFile` field on a config whose
    /// every secret is a distinct `SENTINEL-<n>`, and assert none leaks into
    /// the HTML — the same guarantee `ConfigDocument::to_redacted_toml`
    /// carries for the escape-hatch TOML, checked here for the actual page.
    #[test]
    fn no_settings_page_ever_renders_a_sentinel_secret_value() {
        let (_dir, mut doc) = empty_doc();
        doc.set("server.auth_token", "SENTINEL-1".to_owned());
        doc.set("download_client.password", "SENTINEL-2".to_owned());
        doc.set("trackers.0.api_key", "SENTINEL-3".to_owned());
        doc.set("arr.0.api_key", "SENTINEL-4".to_owned());

        let auth_token = Secret::new("SENTINEL-1");
        let password = Secret::new("SENTINEL-2");
        let tracker_key = Secret::new("SENTINEL-3");
        let arr_key = Secret::new("SENTINEL-4");
        let overrides = Overrides::new();
        let errors = BTreeMap::new();

        let mut html = String::new();
        html += &field_row(
            &doc,
            &overrides,
            field("server.auth_token"),
            "server.auth_token",
            Some(&auth_token),
            None,
        )
        .into_string();
        html += &field_row(
            &doc,
            &overrides,
            field("download_client.password"),
            "download_client.password",
            Some(&password),
            None,
        )
        .into_string();
        html += &repeated_row(
            &doc,
            &overrides,
            &[(field("trackers.*.api_key"), Some(&tracker_key))],
            &[0],
            &errors,
            None,
        )
        .into_string();
        html += &repeated_row(
            &doc,
            &overrides,
            &[(field("arr.*.api_key"), Some(&arr_key))],
            &[0],
            &errors,
            None,
        )
        .into_string();

        for sentinel in ["SENTINEL-1", "SENTINEL-2", "SENTINEL-3", "SENTINEL-4"] {
            assert!(!html.contains(sentinel), "{sentinel} leaked into: {html}");
        }
    }

    /// An environment-sourced secret must show which variable it came from
    /// and render no editable input at all — never a value, and never a
    /// route for the operator to accidentally overwrite it inline.
    #[test]
    fn an_environment_sourced_secret_shows_the_variable_and_no_input() {
        // SAFETY: this test does not spawn other threads that read the
        // environment concurrently.
        unsafe { std::env::set_var("SEEDMEDIC_SERVER_AUTH_TOKEN", "SENTINEL-5") };
        let config = Config::load_from(std::path::Path::new("/nonexistent/config.toml"))
            .expect("a missing file still resolves secrets");
        unsafe { std::env::remove_var("SEEDMEDIC_SERVER_AUTH_TOKEN") };

        let (_dir, doc) = empty_doc();
        let html = field_row(
            &doc,
            &Overrides::new(),
            field("server.auth_token"),
            "server.auth_token",
            Some(&config.server.auth_token),
            None,
        )
        .into_string();

        assert!(!html.contains("SENTINEL-5"));
        assert!(html.contains("SEEDMEDIC_SERVER_AUTH_TOKEN"));
        assert!(!html.contains("type=\"password\""));
    }
}
