//! `/api/v1` — see `docs/todos/0021-a-react-operator-ui.md`.
//!
//! Drives the real `axum::Router` and asserts on JSON, which is strictly
//! stronger than the HTML substring checks it replaces: `body.contains("alpha")`
//! passes when "alpha" appears in an unrelated string, and
//! `body.contains("fake (stub)")` passes when it appears for the wrong adapter.
//! An array of tracker ids does neither.

mod support;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use seedmedic::repair::{
    RepairState, RepairStore, ReviewReason, TransitionReason, TransitionUpdate,
};
use serde_json::Value;
use tower::ServiceExt;

async fn get(router: axum::Router, path: &str) -> (StatusCode, Value) {
    get_with(router, path, None).await
}

async fn get_with(router: axum::Router, path: &str, bearer: Option<&str>) -> (StatusCode, Value) {
    let mut request = Request::get(path).header(header::ACCEPT, "application/json");
    if let Some(token) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = router
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");

    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    // An empty body is a real failure mode here — `.nest()`'s default 404 has
    // one — so surface it as `Value::Null` rather than panicking, and let the
    // assertions say what was wrong.
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[tokio::test]
async fn the_dashboard_reports_zero_jobs_as_zero_rather_than_as_prose() {
    let harness = support::Harness::new().await;
    let (status, body) = get(
        support::router(harness.deps.clone(), None),
        "/api/v1/dashboard",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["counts"]["total"], 0);
    assert_eq!(body["counts"]["by_state"], serde_json::json!([]));
    assert_eq!(body["attention"]["review"], 0);
    assert!(
        body["generated_at"].is_string(),
        "the client shows how stale the page is, so this is not optional"
    );
}

#[tokio::test]
async fn the_dashboard_counts_and_names_the_stub_adapters() {
    let harness = support::Harness::new().await;
    harness.discover().await;
    harness.tick().await;

    let (status, body) = get(
        support::router(harness.deps.clone(), None),
        "/api/v1/dashboard",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body["counts"]["total"].as_i64().expect("a number") > 0);

    let trackers = body["trackers"].as_array().expect("an array");
    assert!(!trackers.is_empty());
    // Exact, not a substring of the whole page: the old assertion would have
    // passed if "fake (stub)" appeared anywhere at all.
    assert_eq!(trackers[0]["adapter"], "fake");
    assert_eq!(trackers[0]["stub"], true);
}

/// Three states on the wire, not `boolean | null`. `Chrome`'s `Option<bool>`
/// distinguishes "no token set" from "this page cannot know", and collapsing them
/// makes a page that cannot know claim the port is unauthenticated.
#[tokio::test]
async fn the_auth_state_is_a_three_way_discriminant() {
    let harness = support::Harness::new().await;

    let (_, body) = get(
        support::router(harness.deps.clone(), None),
        "/api/v1/dashboard",
    )
    .await;
    assert_eq!(body["setup"]["auth"], "unset");

    let (_, body) = get_with(
        support::router(harness.deps.clone(), Some("t0ken".to_owned())),
        "/api/v1/dashboard",
        Some("t0ken"),
    )
    .await;
    assert_eq!(body["setup"]["auth"], "set");
}

#[tokio::test]
async fn a_job_detail_carries_the_evidence_that_was_never_rendered_before() {
    // `Harness::new` already seeds a library whose files match the fixture
    // torrent, so matching reaches `Probable` on the first tick.
    let harness = support::Harness::new().await;
    let job = harness.discover().await;
    harness.tick().await;

    let (status, body) = get(
        support::router(harness.deps.clone(), None),
        &format!("/api/v1/jobs/{}", job.id),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let files = body["files"].as_array().expect("an array");
    assert!(!files.is_empty(), "the plan should have a file by now");

    let matched = files
        .iter()
        .find(|file| !file["source"].is_null())
        .expect("at least one file was matched");
    let evidence = &matched["evidence"];
    assert!(
        evidence["size_matches"].is_boolean(),
        "MatchEvidence is persisted for every file and was displayed nowhere \
         before 0021; it is the 'why do we believe this' a review needs: {evidence}"
    );
    assert!(evidence["candidates_with_matching_size"].is_number());
}

/// The action map is computed on the server precisely so "which action is legal"
/// does not end up in Rust *and* TypeScript.
#[tokio::test]
async fn a_failed_job_is_no_longer_a_dead_end() {
    let harness = support::Harness::new().await;
    let job = harness.discover().await;

    // Park it, then abandon it — the shortest route to `failed`.
    let parked = job
        .plan_transition(
            RepairState::AwaitingReview,
            TransitionReason::Review(ReviewReason::NoCandidates),
        )
        .expect("can park");
    harness
        .store
        .apply(job.id, parked, TransitionUpdate::default())
        .await
        .expect("parked");
    let job = harness.job(job.id).await;
    let failed = job
        .plan_transition(RepairState::Failed, TransitionReason::OperatorAbandon)
        .expect("can abandon");
    harness
        .store
        .apply(job.id, failed, TransitionUpdate::default())
        .await
        .expect("failed");

    let (status, body) = get(
        support::router(harness.deps.clone(), None),
        &format!("/api/v1/jobs/{}", job.id),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["job"]["state"], "failed");
    assert_eq!(
        body["actions"]["restart"]["available"], true,
        "validate_transition has always permitted failed -> discovered; the maud \
         UI simply never rendered the button, so a failed repair was a dead end \
         in the browser"
    );
    assert_eq!(body["actions"]["abandon"]["available"], false);
    assert!(
        body["actions"]["abandon"]["why"].is_string(),
        "an unavailable action says why, in the same words the action itself \
         would refuse with"
    );
}

/// `approve_resume` is double-guarded in `review.rs`; the capability map must be
/// guarded identically or the UI offers the one genuinely dangerous action on a
/// job it does not apply to.
#[tokio::test]
async fn approve_resume_is_offered_only_for_the_auto_resume_reason() {
    let harness = support::Harness::new().await;
    let job = harness.discover().await;

    for (reason, expected) in [
        (ReviewReason::NoCandidates, false),
        (ReviewReason::AutoResumeDisabled, true),
    ] {
        let job_now = harness.job(job.id).await;
        let target = if job_now.state == RepairState::AwaitingReview {
            // Already parked: re-park with the other reason by retrying first.
            let retry = job_now
                .plan_transition(
                    job_now.review_from_state.expect("a resume point"),
                    TransitionReason::OperatorRetry,
                )
                .expect("can retry");
            harness
                .store
                .apply(job.id, retry, TransitionUpdate::default())
                .await
                .expect("retried");
            harness.job(job.id).await
        } else {
            job_now
        };

        let park = target
            .plan_transition(
                RepairState::AwaitingReview,
                TransitionReason::Review(reason),
            )
            .expect("can park");
        harness
            .store
            .apply(job.id, park, TransitionUpdate::default())
            .await
            .expect("parked");

        let (_, body) = get(
            support::router(harness.deps.clone(), None),
            &format!("/api/v1/jobs/{}", job.id),
        )
        .await;
        assert_eq!(
            body["actions"]["approve_resume"]["available"],
            expected,
            "reason {reason:?} should{} offer approve-resume",
            if expected { "" } else { " not" }
        );
    }
}

#[tokio::test]
async fn an_unknown_filter_value_is_rejected_rather_than_answering_a_different_question() {
    let harness = support::Harness::new().await;
    let (status, body) = get(
        support::router(harness.deps.clone(), None),
        "/api/v1/jobs?state=nonsense",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "unknown_field");
}

#[tokio::test]
async fn a_missing_job_is_a_json_404_not_an_empty_body() {
    let harness = support::Harness::new().await;
    let (status, body) = get(
        support::router(harness.deps.clone(), None),
        "/api/v1/jobs/9999",
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

/// `.nest()` gives a nested router axum's default 404 — a status with an **empty
/// body**. A client that always parses JSON chokes on that, and the resulting
/// `SyntaxError` says nothing about the real mistake.
#[tokio::test]
async fn an_unknown_api_path_is_json_rather_than_an_empty_404() {
    let harness = support::Harness::new().await;
    let (status, body) = get(
        support::router(harness.deps.clone(), None),
        "/api/v1/no-such-endpoint",
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        !body.is_null(),
        "an empty body here is the bug this test exists for"
    );
    assert_eq!(body["error"]["code"], "not_found");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("a message")
            .contains("no-such-endpoint"),
        "naming the path is what makes a typo self-diagnosing"
    );
}

#[tokio::test]
async fn the_wrong_method_on_an_api_path_is_also_json() {
    let harness = support::Harness::new().await;
    let response = support::router(harness.deps.clone(), None)
        .oneshot(
            Request::post("/api/v1/dashboard")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body: Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(body["error"]["code"], "method_not_allowed");
}

/// The session endpoint is how the SPA discovers whether there is anything to
/// sign in to, so it must answer before the client has a credential.
#[tokio::test]
async fn the_session_endpoint_answers_without_a_credential() {
    let harness = support::Harness::new().await;

    let (status, body) = get(
        support::router(harness.deps.clone(), Some("t0ken".to_owned())),
        "/api/v1/session",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["auth"], "set");
    assert_eq!(
        body["authenticated"], false,
        "no credential was sent, so the client should be sent to sign in"
    );
    assert!(body["app"]["version"].is_string());
}

#[tokio::test]
async fn with_no_token_configured_every_request_is_already_authenticated() {
    let harness = support::Harness::new().await;
    let (_, body) = get(
        support::router(harness.deps.clone(), None),
        "/api/v1/session",
    )
    .await;

    assert_eq!(body["auth"], "unset");
    assert_eq!(
        body["authenticated"], true,
        "the middleware is a no-op with no token, so sending the client to a \
         login screen with nothing to type would be wrong"
    );
}

/// Everything under `/api/v1` needs a credential when one is configured — the
/// property `tests/settings.rs` already asserts for `/settings`, restated for the
/// surface that replaces it.
#[tokio::test]
async fn api_routes_require_the_token_when_one_is_set() {
    let harness = support::Harness::new().await;

    for path in ["/api/v1/dashboard", "/api/v1/jobs", "/api/v1/diagnostics"] {
        let (status, _) = get(
            support::router(harness.deps.clone(), Some("t0ken".to_owned())),
            path,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} was not guarded");

        let (status, _) = get_with(
            support::router(harness.deps.clone(), Some("t0ken".to_owned())),
            path,
            Some("t0ken"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{path} rejected a valid token");
    }
}

#[tokio::test]
async fn diagnostics_reports_staging_as_configured_or_not_rather_than_as_a_dash() {
    let harness = support::Harness::new().await;
    let (status, body) = get(
        support::router(harness.deps.clone(), None),
        "/api/v1/diagnostics",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["staging"]["configured"], true,
        "the harness stages for real"
    );
    assert_eq!(body["download_client"]["stub"], true);
    assert!(body["ready"].as_bool().expect("a boolean"));
}
