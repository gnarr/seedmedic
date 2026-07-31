//! No response from any route ever contains a secret's value — from any
//! source, on any page, in a body or a header.
//!
//! This generalises three narrower tests that already exist
//! (`web::settings::render::tests::no_settings_page_ever_renders_a_sentinel_secret_value`,
//! `render::tests::an_environment_sourced_secret_shows_the_variable_and_no_input`,
//! and `tests/status.rs::no_secret_appears_in_the_status_page_html`) and is
//! strictly stronger than all three, because it walks the whole route table
//! rather than the pages somebody thought to check.
//!
//! It exists because `docs/todos/0021-a-react-operator-ui.md` moves rendering
//! from `maud` in this crate to TypeScript over a JSON API. Today "a secret
//! cannot reach a browser" is enforced by the compiler — `Secret` has no
//! `Serialize` impl, so `Config` derives only `Deserialize`, so `Json(config)`
//! does not build — and by `web::tests::nothing_under_src_web_calls_expose`.
//! Both survive 0021, but a JSON boundary is a new way to get this wrong, so
//! the property gets a test that does not care how the response was produced.
//!
//! Deliberately blunt, in the same spirit as the `.expose(` grep: a substring
//! search over the raw bytes. It cannot be defeated by a serializer nobody
//! thought about, a debug impl, an error message that echoes its input, or a
//! future template engine.

mod support;

use std::{path::PathBuf, sync::Arc};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use seedmedic::{bootstrap, config::Config, runtime::RuntimeHandle};
use tower::ServiceExt;

/// One sentinel per secret, per source, all distinct — so a failure names
/// which one leaked rather than only that something did.
const INLINE_AUTH: &str = "SENTINEL-INLINE-AUTH-TOKEN";
const INLINE_TRACKER: &str = "SENTINEL-INLINE-TRACKER-KEY";
const INLINE_PASSWORD: &str = "SENTINEL-INLINE-CLIENT-PASSWORD";
const INLINE_ARR: &str = "SENTINEL-INLINE-ARR-KEY";
const ENV_TRACKER: &str = "SENTINEL-ENV-TRACKER-KEY";
const FILE_TRACKER: &str = "SENTINEL-FILE-TRACKER-KEY";

const SENTINELS: &[(&str, &str)] = &[
    ("server.auth_token (inline)", INLINE_AUTH),
    ("trackers.0.api_key (inline)", INLINE_TRACKER),
    ("download_client.password (inline)", INLINE_PASSWORD),
    ("arr.0.api_key (inline)", INLINE_ARR),
    ("trackers.1.api_key (environment)", ENV_TRACKER),
    ("trackers.2.api_key (file)", FILE_TRACKER),
];

/// The environment variable feeding the env-sourced tracker. Derived from that
/// tracker's `id`, not a fixed name like `SEEDMEDIC_SERVER_AUTH_TOKEN`: `cargo
/// test` runs test binaries in parallel and this variable is process-global, so
/// a fixed name would race the equivalent tests in `web::settings::save` and
/// `web::settings::render` — a race that has already been fixed once, in
/// `b24aace`.
const ENV_VAR: &str = "SEEDMEDIC_TRACKER_LEAKTESTENV_API_KEY";

/// Every path that returns a rendered response, with the method to use.
///
/// Hand-maintained on purpose: a route added without a thought about whether it
/// can leak is exactly the failure this file exists to prevent, so the list
/// should have to be edited. `route_inventory_covers_every_registered_route`
/// keeps it honest.
///
/// GET-only for now. The write routes take bodies and change state, and the
/// three `POST /settings/*/test` probes are already covered by
/// `tests/settings_connectivity.rs` (which asserts they make *no outbound
/// request at all* with a blank secret — the stronger property). Extend this
/// list to the JSON write routes as 0021 adds them.
const ROUTES: &[&str] = &[
    "/",
    "/status",
    "/jobs/1",
    "/login",
    "/health",
    "/settings",
    "/settings/server",
    "/settings/staging",
    "/settings/library",
    "/settings/policy",
    "/settings/worker",
    "/settings/download-client",
    "/settings/integrations",
    "/settings/trackers",
    "/settings/arr",
    "/settings/trackers/0/remove",
    "/settings/arr/0/remove",
];

/// A real `config.toml` on disk with a secret from each of the three sources,
/// because `/settings` reads the file through `ConfigDocument` rather than the
/// loaded `Config` — so a leak could come from either, and only a real file
/// exercises both.
struct Env {
    _dir: tempfile::TempDir,
    config_path: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let library = dir.path().join("library");
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&library).expect("mkdir library");
        std::fs::create_dir_all(&staging).expect("mkdir staging");

        let key_file = dir.path().join("tracker.key");
        std::fs::write(&key_file, format!("{FILE_TRACKER}\n")).expect("write key file");

        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
# A comment, so a leak through the raw-TOML disclosure would show up too.
[database]
path = "{db}"

[server]
auth_token = "{INLINE_AUTH}"

[staging]
root = "{staging}"

[library]
roots = ["{library}"]

[worker]
owner = "no-secret-leaks-test"

# Every base_url is a port nothing listens on, so any probe or summary call
# fails immediately instead of waiting on a resolver.
[[trackers]]
id = "inline"
kind = "unit3d"
base_url = "http://127.0.0.1:1/"
api_key = "{INLINE_TRACKER}"

[[trackers]]
id = "leaktestenv"
kind = "unit3d"
base_url = "http://127.0.0.1:1/"

[[trackers]]
id = "fromfile"
kind = "unit3d"
base_url = "http://127.0.0.1:1/"
api_key_file = "{key_file}"

[download_client]
kind = "qbittorrent"
base_url = "http://127.0.0.1:1/"
username = "admin"
password = "{INLINE_PASSWORD}"

[[arr]]
kind = "sonarr"
name = "sonarrone"
base_url = "http://127.0.0.1:1/"
api_key = "{INLINE_ARR}"
"#,
                db = dir.path().join("seedmedic.db").display(),
                staging = staging.display(),
                library = library.display(),
                key_file = key_file.display(),
            ),
        )
        .expect("write config");

        Self {
            _dir: dir,
            config_path,
        }
    }

    async fn start(&self) -> Arc<RuntimeHandle> {
        // SAFETY: set before the runtime is built and removed immediately
        // after, and this binary spawns no other thread that reads the
        // environment in between.
        unsafe { std::env::set_var(ENV_VAR, ENV_TRACKER) };
        let config = Config::load_from(&self.config_path).expect("valid config");
        unsafe { std::env::remove_var(ENV_VAR) };

        assert!(
            matches!(
                config.trackers[1].api_key.source(),
                seedmedic::config::SecretSource::Environment { .. }
            ),
            "the env-sourced tracker key did not resolve from the environment, \
             so this test would not be checking that source at all"
        );
        assert!(
            matches!(
                config.trackers[2].api_key.source(),
                seedmedic::config::SecretSource::File { .. }
            ),
            "the file-sourced tracker key did not resolve from its file, so \
             this test would not be checking that source at all"
        );

        let persistent = bootstrap::open(&config).await.expect("open");
        RuntimeHandle::start(&config, persistent, self.config_path.clone())
            .await
            .expect("start")
    }
}

#[tokio::test]
async fn no_secret_value_appears_in_any_response() {
    let env = Env::new();
    let handle = env.start().await;
    let bind = "127.0.0.1:0".parse().expect("bind address");

    for path in ROUTES {
        // A fresh router per request: `oneshot` consumes it, and building it
        // from the same handle keeps every request on one runtime generation.
        let router = seedmedic::web::router(handle.clone(), bind);
        let response = router
            .oneshot(
                Request::get(*path)
                    // The auth token is configured, so every route but
                    // `/health` and `/login` needs a credential. Sending it as
                    // a bearer header is also the point: a response must never
                    // echo back the token it was authenticated with.
                    .header(header::AUTHORIZATION, format!("Bearer {INLINE_AUTH}"))
                    .header(header::ACCEPT, "text/html")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        let status = response.status();
        assert!(
            status.is_success() || status.is_client_error(),
            "{path} returned {status}; a 5xx means this route was not really \
             exercised and the assertions below prove nothing"
        );

        for (name, value) in response.headers() {
            let rendered = String::from_utf8_lossy(value.as_bytes()).into_owned();
            for (label, sentinel) in SENTINELS {
                assert!(
                    !rendered.contains(sentinel),
                    "{label} leaked into {path}'s {name} response header"
                );
            }
        }

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let body = String::from_utf8_lossy(&body);
        for (label, sentinel) in SENTINELS {
            assert!(
                !body.contains(sentinel),
                "{label} leaked into {path} (status {status})"
            );
        }
    }

    handle.stop_worker().await;
}

/// The negative half above passes trivially if the pages stopped saying
/// anything about secrets at all. This is the positive half, and it is what
/// stops someone "fixing" a leak by deleting the redacted summary: `/status`
/// must still report that each secret *is* set.
#[tokio::test]
async fn the_status_page_still_reports_that_each_secret_is_set() {
    let env = Env::new();
    let handle = env.start().await;
    let bind = "127.0.0.1:0".parse().expect("bind address");

    let router = seedmedic::web::router(handle.clone(), bind);
    let response = router
        .oneshot(
            Request::get("/status")
                .header(header::AUTHORIZATION, format!("Bearer {INLINE_AUTH}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = String::from_utf8_lossy(&body);

    // `redacted_summary` spells a top-level scalar `key = value` and a
    // repeated-section field `key=value`; both spellings are asserted as
    // written rather than normalised, so a change to either is noticed here.
    assert!(body.contains("server.auth_token = set"), "got: {body}");
    assert!(body.contains("api_key=set"), "got: {body}");
    assert!(body.contains("password=set"), "got: {body}");

    handle.stop_worker().await;
}

/// `ROUTES` is hand-maintained, so something has to notice when a route is
/// added without being added here.
///
/// Reads the router's own construction rather than the router itself, because
/// `axum::Router` deliberately exposes no way to enumerate its routes. Blunt for
/// the same reason as everything else in this file: it fails loudly and is
/// trivially fixed, which is the right trade against silently not covering a
/// new page.
#[test]
fn route_inventory_covers_every_registered_route() {
    let sources = [
        include_str!("../src/web/mod.rs"),
        include_str!("../src/web/settings/mod.rs"),
    ];

    let mut missing = Vec::new();
    for source in sources {
        for literal in route_literals(source) {
            // `{id}`-style captures cannot be requested literally; `ROUTES`
            // carries a concrete instance instead, so match on the prefix
            // before the first capture.
            let prefix = literal.split('{').next().unwrap_or(&literal).to_owned();
            let covered = ROUTES
                .iter()
                .any(|route| route.starts_with(&prefix) || prefix.starts_with(route));
            if !covered {
                missing.push(literal);
            }
        }
    }

    assert!(
        missing.is_empty(),
        "these routes are registered but not in tests/no_secret_leaks.rs's \
         ROUTES, so nothing checks them for leaked secrets: {missing:?}"
    );
}

/// Every `.route("…")` path literal in a router-building source file.
fn route_literals(source: &str) -> Vec<String> {
    source
        .match_indices(".route(\"")
        .filter_map(|(offset, matched)| {
            let rest = &source[offset + matched.len()..];
            rest.find('"').map(|end| rest[..end].to_owned())
        })
        .collect()
}
