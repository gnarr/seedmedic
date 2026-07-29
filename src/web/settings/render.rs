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
