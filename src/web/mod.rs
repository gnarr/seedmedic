//! The operator interface: a driving adapter over the repair capability.
//!
//! Server-rendered, no JavaScript, no API surface beyond what the pages need.
//! It reads repair state and performs the review actions; it contains no
//! rules of its own, because a decision the UI could make differently from the
//! worker is a decision in the wrong place.

mod error;
mod health;
mod jobs;
mod layout;
#[cfg(feature = "metrics")]
mod metrics;
mod review;
mod status;

pub use layout::Chrome;

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};

use crate::repair::RepairDeps;

#[derive(Clone)]
pub struct AppState {
    pub deps: Arc<RepairDeps>,
    /// If set, every request but `/health` must present it as
    /// `Authorization: Bearer <token>`.
    auth_token: Option<Arc<str>>,
    /// See [`health::health`].
    health_threshold: Duration,
    /// The effective configuration, secrets redacted, for [`status::page`].
    config_summary: Arc<str>,
    /// Whether `/metrics` serves anything. Only read when built with the
    /// `metrics` feature; harmless otherwise.
    #[cfg_attr(not(feature = "metrics"), allow(dead_code))]
    metrics_enabled: bool,
    /// The setup banner every page shows until nothing is left to configure.
    chrome: Chrome,
}

pub fn router(
    deps: Arc<RepairDeps>,
    auth_token: Option<String>,
    health_threshold: Duration,
    config_summary: String,
    metrics_enabled: bool,
    chrome: Chrome,
) -> Router {
    let state = AppState {
        deps,
        auth_token: auth_token.map(Arc::from),
        health_threshold,
        config_summary: Arc::from(config_summary),
        metrics_enabled,
        chrome,
    };

    let router = Router::new()
        .route("/", get(jobs::list))
        .route("/status", get(status::page))
        .route("/jobs/{id}", get(jobs::detail));

    #[cfg(feature = "metrics")]
    let router = router.route("/metrics", get(metrics::handler));

    router
        .route("/jobs/{id}/retry", post(review::retry))
        .route("/jobs/{id}/restart", post(review::restart))
        .route("/jobs/{id}/abandon", post(review::abandon))
        .route(
            "/jobs/{id}/abandon-and-discard",
            post(review::abandon_and_discard),
        )
        .route("/jobs/{id}/approve-resume", post(review::approve_resume))
        .route(
            "/jobs/{id}/choose-candidate",
            post(review::choose_candidate),
        )
        .route("/jobs/bulk/retry", post(review::bulk_retry))
        .route("/jobs/bulk/abandon", post(review::bulk_abandon))
        .route("/health", get(health::health))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_auth_token,
        ))
        .with_state(state)
}

/// No-op when `server.auth_token` is unset — the documented default posture
/// is "do not expose this to the internet," not "this is secure." `/health`
/// is exempt so a container orchestrator does not need the token.
async fn require_auth_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }

    match &state.auth_token {
        None => next.run(request).await,
        Some(expected) => {
            let provided = request
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "));

            if provided == Some(expected.as_ref()) {
                next.run(request).await
            } else {
                (StatusCode::UNAUTHORIZED, "unauthorized\n").into_response()
            }
        }
    }
}
