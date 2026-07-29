//! The web UI's optional auth-token protection: a bearer header for scripts,
//! a session cookie for browsers — see
//! `docs/todos/0011-configuration-and-secrets.md` and
//! `docs/todos/0018-browser-usable-authentication.md`.

mod support;

use axum::{
    Router,
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

/// Log in against `router` and return just the `name=value` pair a browser
/// would send back on the next request — not the whole `Set-Cookie` line,
/// which also carries attributes a `Cookie` request header never repeats.
async fn login_cookie(router: &Router, token: &str) -> String {
    let response = router
        .clone()
        .oneshot(form_request("POST", "/login", &format!("token={token}")))
        .await
        .expect("response");
    let set_cookie = response
        .headers()
        .get(header::SET_COOKIE)
        .expect("login sets a cookie")
        .to_str()
        .expect("ascii header")
        .to_owned();
    set_cookie
        .split(';')
        .next()
        .expect("cookie header always has a name=value pair")
        .to_owned()
}

#[tokio::test]
async fn a_page_with_no_token_configured_recommends_setting_one() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), None);

    let response = router
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("response");
    let body = body_text(response).await;

    assert!(body.contains("No auth token is set"));
    assert!(!body.contains("Sign out"));
}

#[tokio::test]
async fn an_authenticated_page_shows_a_sign_out_link() {
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
    let body = body_text(response).await;

    assert!(body.contains("Sign out"));
    assert!(!body.contains("No auth token is set"));
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

#[tokio::test]
async fn an_html_request_without_credentials_is_sent_to_login() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(
            Request::get("/")
                .header(header::ACCEPT, "text/html")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response
            .headers()
            .get(header::LOCATION)
            .expect("redirect target")
            .to_str()
            .expect("ascii header"),
        "/login"
    );
}

#[tokio::test]
async fn a_bad_bearer_never_redirects_even_when_html_is_accepted() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(
            Request::get("/")
                .header(header::ACCEPT, "text/html")
                .header(header::AUTHORIZATION, "Bearer wrong")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a script must never be silently redirected to a login page"
    );
}

#[tokio::test]
async fn a_session_cookie_authorises_a_request() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));
    let cookie = login_cookie(&router, "s3cret").await;

    let response = router
        .oneshot(
            Request::get("/")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_made_up_session_id_does_not_authorise() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(
            Request::get("/")
                .header(header::COOKIE, "seedmedic_session=not-a-real-session")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn post_logout_clears_the_session() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));
    let cookie = login_cookie(&router, "s3cret").await;

    let logout_response = router
        .clone()
        .oneshot(
            Request::post("/logout")
                .header(header::COOKIE, cookie.clone())
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(logout_response.status(), StatusCode::SEE_OTHER);

    let response = router
        .oneshot(
            Request::get("/")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a logged-out cookie must no longer authorise anything"
    );
}

#[tokio::test]
async fn a_cross_site_post_is_rejected_but_same_origin_is_allowed() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));
    let cookie = login_cookie(&router, "s3cret").await;

    let cross_site = router
        .clone()
        .oneshot(
            Request::post("/logout")
                .header(header::COOKIE, cookie.clone())
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);

    let same_origin = router
        .oneshot(
            Request::post("/logout")
                .header(header::COOKIE, cookie)
                .header("sec-fetch-site", "same-origin")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(same_origin.status(), StatusCode::SEE_OTHER);
}
