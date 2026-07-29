//! `/metrics`: present only when built with the `metrics` feature, and only
//! serving anything when `metrics.enabled` is also true — see
//! `docs/todos/0012-observability.md`.
//!
//! Compiled only with the feature, so a default build proves the compile-time
//! half of the gate: nothing here even exists otherwise.

#![cfg(feature = "metrics")]

mod support;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

#[tokio::test]
async fn returns_not_found_when_metrics_enabled_is_false() {
    let harness = support::Harness::new().await;
    let runtime = support::runtime_with_deps(
        harness.deps.clone(),
        None,
        support::HEALTH_THRESHOLD,
        String::new(),
        false,
    );
    let router = support::router_with(runtime);

    let response = router
        .oneshot(
            Request::get("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reports_a_transition_once_enabled() {
    let harness = support::Harness::new().await;
    harness.discover().await;
    harness.tick().await;
    let runtime = support::runtime_with_deps(
        harness.deps.clone(),
        None,
        support::HEALTH_THRESHOLD,
        String::new(),
        true,
    );
    let router = support::router_with(runtime);

    let response = router
        .oneshot(
            Request::get("/metrics")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("valid json");
    assert!(
        json["transitions"]
            .as_array()
            .is_some_and(|t| !t.is_empty())
    );
    assert!(json["repairs_by_state"].is_object());
}
