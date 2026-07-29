//! `/settings` — docs/todos/0017-the-settings-pages.md.
//!
//! The acceptance test exercises the real `bootstrap::open`/`RuntimeHandle`
//! wiring (not the `support::Harness` shortcut, which never touches
//! `RuntimeHandle`), because the property under test is the whole plan: cold
//! start to a working worker, entirely through the browser, no restart.

mod support;

use std::{path::PathBuf, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use chrono::Utc;
use seedmedic::{
    bootstrap,
    config::Config,
    repair::{
        RepairJob, RepairState, RepairStore, ReviewReason, TransitionReason, TransitionUpdate,
    },
    runtime::RuntimeHandle,
    tracker::{HitAndRun, TrackerId, TrackerTorrentId},
};
use tower::ServiceExt;

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

fn form_request(method: &str, path: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_owned()))
        .expect("request")
}

/// A working directory with the `config.toml` a real `RuntimeHandle` reads
/// from, and the demo tracker's movie file already present in the library —
/// so the demo hit-and-run matches at `Probable` confidence the moment
/// discovery runs, with nothing standing between it and staging except
/// `staging.root` being unset.
struct Env {
    _dir: tempfile::TempDir,
    config_path: PathBuf,
    library_path: PathBuf,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let library_path = dir.path().join("library");
        std::fs::create_dir_all(library_path.join("Demo.Movie.2024.1080p")).expect("mkdir library");
        std::fs::create_dir_all(library_path.join("Demo.Show.S01.1080p")).expect("mkdir library");
        // A `kind = "fake"` tracker always seeds *both* of
        // bootstrap::demo_torrents's hit-and-runs, so both need a library
        // match — otherwise whichever of the two `wait_for` happens to see
        // first could be the other one, parked for `NoCandidates` instead
        // of the unconfigured-staging outcome this test is about.
        std::fs::write(
            library_path.join("Demo.Movie.2024.1080p").join("movie.mkv"),
            vec![0_u8; 1 << 20],
        )
        .expect("write library file");
        std::fs::write(
            library_path.join("Demo.Show.S01.1080p").join("S01E01.mkv"),
            vec![0_u8; 2 << 20],
        )
        .expect("write library file");
        std::fs::write(
            library_path.join("Demo.Show.S01.1080p").join("S01E02.mkv"),
            vec![0_u8; 3 << 20],
        )
        .expect("write library file");

        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"
[database]
path = "{db}"

[library]
roots = ["{library}"]

[worker]
owner = "settings-test"
poll_interval_seconds = 1
discovery_interval_seconds = 1

[[trackers]]
id = "demo"
kind = "fake"

[download_client]
kind = "fake"
"#,
                db = dir.path().join("seedmedic.db").display(),
                library = library_path.display(),
            ),
        )
        .expect("write config");

        Self {
            _dir: dir,
            config_path,
            library_path,
        }
    }

    async fn start(&self) -> Arc<RuntimeHandle> {
        let config = Config::load_from(&self.config_path).expect("valid config");
        let persistent = bootstrap::open(&config)
            .await
            .expect("open persistent state");
        let handle = RuntimeHandle::start(&config, persistent, self.config_path.clone())
            .await
            .expect("start");
        // The first tick fires immediately; give discovery+matching+the
        // parked-for-review outcome time to land before polling for it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle
    }
}

/// Poll the real, running worker for a job matching `predicate` rather than
/// driving it by hand — `RuntimeHandle::start` spawns a genuine background
/// task, unlike `support::Harness::tick`.
async fn wait_for(handle: &RuntimeHandle, predicate: impl Fn(&RepairJob) -> bool) -> RepairJob {
    for _ in 0..100 {
        let jobs = handle
            .current()
            .deps
            .store
            .jobs(50)
            .await
            .expect("list jobs");
        if let Some(job) = jobs.iter().find(|job| predicate(job)) {
            return job.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for a matching job");
}

/// The acceptance test for the whole plan: a fresh install with no
/// `staging.root` parks its one repair for review; setting it from the
/// browser and retrying resumes that exact job all the way to `staged`,
/// without restarting the process.
#[tokio::test]
async fn cold_start_to_a_staged_repair_entirely_through_the_browser() {
    let env = Env::new();
    let handle = env.start().await;

    let parked = wait_for(&handle, |job| job.state == RepairState::AwaitingReview).await;
    assert_eq!(
        parked.review_reason,
        Some(ReviewReason::AdapterNotImplemented),
        "the job must park specifically because staging is unconfigured"
    );

    let router = seedmedic::web::router(handle.clone(), "127.0.0.1:0".parse().unwrap());
    let staging_root = env.library_path.parent().unwrap().join("staging");

    let response = router
        .clone()
        .oneshot(form_request(
            "POST",
            "/settings/staging",
            &format!(
                "staging.root={}&staging.min_free_bytes=0",
                urlencode(&staging_root.display().to_string())
            ),
        ))
        .await
        .expect("response");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        body_text(response).await
    );

    assert_eq!(
        handle.current().config.staging.root,
        staging_root,
        "the reload after saving must pick up the new staging.root"
    );

    let response = router
        .clone()
        .oneshot(
            Request::post(format!("/jobs/{}/retry", parked.id))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::SEE_OTHER);

    // The fake download client's recheck completes near-instantly, so the
    // job may already be past `Staged` (into `Rechecking` or further) by the
    // time this notices — "reached staged" means "got there", not "is
    // sitting there right now", so check position in PROGRESSION rather
    // than exact equality.
    let reached_staged_index = RepairState::PROGRESSION
        .iter()
        .position(|state| *state == RepairState::Staged)
        .expect("Staged is in PROGRESSION");
    let staged = wait_for(&handle, |job| {
        job.id == parked.id
            && RepairState::PROGRESSION
                .iter()
                .position(|state| *state == job.state)
                .is_some_and(|index| index >= reached_staged_index)
    })
    .await;
    assert_eq!(staged.id, parked.id);
}

/// Percent-encode just enough (spaces and slashes are the only characters a
/// temp directory path needs) for a `application/x-www-form-urlencoded`
/// body — this repo has no urlencoding crate, and a path is not attacker
/// input here, so a tiny hand-rolled encoder is proportionate.
fn urlencode(value: &str) -> String {
    value.replace('%', "%25").replace(' ', "%20")
}

/// `POST /jobs/bulk/retry` with two `id` values — the case that returns 422
/// against `main` today, because `Form<BulkForm { id: Vec<i64> }>` cannot
/// decode a field-level sequence (see `src/web/AGENTS.md`).
#[tokio::test]
async fn bulk_retry_with_two_ids_works_over_real_http() {
    let harness = support::Harness::new().await;
    let tracker = TrackerId::new("test-tracker");
    let mut ids = Vec::new();

    for i in 0..2 {
        let hit_and_run = HitAndRun {
            tracker: tracker.clone(),
            torrent_id: TrackerTorrentId::new(format!("t-{i}")),
            torrent_name: format!("Show.{i}"),
            info_hash: None,
            size_bytes: 100,
            deadline: None,
            observed_at: Utc::now(),
        };
        let discovered = harness
            .store
            .record_discovery(&hit_and_run)
            .await
            .expect("discover");
        let job = harness
            .store
            .job(discovered.id)
            .await
            .expect("job lookup")
            .expect("job exists");
        let transition = job
            .plan_transition(
                RepairState::AwaitingReview,
                TransitionReason::Review(ReviewReason::NoCandidates),
            )
            .expect("any actionable state can be parked for review");
        harness
            .store
            .apply(discovered.id, transition, TransitionUpdate::default())
            .await
            .expect("park for review");
        ids.push(discovered.id.0);
    }

    let router = support::router(harness.deps.clone(), None);
    let response = router
        .oneshot(form_request(
            "POST",
            "/jobs/bulk/retry",
            &format!("id={}&id={}", ids[0], ids[1]),
        ))
        .await
        .expect("response");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a repeated `id` key must decode, not 422"
    );
    let body = body_text(response).await;
    assert!(body.contains("2 of 2 jobs updated"));

    for id in ids {
        let job = harness
            .store
            .job(seedmedic::repair::JobId(id))
            .await
            .expect("job lookup")
            .expect("job exists");
        assert_eq!(job.state, RepairState::Discovered);
    }
}

/// Settings routes are behind the same bearer-token middleware as
/// everything else — nothing about `/settings` is special-cased out of it.
#[tokio::test]
async fn settings_routes_require_the_auth_token_when_one_is_set() {
    let harness = support::Harness::new().await;
    let router = support::router(harness.deps.clone(), Some("s3cret".to_owned()));

    let response = router
        .oneshot(
            Request::get("/settings")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
