//! A readiness probe, not a liveness one that happens to always say yes.
//!
//! `200` means the database is reachable and the worker has ticked recently;
//! `503` otherwise. Deliberately blind to trackers and the download client —
//! those being unreachable is a normal, recoverable condition, and a health
//! check that fails on it would get the container restarted for no reason.

use axum::{extract::State, http::StatusCode, response::IntoResponse};

use super::AppState;

pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let runtime = state.runtime.current();
    let database_ok = runtime.deps.store.ping().await.is_ok();

    let now = runtime.deps.clock.now();
    let worker_recent = runtime.deps.worker_health.last_tick().is_some_and(|last| {
        let threshold = chrono::Duration::from_std(runtime.health_threshold)
            .unwrap_or_else(|_| chrono::Duration::seconds(60));
        now - last <= threshold
    });

    if database_ok && worker_recent {
        (StatusCode::OK, "ok\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready\n")
    }
}
