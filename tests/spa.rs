//! The real built operator UI, embedded in the router.

mod support;

use std::sync::Arc;

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
    response::Response,
};
use tower::ServiceExt;

const EXPECT_BUNDLE_ENV: &str = "SEEDMEDIC_EXPECT_BUNDLE";

async fn get(router: axum::Router, path: &str) -> Response {
    router
        .oneshot(Request::get(path).body(Body::empty()).expect("request"))
        .await
        .expect("response")
}

async fn body(response: Response) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    String::from_utf8(bytes.to_vec()).expect("operator UI is UTF-8")
}

async fn built(response: Response) -> Option<Response> {
    if response.status() != StatusCode::SERVICE_UNAVAILABLE {
        return Some(response);
    }

    let missing = body(response).await;
    assert!(
        missing.contains("operator UI was not built"),
        "a bundle-absent build must explain how to build the UI"
    );
    assert!(
        std::env::var_os(EXPECT_BUNDLE_ENV).is_none(),
        "{EXPECT_BUNDLE_ENV} is set, but the compiled binary contains no UI bundle"
    );
    None
}

/// With a Vite build present, proves the embedded bytes are a real shell, its
/// history fallback works, and its content-hashed script receives the cache
/// policy that makes an upgrade safe. The Node-free suite exercises the
/// documented bundle-absent response instead.
#[tokio::test]
async fn a_built_bundle_serves_the_shell_history_fallback_and_assets() {
    let harness = support::Harness::new().await;
    let response = get(support::router(harness.deps.clone(), None), "/repairs/42").await;
    let Some(response) = built(response).await else {
        return;
    };
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");

    let html = body(response).await;
    assert!(html.contains("<div id=\"root\"></div>"));
    assert!(html.contains("<base href=\"/\""));
    assert!(!html.contains("The operator UI was not built"));

    let asset = html
        .split("src=\"./")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("the built shell names its JavaScript asset");
    let response = get(
        support::router(harness.deps.clone(), None),
        &format!("/{asset}"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/javascript; charset=utf-8"
    );

    let response = get(support::router(harness.deps, None), "/assets/missing.js").await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_built_shell_uses_the_configured_reverse_proxy_base_path() {
    let harness = support::Harness::new().await;
    let mut runtime = support::runtime_with_deps(
        harness.deps,
        None,
        support::HEALTH_THRESHOLD,
        String::new(),
        false,
    );
    runtime.base_path = Arc::from("/seedmedic");

    let response = get(support::router_with(runtime), "/").await;
    let Some(response) = built(response).await else {
        return;
    };
    assert_eq!(response.status(), StatusCode::OK);
    assert!(body(response).await.contains("<base href=\"/seedmedic/\""));
}
