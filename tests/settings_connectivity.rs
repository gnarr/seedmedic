//! `/settings/*/test` — docs/todos/0019-connection-tests.md.
//!
//! Drives a real `bootstrap::open`/`RuntimeHandle::start`, same as
//! `tests/settings.rs`, because the property under test — a probe using
//! only what is in the submitted form, never the live configuration, and
//! never writing `config.toml` — depends on the real save/reload plumbing
//! being present to *not* be invoked.

use std::path::PathBuf;

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use seedmedic::{bootstrap, config::Config, runtime::RuntimeHandle};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

fn form_request(path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .expect("request")
}

/// Percent-encode just enough (this repo has no urlencoding crate, and a
/// mock server's `127.0.0.1:PORT` uri is not attacker input here) for a
/// `application/x-www-form-urlencoded` body.
fn urlencode(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace(':', "%3A")
        .replace('/', "%2F")
}

struct Env {
    _dir: tempfile::TempDir,
    config_path: PathBuf,
}

impl Env {
    /// A fresh install: an empty `config.toml`, so the real worker this
    /// spawns has nothing configured to poll and can never itself reach a
    /// mock server a test points a *draft* at.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "").expect("write empty config");
        Self {
            _dir: dir,
            config_path,
        }
    }

    async fn start(&self) -> std::sync::Arc<RuntimeHandle> {
        let config = Config::load_from(&self.config_path).expect("valid config");
        let persistent = bootstrap::open(&config)
            .await
            .expect("open persistent state");
        RuntimeHandle::start(&config, persistent, self.config_path.clone())
            .await
            .expect("start")
    }
}

/// The security test: a blank password must refuse before any adapter is
/// built, not merely report a refusal after quietly reaching for the stored
/// value. Without this, pointing `download_client.base_url` at a host you
/// control and leaving the password blank would exfiltrate the operator's
/// real, saved qBittorrent password to it — see the module doc on
/// `src/connectivity.rs` and `docs/todos/0019-connection-tests.md`.
#[tokio::test]
async fn testing_the_download_client_with_a_blank_password_makes_no_request() {
    let attacker_controlled = MockServer::start().await;
    // No mock mounted: any request this server actually receives at all is
    // the failure this test exists to catch.

    let env = Env::new();
    std::fs::write(
        &env.config_path,
        "[download_client]\nkind = \"qbittorrent\"\nusername = \"admin\"\npassword = \"real-saved-password\"\n",
    )
    .expect("seed config with a real saved password");

    let handle = env.start().await;
    let router = seedmedic::web::router(handle.clone(), "127.0.0.1:0".parse().expect("addr"));

    let response = router
        .oneshot(form_request(
            "/settings/download-client/test",
            &format!(
                "download_client.kind=qbittorrent&download_client.base_url={}&\
                 download_client.username=admin&download_client.password=",
                urlencode(&attacker_controlled.uri()),
            ),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(
        body.contains("Enter the password"),
        "the page must refuse with a clear message: {body}"
    );

    let received = attacker_controlled
        .received_requests()
        .await
        .expect("request recording is enabled by default");
    assert!(
        received.is_empty(),
        "a blank password must never reach any adapter, let alone one pointed at an \
         attacker-supplied host"
    );
}

/// A probe must use the row as submitted, not the row as saved — and must
/// never write `config.toml` doing it. Seeds a saved tracker of kind `fake`
/// (in-memory, no network — so the real worker this starts can never
/// itself reach the mock server below) and drafts a `unit3d` tracker over
/// it in the test request; only a probe that actually uses the submitted
/// draft can reach the mock at all.
#[tokio::test]
async fn testing_a_tracker_probes_the_submitted_draft_and_never_writes_the_file() {
    let draft_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/hit-and-runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [],
            "links": { "next": null },
        })))
        .mount(&draft_server)
        .await;

    let env = Env::new();
    std::fs::write(
        &env.config_path,
        "[[trackers]]\nid = \"seeded\"\nkind = \"fake\"\nbase_url = \"http://localhost\"\n",
    )
    .expect("seed config with a saved, harmless fake tracker");
    let before_bytes = std::fs::read(&env.config_path).expect("read config");
    let before_mtime = std::fs::metadata(&env.config_path)
        .expect("metadata")
        .modified()
        .expect("mtime");

    let handle = env.start().await;
    let router = seedmedic::web::router(handle.clone(), "127.0.0.1:0".parse().expect("addr"));

    let response = router
        .oneshot(form_request(
            "/settings/trackers/0/test",
            &format!(
                "trackers.0.kind=unit3d&trackers.0.base_url={}&trackers.0.api_key=draft-key&\
                 trackers.0.token_placement=header",
                urlencode(&draft_server.uri()),
            ),
        ))
        .await
        .expect("response");

    let status = response.status();
    let body = body_text(response).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("reachable"),
        "a probe against the drafted unit3d tracker must succeed: {body}"
    );

    let received = draft_server
        .received_requests()
        .await
        .expect("request recording is enabled by default");
    assert!(
        !received.is_empty(),
        "the probe must have used the drafted kind/base_url — the saved row is a `fake` \
         tracker, which never touches the network at all"
    );

    let after_bytes = std::fs::read(&env.config_path).expect("read config");
    let after_mtime = std::fs::metadata(&env.config_path)
        .expect("metadata")
        .modified()
        .expect("mtime");
    assert_eq!(
        before_bytes, after_bytes,
        "a probe must never write config.toml"
    );
    assert_eq!(
        before_mtime, after_mtime,
        "a probe must not touch the file's mtime"
    );
}

/// Same security property as the download client, for an *arr instance's
/// API key — and for the same reason the download client test seeds a real
/// saved secret rather than testing a blank brand-new row: a brand-new row
/// with no key is simply invalid (`Config::problems()` already refuses it),
/// which would exercise a different code path than the one this feature
/// adds. Seeding an existing row with a real saved key and blanking it only
/// in the test submission is what actually exercises the "blank means
/// unset, not unchanged" rule — see `submitted_secret_is_empty` in
/// `src/web/settings/mod.rs`.
#[tokio::test]
async fn testing_an_arr_instance_with_a_blank_api_key_makes_no_request() {
    let attacker_controlled = MockServer::start().await;

    let env = Env::new();
    std::fs::write(
        &env.config_path,
        "[[arr]]\nkind = \"sonarr\"\nname = \"probe\"\nbase_url = \"http://original-host\"\n\
         api_key = \"real-saved-arr-key\"\n",
    )
    .expect("seed config with a real saved arr api_key");

    let handle = env.start().await;
    let router = seedmedic::web::router(handle.clone(), "127.0.0.1:0".parse().expect("addr"));

    let response = router
        .oneshot(form_request(
            "/settings/arr/0/test",
            &format!(
                "arr.0.base_url={}&arr.0.api_key=",
                urlencode(&attacker_controlled.uri()),
            ),
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = body_text(response).await;
    assert!(
        body.contains("Enter the API key"),
        "the page must refuse with a clear message: {body}"
    );

    let received = attacker_controlled
        .received_requests()
        .await
        .expect("request recording is enabled by default");
    assert!(
        received.is_empty(),
        "a blank key must never reach any adapter, let alone one pointed at a base_url the \
         test submission just changed to an attacker-controlled host"
    );
}
