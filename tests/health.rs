//! `/health` distinguishes "process alive" from "able to work" — see
//! `docs/todos/0012-observability.md`.

mod support;

use std::time::Duration;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use seedmedic::web;
use tower::ServiceExt;

async fn get_health(harness: &support::Harness, threshold: Duration) -> StatusCode {
    let router = web::router(
        harness.deps.clone(),
        None,
        threshold,
        String::new(),
        false,
        web::Chrome::none(),
    );
    router
        .oneshot(
            Request::get("/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response")
        .status()
}

#[tokio::test]
async fn health_is_unready_before_the_worker_has_ever_ticked() {
    let harness = support::Harness::new().await;

    assert_eq!(
        get_health(&harness, support::HEALTH_THRESHOLD).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn health_is_ok_once_the_worker_has_ticked_within_the_threshold() {
    let harness = support::Harness::new().await;
    harness.tick().await;

    assert_eq!(
        get_health(&harness, support::HEALTH_THRESHOLD).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn health_is_unready_once_the_last_tick_is_older_than_the_threshold() {
    let harness = support::Harness::new().await;
    harness.tick().await;
    harness.clock.advance(chrono::Duration::seconds(120));

    assert_eq!(
        get_health(&harness, Duration::from_secs(60)).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn health_is_ok_even_with_every_tracker_unreachable() {
    let harness = support::Harness::new().await;
    harness
        .tracker
        .fail_next_call_with(seedmedic::tracker::TrackerError::Transport("down".into()));
    harness.tick().await;
    harness.worker().discover().await;

    assert_eq!(
        get_health(&harness, support::HEALTH_THRESHOLD).await,
        StatusCode::OK
    );
}
