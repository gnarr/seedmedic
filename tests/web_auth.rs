//! The web UI's optional auth-token protection: a bearer header for scripts,
//! a session cookie for browsers — see
//! `docs/todos/0011-configuration-and-secrets.md` and
//! `docs/todos/0018-browser-usable-authentication.md`.

mod support;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use tower::ServiceExt;

fn form_request(method: &str, path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .expect("request")
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

#[tokio::test]
async fn unset_auth_token_allows_every_request() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), None);

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_request_without_the_token_is_rejected() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_request_with_the_wrong_token_is_rejected() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

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
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

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
    harness.tick().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

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

#[tokio::test]
async fn get_login_is_reachable_without_credentials() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(Request::get("/login").body(Body::empty()).expect("request"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_right_token_sets_a_cookie_and_redirects() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(form_request("POST", "/login", "token=s3cret"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("a successful login sets a cookie")
        .to_str()
        .expect("ascii header");
    assert!(cookie.contains("seedmedic_session="));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
    assert!(
        !cookie.contains("s3cret"),
        "the cookie must carry a session id, never the token: {cookie}"
    );
}

#[tokio::test]
async fn the_wrong_token_re_renders_with_no_set_cookie() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(form_request("POST", "/login", "token=wrong"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get(header::SET_COOKIE).is_none());
    let body = body_text(response).await;
    assert!(body.contains("Incorrect token"));
    assert!(!body.contains("s3cret"));
}

#[tokio::test]
async fn the_token_never_appears_in_a_location_header() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(form_request("POST", "/login", "token=s3cret"))
        .await
        .expect("response");

    let location = response
        .headers()
        .get(header::LOCATION)
        .expect("a successful login redirects")
        .to_str()
        .expect("ascii header");
    assert!(!location.contains("s3cret"));
}
