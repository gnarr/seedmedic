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
mod settings;
mod status;

pub use layout::Chrome;

use std::{net::SocketAddr, sync::Arc};

use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};

use crate::runtime::RuntimeHandle;

/// One generation lives on `runtime` and is fetched fresh — `runtime.current()`
/// — at the top of every handler, so a request always sees a consistent
/// snapshot even if a reload lands mid-request.
#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<RuntimeHandle>,
    /// What the process is actually listening on, fixed for its lifetime.
    /// Unlike everything on `Runtime`, a reload can never replace this — so
    /// `server.bind_address` is reported as needing a restart instead of
    /// being silently ignored.
    pub bind_address: SocketAddr,
}

pub fn router(runtime: Arc<RuntimeHandle>, bind_address: SocketAddr) -> Router {
    let state = AppState {
        runtime,
        bind_address,
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
        .merge(settings::router())
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

    match &state.runtime.current().auth_token {
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
