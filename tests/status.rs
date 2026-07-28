//! `/status`: what is SeedMedic doing and can it reach everything? — see
//! `docs/todos/0012-observability.md`.

mod support;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use seedmedic::web;
use tower::ServiceExt;

async fn get_status(router: axum::Router) -> (StatusCode, String) {
    let response = router
        .oneshot(
            Request::get("/status")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    (status, String::from_utf8(body.to_vec()).expect("utf8 body"))
}

#[tokio::test]
async fn renders_with_zero_jobs() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), None);

    let (status, body) = get_status(router).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("No hit-and-runs discovered yet."));
}

#[tokio::test]
async fn renders_with_a_job_underway_and_names_the_stub_adapters() {
    let harness = support::Harness::new().await;
    harness.discover().await;
    harness.tick().await;
    let router = support::router(harness.deps.clone(), None);

    let (status, body) = get_status(router).await;

    assert_eq!(status, StatusCode::OK);
    // The test harness only ever wires up fake adapters.
    assert!(body.contains("fake (stub)"));
}

#[tokio::test]
async fn no_secret_appears_in_the_status_page_html() {
    let harness = support::Harness::new().await;
    let toml_text = r#"
        [staging]
        root = "/srv/seedmedic/staging"

        [[trackers]]
        id = "example"
        kind = "unit3d"
        base_url = "http://example.test"
        api_key = "tr4ck3r-secret"

        [download_client]
        password = "qbit-secret"
    "#;
    let config: seedmedic::config::Config = toml::from_str(toml_text).expect("parses");

    let router = web::router(
        harness.deps.clone(),
        None,
        support::HEALTH_THRESHOLD,
        config.redacted_summary(),
    );

    let (status, body) = get_status(router).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!body.contains("tr4ck3r-secret"));
    assert!(!body.contains("qbit-secret"));
    assert!(body.contains("api_key=set"));
    assert!(body.contains("password=set"));
}
