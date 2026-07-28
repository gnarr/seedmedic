//! The web UI's optional bearer-token protection — see
//! `docs/todos/0011-configuration-and-secrets.md`.

mod support;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use seedmedic::web;
use tower::ServiceExt;

#[tokio::test]
async fn unset_auth_token_allows_every_request() {
    let harness = support::Harness::new().await;
    let router = web::router(harness.deps.clone(), None);

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_request_without_the_token_is_rejected() {
    let harness = support::Harness::new().await;
    let router = web::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_request_with_the_wrong_token_is_rejected() {
    let harness = support::Harness::new().await;
    let router = web::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(
            Request::get("/")
                .header(header::AUTHORIZATION, "Bearer wrong")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_request_with_the_right_token_is_allowed() {
    let harness = support::Harness::new().await;
    let router = web::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(
            Request::get("/")
                .header(header::AUTHORIZATION, "Bearer s3cret")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn health_is_reachable_without_the_token() {
    let harness = support::Harness::new().await;
    let router = web::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(
            Request::get("/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
}
