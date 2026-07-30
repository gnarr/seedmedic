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
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use maud::html;

use crate::{
    config::{Config, ConfigDocument, Secret},
    connectivity::{self, ProbeResult},
};

use super::{AppState, error::WebError, layout, login};
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
        .route("/settings/trackers/{index}/test", post(trackers_test))
        .route("/settings/arr", get(arr_page).post(arr_submit))
        .route(
            "/settings/arr/{index}/remove",
            get(arr_remove_confirm).post(arr_remove),
        )
        .route("/settings/arr/{index}/test", post(arr_test))
        .route(
            "/settings/arr/{index}/path-mappings/{sub}/remove",
            post(arr_path_mapping_remove),
        )
        .route("/settings/download-client/test", post(download_client_test))
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
        (writability_notice(&doc))
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

/// Said before the operator types anything, on every page, not just as a
/// 500 after they press save — see docs/todos/0017-the-settings-pages.md's
/// "read-only degradation" invariant.
fn writability_notice(doc: &ConfigDocument) -> maud::Markup {
    html! {
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
    }
}

#[cfg(feature = "fakes")]
fn can_load_demo(config: &Config) -> bool {
    config.trackers.is_empty()
}

#[cfg(not(feature = "fakes"))]
fn can_load_demo(_config: &Config) -> bool {
    false
}

/// Whether `key`'s value in the *raw submission* was empty. Checked against
/// `overrides` (the submitted pairs), never against the resulting draft
/// `Config`: once `apply_pairs` runs, a blank secret field means "leave the
/// stored value unchanged" (right for Save), so the draft's secret can be
/// non-empty even though the operator typed nothing. A probe must refuse on
/// the raw submission instead, or a blank field plus a form-supplied host
/// becomes a way to exfiltrate a stored secret — see
/// `docs/todos/0019-connection-tests.md`'s empty-secret invariant.
fn submitted_secret_is_empty(overrides: &Overrides, key: &str) -> bool {
    overrides.get(key).is_none_or(|value| value.is_empty())
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
    render_simple_page(
        &state,
        &slug,
        &Overrides::new(),
        &FieldErrors::new(),
        &[],
        None,
    )
    .await
}

/// The only page a "Test connection" button appears on: the others have
/// nothing this feature can probe (`docs/todos/0019-connection-tests.md`
/// covers trackers, *arr instances, and the download client only).
const DOWNLOAD_CLIENT_SLUG: &str = "download-client";

async fn render_simple_page(
    state: &AppState,
    slug: &str,
    overrides: &Overrides,
    errors: &FieldErrors,
    general: &[String],
    probe: Option<&ProbeResult>,
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
    let test_href = (slug == DOWNLOAD_CLIENT_SLUG).then_some("/settings/download-client/test");

    let body = html! {
        h2 { (page.title) }
        (writability_notice(&doc))
        @for message in general {
            div.notice.danger { p { (message) } }
        }
        (render::simple_form(&doc, overrides, &entries, errors, test_href, probe))
        p { a href="/settings" { "Back to settings" } }
    };

    Ok(layout::page(&runtime.chrome, page.title, body).into_response())
}

async fn simple_submit(
    State(state): State<AppState>,
    AxumPath(slug): AxumPath<String>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let Some(page) = PAGES.iter().find(|p| p.slug == slug) else {
        return Err(WebError::NotFound);
    };
    let templates: Vec<&'static Field> = fields_for(page.prefixes).collect();
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    let before = doc.to_config().unwrap_or_default();
    let overrides: Overrides = pairs.iter().cloned().collect();

    match apply_pairs(&mut doc, &before, &templates, pairs, &|_| false) {
        Err(message) => Err(WebError::Refused(message)),
        Ok(errors) if !errors.is_empty() => {
            render_simple_page(&state, &slug, &overrides, &errors, &[], None).await
        }
        Ok(_) => match validate(&doc) {
            Err(invalid) => {
                render_simple_page(
                    &state,
                    &slug,
                    &overrides,
                    &invalid.errors,
                    &invalid.general,
                    None,
                )
                .await
            }
            Ok(_) => save_and_reload(&state, doc, &headers, &format!("/settings/{slug}")).await,
        },
    }
}

/// Posted by the download client page's "Test connection" button —
/// `formaction`, so it submits the same draft the "Save" button would, but
/// never writes: it builds one throwaway adapter from the submitted values
/// and probes it (`crate::connectivity::test_download_client`).
async fn download_client_test(
    State(state): State<AppState>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    let before = doc.to_config().unwrap_or_default();
    let templates: Vec<&'static Field> = fields_for(&["download_client."]).collect();
    let overrides: Overrides = pairs.iter().cloned().collect();

    match apply_pairs(&mut doc, &before, &templates, pairs, &|_| false) {
        Err(message) => Err(WebError::Refused(message)),
        Ok(errors) if !errors.is_empty() => {
            render_simple_page(&state, DOWNLOAD_CLIENT_SLUG, &overrides, &errors, &[], None).await
        }
        Ok(_) => match validate(&doc) {
            Err(invalid) => {
                render_simple_page(
                    &state,
                    DOWNLOAD_CLIENT_SLUG,
                    &overrides,
                    &invalid.errors,
                    &invalid.general,
                    None,
                )
                .await
            }
            Ok(config) => {
                let Some(download_client) = &config.download_client else {
                    return render_simple_page(
                        &state,
                        DOWNLOAD_CLIENT_SLUG,
                        &overrides,
                        &FieldErrors::new(),
                        &["Configure a download client before testing it.".to_owned()],
                        None,
                    )
                    .await;
                };
                if submitted_secret_is_empty(&overrides, "download_client.password") {
                    return render_simple_page(
                        &state,
                        DOWNLOAD_CLIENT_SLUG,
                        &overrides,
                        &FieldErrors::new(),
                        &["Enter the password to test this connection.".to_owned()],
                        None,
                    )
                    .await;
                }
                let result = connectivity::test_download_client(download_client).await;
                render_simple_page(
                    &state,
                    DOWNLOAD_CLIENT_SLUG,
                    &overrides,
                    &FieldErrors::new(),
                    &[],
                    Some(&result),
                )
                .await
            }
        },
    }
}

/// `headers` is only ever consulted to decide `Secure` on the cookie minted
/// below, and only when this save just changed `server.auth_token` — every
/// other save leaves `headers` untouched.
async fn save_and_reload(
    state: &AppState,
    doc: ConfigDocument,
    headers: &HeaderMap,
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
    let mut response = layout::page(&state.runtime.current().chrome, "Saved", body).into_response();

    // A token change just invalidated every session, including whichever one
    // (if any) the operator making this save was using — see
    // docs/todos/0018-browser-usable-authentication.md step 5. Mint them a
    // fresh one in the same response so saving a token cannot lock them out
    // of the page they saved it from. Not needed when the save cleared the
    // token: nothing protects the UI anymore, so there is nothing to sign
    // into.
    if applied.auth_token_changed && state.runtime.current().auth_token.is_some() {
        let session_id = state.runtime.create_session();
        response.headers_mut().insert(
            header::SET_COOKIE,
            login::cookie_header(&session_id, headers),
        );
    }

    Ok(response)
}

// --- trackers (repeated) ---

fn tracker_row_fields() -> Vec<&'static Field> {
    fields_for(&["trackers.*"]).collect()
}

async fn trackers_page(State(state): State<AppState>) -> Result<Response, WebError> {
    render_trackers_page(&state, &Overrides::new(), &FieldErrors::new(), &[], None).await
}

async fn render_trackers_page(
    state: &AppState,
    overrides: &Overrides,
    errors: &FieldErrors,
    general: &[String],
    probe: Option<(usize, &ProbeResult)>,
) -> Result<Response, WebError> {
    let runtime = state.runtime.current();
    let doc = open_document(state)?;
    let config = doc.to_config().unwrap_or_default();
    let default_secret = Secret::default();
    let row_fields = tracker_row_fields();
    let existing = doc.row_count("trackers");

    let body = html! {
        h2 { "Trackers" }
        (writability_notice(&doc))
        @for message in general {
            div.notice.danger { p { (message) } }
        }
        form method="post" {
            @for row in 0..=existing {
                @let secret_ref = config.trackers.get(row).map(|t| &t.api_key).unwrap_or(&default_secret);
                @let entries = row_entries(&row_fields, secret_ref);
                @let remove_href = (row < existing).then(|| format!("/settings/trackers/{row}/remove"));
                @let test_href = (row < existing).then(|| format!("/settings/trackers/{row}/test"));
                @let row_probe = probe.and_then(|(probed_row, result)| (probed_row == row).then_some(result));
                @let actions = render::RowActions { remove_href: remove_href.as_deref(), test_href: test_href.as_deref(), probe: row_probe };
                (render::repeated_row(&doc, overrides, &entries, &[row], errors, actions))
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
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    let before = doc.to_config().unwrap_or_default();
    let templates = tracker_row_fields();
    let existing = doc.row_count("trackers");
    let overrides: Overrides = pairs.iter().cloned().collect();
    let skip = gate_new_rows("trackers", existing, &templates, &pairs);

    match apply_pairs(&mut doc, &before, &templates, pairs, &|key| {
        in_skipped_row(key, "trackers", &skip)
    }) {
        Err(message) => Err(WebError::Refused(message)),
        Ok(errors) if !errors.is_empty() => {
            render_trackers_page(&state, &overrides, &errors, &[], None).await
        }
        Ok(_) => match validate(&doc) {
            Err(invalid) => {
                render_trackers_page(&state, &overrides, &invalid.errors, &invalid.general, None)
                    .await
            }
            Ok(_) => save_and_reload(&state, doc, &headers, "/settings/trackers").await,
        },
    }
}

/// Posted by a tracker row's "Test connection" button. Shares `apply_pairs`
/// and `validate` with [`trackers_submit`] — the same draft-building
/// pipeline — but never reaches `save_and_reload`: it probes the row at
/// `index` from the submitted draft and re-renders the page with a result
/// panel instead.
async fn trackers_test(
    State(state): State<AppState>,
    AxumPath(index): AxumPath<usize>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    let before = doc.to_config().unwrap_or_default();
    let templates = tracker_row_fields();
    let existing = doc.row_count("trackers");
    let overrides: Overrides = pairs.iter().cloned().collect();
    let skip = gate_new_rows("trackers", existing, &templates, &pairs);

    match apply_pairs(&mut doc, &before, &templates, pairs, &|key| {
        in_skipped_row(key, "trackers", &skip)
    }) {
        Err(message) => Err(WebError::Refused(message)),
        Ok(errors) if !errors.is_empty() => {
            render_trackers_page(&state, &overrides, &errors, &[], None).await
        }
        Ok(_) => match validate(&doc) {
            Err(invalid) => {
                render_trackers_page(&state, &overrides, &invalid.errors, &invalid.general, None)
                    .await
            }
            Ok(config) => {
                let Some(tracker) = config.trackers.get(index) else {
                    return Err(WebError::NotFound);
                };
                if submitted_secret_is_empty(&overrides, &format!("trackers.{index}.api_key")) {
                    return render_trackers_page(
                        &state,
                        &overrides,
                        &FieldErrors::new(),
                        &["Enter the API key to test this connection.".to_owned()],
                        None,
                    )
                    .await;
                }
                let result = connectivity::test_tracker(tracker).await;
                render_trackers_page(
                    &state,
                    &overrides,
                    &FieldErrors::new(),
                    &[],
                    Some((index, &result)),
                )
                .await
            }
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
    headers: HeaderMap,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    doc.remove_row("trackers", index);

    match validate(&doc) {
        Err(invalid) => Err(WebError::Refused(invalid_message(&invalid))),
        Ok(_) => save_and_reload(&state, doc, &headers, "/settings/trackers").await,
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
    render_arr_page(&state, &Overrides::new(), &FieldErrors::new(), &[], None).await
}

async fn render_arr_page(
    state: &AppState,
    overrides: &Overrides,
    errors: &FieldErrors,
    general: &[String],
    probe: Option<(usize, &ProbeResult)>,
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
        (writability_notice(&doc))
        @for message in general {
            div.notice.danger { p { (message) } }
        }
        form method="post" {
            @for row in 0..=existing {
                @let secret_ref = config.arr.get(row).map(|a| &a.api_key).unwrap_or(&default_secret);
                @let entries = row_entries(&row_fields, secret_ref);
                @let remove_href = (row < existing).then(|| format!("/settings/arr/{row}/remove"));
                @let test_href = (row < existing).then(|| format!("/settings/arr/{row}/test"));
                @let row_probe = probe.and_then(|(probed_row, result)| (probed_row == row).then_some(result));
                @let actions = render::RowActions { remove_href: remove_href.as_deref(), test_href: test_href.as_deref(), probe: row_probe };
                (render::repeated_row(&doc, overrides, &entries, &[row], errors, actions))

                @if row < existing {
                    @let mapping_existing = doc.row_count(&format!("arr.{row}.path_mappings"));
                    div.sub {
                        h4 { "Path mappings" }
                        @for sub in 0..=mapping_existing {
                            @let mapping_entries = row_entries(&mapping_fields, &default_secret);
                            @let mapping_remove = (sub < mapping_existing)
                                .then(|| format!("/settings/arr/{row}/path-mappings/{sub}/remove"));
                            @let mapping_actions = render::RowActions { remove_href: mapping_remove.as_deref(), ..Default::default() };
                            (render::repeated_row(&doc, overrides, &mapping_entries, &[row, sub], errors, mapping_actions))
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

/// Everything but the row bookkeeping: parse the submitted pairs (including
/// the nested path-mapping gating) into `doc`. Shared by [`arr_submit`] and
/// [`arr_test`] so both build their draft the exact same way — the reason a
/// test and a save behave identically up to the point where a test probes
/// instead of saving.
fn arr_apply(
    doc: &mut ConfigDocument,
    before: &Config,
    pairs: Vec<(String, String)>,
) -> Result<FieldErrors, String> {
    let mut templates = arr_row_fields();
    templates.extend(mapping_row_fields());

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

    apply_pairs(doc, before, &templates, pairs, &skip)
}

async fn arr_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    let before = doc.to_config().unwrap_or_default();
    let overrides: Overrides = pairs.iter().cloned().collect();

    match arr_apply(&mut doc, &before, pairs) {
        Err(message) => Err(WebError::Refused(message)),
        Ok(errors) if !errors.is_empty() => {
            render_arr_page(&state, &overrides, &errors, &[], None).await
        }
        Ok(_) => match validate(&doc) {
            Err(invalid) => {
                render_arr_page(&state, &overrides, &invalid.errors, &invalid.general, None).await
            }
            Ok(_) => save_and_reload(&state, doc, &headers, "/settings/arr").await,
        },
    }
}

/// Posted by an *arr row's "Test connection" button. Shares [`arr_apply`]
/// with [`arr_submit`] — see its doc comment.
async fn arr_test(
    State(state): State<AppState>,
    AxumPath(index): AxumPath<usize>,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    let before = doc.to_config().unwrap_or_default();
    let overrides: Overrides = pairs.iter().cloned().collect();

    match arr_apply(&mut doc, &before, pairs) {
        Err(message) => Err(WebError::Refused(message)),
        Ok(errors) if !errors.is_empty() => {
            render_arr_page(&state, &overrides, &errors, &[], None).await
        }
        Ok(_) => match validate(&doc) {
            Err(invalid) => {
                render_arr_page(&state, &overrides, &invalid.errors, &invalid.general, None).await
            }
            Ok(config) => {
                let Some(arr) = config.arr.get(index) else {
                    return Err(WebError::NotFound);
                };
                if submitted_secret_is_empty(&overrides, &format!("arr.{index}.api_key")) {
                    return render_arr_page(
                        &state,
                        &overrides,
                        &FieldErrors::new(),
                        &["Enter the API key to test this connection.".to_owned()],
                        None,
                    )
                    .await;
                }
                let result = connectivity::test_arr(arr).await;
                render_arr_page(
                    &state,
                    &overrides,
                    &FieldErrors::new(),
                    &[],
                    Some((index, &result)),
                )
                .await
            }
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
    headers: HeaderMap,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    doc.remove_row("arr", index);

    match validate(&doc) {
        Err(invalid) => Err(WebError::Refused(invalid_message(&invalid))),
        Ok(_) => save_and_reload(&state, doc, &headers, "/settings/arr").await,
    }
}

async fn arr_path_mapping_remove(
    State(state): State<AppState>,
    AxumPath((index, sub)): AxumPath<(usize, usize)>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    let mut doc = open_document(&state)?;
    require_writable(&doc)?;
    doc.remove_row(&format!("arr.{index}.path_mappings"), sub);

    match validate(&doc) {
        Err(invalid) => Err(WebError::Refused(invalid_message(&invalid))),
        Ok(_) => save_and_reload(&state, doc, &headers, "/settings/arr").await,
    }
}

// --- the fakes-only demo setup (step 13) ---

#[cfg(feature = "fakes")]
async fn load_demo(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
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
        Ok(_) => save_and_reload(&state, doc, &headers, "/settings").await,
    }
}

#[cfg(not(feature = "fakes"))]
async fn load_demo(State(_state): State<AppState>) -> Result<Response, WebError> {
    Err(WebError::Refused(
        "This build does not include the `fakes` feature.".to_owned(),
    ))
}
