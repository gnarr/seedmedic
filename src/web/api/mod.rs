//! The JSON API the operator UI is built on.
//!
//! Versioned at `/api/v1` for two structural reasons, not fashion:
//!
//! 1. **The SPA history fallback needs a prefix that never falls back.** Every
//!    unmatched path serves `index.html`, so without a reserved prefix a typo'd
//!    endpoint returns HTML with `200` and the client throws
//!    `SyntaxError: Unexpected token '<'` — the most confusing failure mode a
//!    single-page app has. One rule instead: anything under `/api/` is JSON and
//!    never falls back.
//! 2. **`Authorization: Bearer` is already a documented script surface** (see
//!    `docs/todos/0011-configuration-and-secrets.md`). Once anything is scripted
//!    against `/api/jobs`, the only way to reshape a response without a version
//!    segment is to invent a new noun.
//!
//! `/health` and `/metrics` stay unversioned at the root: the first is in the
//! Dockerfile's `HEALTHCHECK` and the CI smoke test, the second is a scrape
//! target. Neither belongs to the SPA.
//!
//! **This module lives under `src/web/` deliberately.** The
//! `nothing_under_src_web_calls_expose` test walks this directory looking for
//! any call to `Secret`'s plaintext accessor, so an API at `src/api/` would
//! silently escape the one guard the repository calls "the one thing here that
//! must never regress silently". That test is a plain substring search, which is
//! also why the accessor cannot be named literally anywhere under here — not
//! even in a comment.

pub mod actions;
pub mod dashboard;
pub mod error;
pub mod events;
pub mod jobs;
pub mod session;
pub mod view;

use axum::{
    Json, Router,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};

use super::AppState;

/// Routes that require a credential when one is configured.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/dashboard", get(dashboard::dashboard))
        .route("/diagnostics", get(dashboard::diagnostics))
        .route("/jobs", get(jobs::list))
        .route("/jobs/{id}", get(jobs::detail))
        // Kebab-case paths, matching the maud routes they replace, so the diff
        // is a prefix and a response type rather than a rename.
        .route("/jobs/{id}/retry", post(actions::retry))
        .route("/jobs/{id}/restart", post(actions::restart))
        .route("/jobs/{id}/abandon", post(actions::abandon))
        .route(
            "/jobs/{id}/abandon-and-discard",
            post(actions::abandon_and_discard),
        )
        .route("/jobs/{id}/approve-resume", post(actions::approve_resume))
        .route(
            "/jobs/{id}/choose-candidate",
            post(actions::choose_candidate),
        )
        .route("/jobs/{id}/discard-staging", post(actions::discard_staging))
        .route("/jobs/bulk/retry", post(actions::bulk_retry))
        .route("/jobs/bulk/abandon", post(actions::bulk_abandon))
        // Inside the guarded router: an event stream carries job names, staging
        // paths and tracker error text, so it needs a credential exactly like
        // every other read.
        .route("/events", get(events::stream))
        // `.nest()` gives a nested router axum's default 404 — status only, with
        // an **empty body**. A client that always parses JSON chokes on that, so
        // both fallbacks are set explicitly.
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
}

/// Routes reachable without a credential.
///
/// Merged in unguarded rather than named in a `matches!` inside the middleware,
/// so "did we remember to exempt it" stops being a question anyone can get
/// wrong: exemption is where the route is registered.
pub fn open_router() -> Router<AppState> {
    Router::new().route(
        "/session",
        get(session::show)
            .post(session::create)
            .merge(delete(session::destroy)),
    )
}

async fn not_found(uri: axum::http::Uri) -> Response {
    problem(
        StatusCode::NOT_FOUND,
        "not_found",
        format!("{} is not an endpoint SeedMedic has.", uri.path()),
    )
}

async fn method_not_allowed() -> Response {
    problem(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "That method is not allowed on this endpoint.".to_owned(),
    )
}

/// The same body shape as [`error::ApiError`], for the two fallbacks that cannot
/// go through it because they have no `ApiError` to convert.
fn problem(status: StatusCode, code: &'static str, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": { "code": code, "message": message, "fields": {}, "general": [] }
        })),
    )
        .into_response()
}
