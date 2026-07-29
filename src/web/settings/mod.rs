//! `/settings`: viewing and editing every `Config` key from the browser.
//! See `docs/todos/0017-the-settings-pages.md` for the design this follows,
//! and `src/web/AGENTS.md` for the traps already hit once (`Form<Vec<(String,
//! String)>>`, the hidden-input bool convention, never calling `expose()`).

mod fields;
mod render;
mod save;

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
};

use axum::{
    Router,
    extract::{Form, Path as AxumPath, State},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use maud::html;

use crate::config::{Config, ConfigDocument, Secret};

use super::{AppState, error::WebError, layout};
use fields::{Field, Kind, fields_for};
use render::Overrides;
use save::{FieldErrors, apply_pairs, gate_new_rows, in_skipped_row, validate};

struct Page {
    slug: &'static str,
    title: &'static str,
    prefixes: &'static [&'static str],
}

const PAGES: &[Page] = &[
    Page {
        slug: "server",
        title: "Server",
        prefixes: &["server.", "database."],
    },
    Page {
        slug: "staging",
        title: "Staging",
        prefixes: &["staging."],
    },
    Page {
        slug: "library",
        title: "Library",
        prefixes: &["library."],
    },
    Page {
        slug: "policy",
        title: "Policy",
        prefixes: &["policy."],
    },
    Page {
        slug: "worker",
        title: "Worker",
        prefixes: &["worker."],
    },
    Page {
        slug: "download-client",
        title: "Download client",
        prefixes: &["download_client."],
    },
    Page {
        slug: "integrations",
        title: "Integrations",
        prefixes: &["metrics.", "notifications."],
    },
];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings", get(index))
        .route(
            "/settings/trackers",
            get(trackers_page).post(trackers_submit),
        )
        .route(
            "/settings/trackers/{index}/remove",
            get(trackers_remove_confirm).post(trackers_remove),
        )
        .route("/settings/arr", get(arr_page).post(arr_submit))
        .route(
            "/settings/arr/{index}/remove",
            get(arr_remove_confirm).post(arr_remove),
        )
        .route(
            "/settings/arr/{index}/path-mappings/{sub}/remove",
            post(arr_path_mapping_remove),
        )
        .route("/settings/load-demo", post(load_demo))
        .route("/settings/{slug}", get(simple_page).post(simple_submit))
}

fn open_document(state: &AppState) -> Result<ConfigDocument, WebError> {
    ConfigDocument::read(state.runtime.config_path())
        .map_err(|error| WebError::Refused(error.to_string()))
}

fn require_writable(doc: &ConfigDocument) -> Result<(), WebError> {
    if doc.writable() {
        Ok(())
    } else {
        Err(WebError::Refused(format!(
            "{} is not writable, so nothing here can be saved. Fix the file or directory \
             permissions, or the mount it lives on.",
            doc.path().display()
        )))
    }
}

async fn index(State(state): State<AppState>) -> Result<Response, WebError> {
    let runtime = state.runtime.current();
    let doc = open_document(&state)?;

    let body = html! {
        @if !doc.writable() {
            div.notice.danger {
                p {
                    strong { (doc.path().display().to_string()) }
                    " is not writable. Settings can be viewed here but not saved."
                }
                details {
                    summary { "Show the current configuration as TOML (secrets redacted)" }
                    pre { (doc.to_redacted_toml()) }
                }
            }
        }
        ul {
            @for page in PAGES {
                li { a href={ "/settings/" (page.slug) } { (page.title) } }
            }
            li { a href="/settings/trackers" { "Trackers" } }
            li { a href="/settings/arr" { "Arr instances" } }
        }
        @if can_load_demo(&runtime.config) {
            form method="post" action="/settings/load-demo" {
                button type="submit" { "Load demo configuration" }
            }
        }
    };

    Ok(layout::page(&runtime.chrome, "Settings", body).into_response())
}

#[cfg(feature = "fakes")]
fn can_load_demo(config: &Config) -> bool {
    config.trackers.is_empty()
}

#[cfg(not(feature = "fakes"))]
fn can_load_demo(_config: &Config) -> bool {
    false
}

fn secret_for<'a>(config: &'a Config, key: &str, fallback: &'a Secret) -> &'a Secret {
    match key {
        "server.auth_token" => &config.server.auth_token,
        "download_client.password" => config
            .download_client
            .as_ref()
            .map(|client| &client.password)
            .unwrap_or(fallback),
        _ => fallback,
    }
}

async fn simple_page(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
) -> Result<Response, WebError> {
    render_simple_page(&state, &slug, &Overrides::new(), &FieldErrors::new(), &[]).await
}

async fn render_simple_page(
    state: &AppState,
    slug: &str,
    overrides: &Overrides,
    errors: &FieldErrors,
    general: &[String],
) -> Result<Response, WebError> {
    let Some(page) = PAGES.iter().find(|p| p.slug == slug) else {
        return Err(WebError::NotFound);
    };
    let runtime = state.runtime.current();
    let doc = open_document(state)?;
    let config = doc.to_config().unwrap_or_default();
    let default_secret = Secret::default();

    let entries: Vec<(&'static Field, Option<&Secret>)> = fields_for(page.prefixes)
        .map(|field| {
            let secret = matches!(field.kind, Kind::Secret { .. })
                .then(|| secret_for(&config, field.key, &default_secret));
            (field, secret)
        })
        .collect();

    let body = html! {
        h2 { (page.title) }
        @for message in general {
            div.notice.danger { p { (message) } }
        }
        (render::simple_form(&doc, overrides, &entries, errors))
        p { a href="/settings" { "Back to settings" } }
    };

    Ok(layout::page(&runtime.chrome, page.title, body).into_response())
}

async fn simple_submit(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let Some(page) = PAGES.iter().find(|p| p.slug == slug) else {
        return Err(WebError::NotFound);
    };
    let templates: Vec<&'static Field> = fields_for(page.prefixes).collect();
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    let overrides: Overrides = pairs.iter().cloned().collect();

    match apply_pairs(&mut doc, &templates, pairs, &|_| false) {
        Err(message) => Err(WebError::Refused(message)),
        Ok(errors) if !errors.is_empty() => {
            render_simple_page(&state, &slug, &overrides, &errors, &[]).await
        }
        Ok(_) => match validate(&doc) {
            Err(invalid) => {
                render_simple_page(&state, &slug, &overrides, &invalid.errors, &invalid.general)
                    .await
            }
            Ok(_) => save_and_reload(&state, doc, &format!("/settings/{slug}")).await,
        },
    }
}

async fn save_and_reload(
    state: &AppState,
    doc: ConfigDocument,
    back_href: &str,
) -> Result<Response, WebError> {
    doc.save()
        .map_err(|error| WebError::Refused(error.to_string()))?;
    let applied = state
        .runtime
        .reload()
        .await
        .map_err(|error| WebError::Refused(error.to_string()))?;

    let body = html! {
        h2 { "Saved" }
        p { "The configuration was saved." }
        @if !applied.restart_needed.is_empty() {
            div.notice {
                strong { "Restart required for: " (applied.restart_needed.join(", ")) }
            }
        }
        p { a href=(back_href) { "Back" } }
    };
    Ok(layout::page(&state.runtime.current().chrome, "Saved", body).into_response())
}

// --- trackers (repeated) ---

fn tracker_row_fields() -> Vec<&'static Field> {
    fields_for(&["trackers.*"]).collect()
}

async fn trackers_page(State(state): State<AppState>) -> Result<Response, WebError> {
    render_trackers_page(&state, &Overrides::new(), &FieldErrors::new(), &[]).await
}

async fn render_trackers_page(
    state: &AppState,
    overrides: &Overrides,
    errors: &FieldErrors,
    general: &[String],
) -> Result<Response, WebError> {
    let runtime = state.runtime.current();
    let doc = open_document(state)?;
    let config = doc.to_config().unwrap_or_default();
    let default_secret = Secret::default();
    let row_fields = tracker_row_fields();
    let existing = doc.row_count("trackers");

    let body = html! {
        h2 { "Trackers" }
        @for message in general {
            div.notice.danger { p { (message) } }
        }
        form method="post" {
            @for row in 0..=existing {
                @let secret_ref = config.trackers.get(row).map(|t| &t.api_key).unwrap_or(&default_secret);
                @let entries = row_entries(&row_fields, secret_ref);
                @let remove_href = (row < existing).then(|| format!("/settings/trackers/{row}/remove"));
                (render::repeated_row(&doc, overrides, &entries, &[row], errors, remove_href.as_deref()))
            }
            div.actions { button type="submit" { "Save" } }
        }
        p { a href="/settings" { "Back to settings" } }
    };

    Ok(layout::page(&runtime.chrome, "Trackers", body).into_response())
}

fn row_entries<'a>(
    row_fields: &[&'static Field],
    secret: &'a Secret,
) -> Vec<(&'static Field, Option<&'a Secret>)> {
    row_fields
        .iter()
        .map(|field| {
            let secret = matches!(field.kind, Kind::Secret { .. }).then_some(secret);
            (*field, secret)
        })
        .collect()
}

async fn trackers_submit(
    State(state): State<AppState>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    let templates = tracker_row_fields();
    let existing = doc.row_count("trackers");
    let overrides: Overrides = pairs.iter().cloned().collect();
    let skip = gate_new_rows("trackers", existing, &templates, &pairs);

    match apply_pairs(&mut doc, &templates, pairs, &|key| {
        in_skipped_row(key, "trackers", &skip)
    }) {
        Err(message) => Err(WebError::Refused(message)),
        Ok(errors) if !errors.is_empty() => {
            render_trackers_page(&state, &overrides, &errors, &[]).await
        }
        Ok(_) => match validate(&doc) {
            Err(invalid) => {
                render_trackers_page(&state, &overrides, &invalid.errors, &invalid.general).await
            }
            Ok(_) => save_and_reload(&state, doc, "/settings/trackers").await,
        },
    }
}

async fn trackers_remove_confirm(
    State(state): State<AppState>,
    AxumPath(index): AxumPath<usize>,
) -> Result<Response, WebError> {
    let runtime = state.runtime.current();
    let doc = open_document(&state)?;
    let config = doc.to_config().unwrap_or_default();
    let Some(tracker) = config.trackers.get(index) else {
        return Err(WebError::NotFound);
    };
    let unfinished = runtime.deps.store.unfinished().await?;
    let affected = unfinished
        .iter()
        .filter(|job| job.tracker.as_str() == tracker.id)
        .count();

    let body = html! {
        h2 { "Remove tracker " (tracker.id) "?" }
        @if affected > 0 {
            div.notice.danger {
                p {
                    "This will orphan " (affected) " unfinished repair(s) filed under this \
                     tracker — they will no longer have a tracker to poll."
                }
            }
        } @else {
            p { "No unfinished repairs are filed under this tracker." }
        }
        form method="post" {
            button.danger type="submit" { "Remove tracker " (tracker.id) }
        }
        p { a href="/settings/trackers" { "Cancel" } }
    };
    Ok(layout::page(&runtime.chrome, "Remove tracker", body).into_response())
}

async fn trackers_remove(
    State(state): State<AppState>,
    AxumPath(index): AxumPath<usize>,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    doc.remove_row("trackers", index);

    match validate(&doc) {
        Err(invalid) => Err(WebError::Refused(invalid_message(&invalid))),
        Ok(_) => save_and_reload(&state, doc, "/settings/trackers").await,
    }
}

fn invalid_message(invalid: &save::Invalid) -> String {
    invalid
        .general
        .iter()
        .cloned()
        .chain(invalid.errors.values().cloned())
        .collect::<Vec<_>>()
        .join("; ")
}

// --- arr (repeated, with path_mappings nested inside each row) ---

fn arr_row_fields() -> Vec<&'static Field> {
    fields_for(&["arr.*"])
        .filter(|field| !field.key.contains("path_mappings"))
        .collect()
}

fn mapping_row_fields() -> Vec<&'static Field> {
    fields_for(&["arr.*.path_mappings.*"]).collect()
}

async fn arr_page(State(state): State<AppState>) -> Result<Response, WebError> {
    render_arr_page(&state, &Overrides::new(), &FieldErrors::new(), &[]).await
}

async fn render_arr_page(
    state: &AppState,
    overrides: &Overrides,
    errors: &FieldErrors,
    general: &[String],
) -> Result<Response, WebError> {
    let runtime = state.runtime.current();
    let doc = open_document(state)?;
    let config = doc.to_config().unwrap_or_default();
    let default_secret = Secret::default();
    let row_fields = arr_row_fields();
    let mapping_fields = mapping_row_fields();
    let existing = doc.row_count("arr");

    let body = html! {
        h2 { "Arr instances" }
        @for message in general {
            div.notice.danger { p { (message) } }
        }
        form method="post" {
            @for row in 0..=existing {
                @let secret_ref = config.arr.get(row).map(|a| &a.api_key).unwrap_or(&default_secret);
                @let entries = row_entries(&row_fields, secret_ref);
                @let remove_href = (row < existing).then(|| format!("/settings/arr/{row}/remove"));
                (render::repeated_row(&doc, overrides, &entries, &[row], errors, remove_href.as_deref()))

                @if row < existing {
                    @let mapping_existing = doc.row_count(&format!("arr.{row}.path_mappings"));
                    div.sub {
                        h4 { "Path mappings" }
                        @for sub in 0..=mapping_existing {
                            @let mapping_entries = row_entries(&mapping_fields, &default_secret);
                            @let mapping_remove = (sub < mapping_existing)
                                .then(|| format!("/settings/arr/{row}/path-mappings/{sub}/remove"));
                            (render::repeated_row(&doc, overrides, &mapping_entries, &[row, sub], errors, mapping_remove.as_deref()))
                        }
                    }
                }
            }
            div.actions { button type="submit" { "Save" } }
        }
        p { a href="/settings" { "Back to settings" } }
    };

    Ok(layout::page(&runtime.chrome, "Arr instances", body).into_response())
}

async fn arr_submit(
    State(state): State<AppState>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;

    let mut templates = arr_row_fields();
    templates.extend(mapping_row_fields());
    let overrides: Overrides = pairs.iter().cloned().collect();

    let existing = doc.row_count("arr");
    let arr_skip = gate_new_rows("arr", existing, &templates, &pairs);

    let mut arr_rows_with_mappings: BTreeSet<usize> = BTreeSet::new();
    for (key, _) in &pairs {
        if let Some(rest) = key.strip_prefix("arr.")
            && let Some((index_text, tail)) = rest.split_once('.')
            && tail.starts_with("path_mappings.")
            && let Ok(index) = index_text.parse::<usize>()
        {
            arr_rows_with_mappings.insert(index);
        }
    }
    let mapping_templates = mapping_row_fields();
    let mut mapping_skip: HashMap<String, HashSet<usize>> = HashMap::new();
    for row in arr_rows_with_mappings {
        let section = format!("arr.{row}.path_mappings");
        let mapping_existing = doc.row_count(&section);
        let skip = gate_new_rows(&section, mapping_existing, &mapping_templates, &pairs);
        mapping_skip.insert(section, skip);
    }

    let skip = |key: &str| {
        if in_skipped_row(key, "arr", &arr_skip) {
            return true;
        }
        mapping_skip
            .iter()
            .any(|(section, skip)| in_skipped_row(key, section, skip))
    };

    match apply_pairs(&mut doc, &templates, pairs, &skip) {
        Err(message) => Err(WebError::Refused(message)),
        Ok(errors) if !errors.is_empty() => render_arr_page(&state, &overrides, &errors, &[]).await,
        Ok(_) => match validate(&doc) {
            Err(invalid) => {
                render_arr_page(&state, &overrides, &invalid.errors, &invalid.general).await
            }
            Ok(_) => save_and_reload(&state, doc, "/settings/arr").await,
        },
    }
}

async fn arr_remove_confirm(
    State(state): State<AppState>,
    AxumPath(index): AxumPath<usize>,
) -> Result<Response, WebError> {
    let runtime = state.runtime.current();
    let doc = open_document(&state)?;
    let config = doc.to_config().unwrap_or_default();
    let Some(arr) = config.arr.get(index) else {
        return Err(WebError::NotFound);
    };

    let body = html! {
        h2 { "Remove arr instance " (arr.name) "?" }
        p { "It will no longer be asked for candidates." }
        form method="post" {
            button.danger type="submit" { "Remove " (arr.name) }
        }
        p { a href="/settings/arr" { "Cancel" } }
    };
    Ok(layout::page(&runtime.chrome, "Remove arr instance", body).into_response())
}

async fn arr_remove(
    State(state): State<AppState>,
    AxumPath(index): AxumPath<usize>,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    doc.remove_row("arr", index);

    match validate(&doc) {
        Err(invalid) => Err(WebError::Refused(invalid_message(&invalid))),
        Ok(_) => save_and_reload(&state, doc, "/settings/arr").await,
    }
}

async fn arr_path_mapping_remove(
    State(state): State<AppState>,
    AxumPath((index, sub)): AxumPath<(usize, usize)>,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    doc.remove_row(&format!("arr.{index}.path_mappings"), sub);

    match validate(&doc) {
        Err(invalid) => Err(WebError::Refused(invalid_message(&invalid))),
        Ok(_) => save_and_reload(&state, doc, "/settings/arr").await,
    }
}

// --- the fakes-only demo setup (step 13) ---

#[cfg(feature = "fakes")]
async fn load_demo(State(state): State<AppState>) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    let config = doc.to_config().unwrap_or_default();
    if !config.trackers.is_empty() {
        return Err(WebError::Refused(
            "Trackers are already configured; the demo setup is only offered on a fresh \
             install."
                .to_owned(),
        ));
    }

    let config_dir = state
        .runtime
        .config_path()
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging_root = std::path::absolute(config_dir.join("data/staging"))
        .unwrap_or_else(|_| config_dir.join("data/staging"));

    doc.set("staging.root", staging_root.display().to_string());
    doc.set("trackers.0.id", "demo".to_owned());
    doc.set("trackers.0.kind", "fake".to_owned());
    doc.set("trackers.0.base_url", "http://localhost".to_owned());
    doc.set("download_client.kind", "fake".to_owned());

    match validate(&doc) {
        Err(invalid) => Err(WebError::Refused(invalid_message(&invalid))),
        Ok(_) => save_and_reload(&state, doc, "/settings").await,
    }
}

#[cfg(not(feature = "fakes"))]
async fn load_demo(State(_state): State<AppState>) -> Result<Response, WebError> {
    Err(WebError::Refused(
        "This build does not include the `fakes` feature.".to_owned(),
    ))
}
