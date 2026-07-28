//! The operator interface: a driving adapter over the repair capability.
//!
//! Server-rendered, no JavaScript, no API surface beyond what the pages need.
//! It reads repair state and performs the review actions; it contains no
//! rules of its own, because a decision the UI could make differently from the
//! worker is a decision in the wrong place.

mod error;
mod jobs;
mod layout;
mod review;

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use crate::repair::RepairDeps;

#[derive(Clone)]
pub struct AppState {
    pub deps: Arc<RepairDeps>,
}

pub fn router(deps: Arc<RepairDeps>) -> Router {
    Router::new()
        .route("/", get(jobs::list))
        .route("/jobs/{id}", get(jobs::detail))
        .route("/jobs/{id}/retry", post(review::retry))
        .route("/jobs/{id}/restart", post(review::restart))
        .route("/jobs/{id}/abandon", post(review::abandon))
        .route("/jobs/{id}/approve-resume", post(review::approve_resume))
        .route("/health", get(health))
        .with_state(AppState { deps })
}

async fn health() -> &'static str {
    "ok"
}
