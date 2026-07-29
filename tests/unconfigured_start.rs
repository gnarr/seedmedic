//! Proves the premise of `docs/todos/0015-start-without-a-configuration-file.md`
//! end to end: `Config::default()` — what a process gets with no
//! configuration file at all — is a process that starts, serves pages, ticks
//! its worker, and creates nothing on disk beyond its own database. And a
//! repair that reaches the one step an unset `staging.root` cannot support
//! parks for review saying so, rather than guessing a path or crashing.

mod support;

use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use seedmedic::{
    bootstrap,
    config::Config,
    diagnostics::Diagnostics,
    repair::{
        RepairStore, ReviewReason, WorkerHealth, reconcile::reconcile_on_startup,
        worker::RepairDeps,
    },
    staging::adapters::unconfigured::UnconfiguredStaging,
    web,
};
use tower::ServiceExt;

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

#[tokio::test]
async fn a_default_configuration_serves_pages_ticks_and_creates_nothing_on_disk() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut config = Config::default();
    config.database.path = temp.path().join("seedmedic.db");

    let app = bootstrap::build(config, std::path::Path::new("config.toml"))
        .await
        .expect("Config::default() must be startable");

    // The same startup sequence main.rs runs.
    reconcile_on_startup(&app.deps, &app.worker_config.owner).await;
    app.worker().tick().await;

    let router = web::router(
        app.deps.clone(),
        app.auth_token.clone(),
        app.health_threshold,
        app.config_summary.clone(),
        app.metrics_enabled,
        app.chrome.clone(),
    );

    let health = router
        .clone()
        .oneshot(
            Request::get("/health")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(
        health.status(),
        StatusCode::OK,
        "an idle, unconfigured worker is still ready"
    );

    let index = router
        .clone()
        .oneshot(Request::get("/").body(Body::empty()).expect("request"))
        .await
        .expect("response");
    assert_eq!(index.status(), StatusCode::OK);
    assert!(
        body_text(index)
            .await
            .contains("No hit-and-runs discovered yet.")
    );

    let status = router
        .oneshot(
            Request::get("/status")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(status.status(), StatusCode::OK);
    let status_body = body_text(status).await;
    assert!(status_body.contains("No trackers configured."));
    assert!(status_body.contains("not configured"), "{status_body}");

    // Nothing under the temp directory beyond the sqlite database itself —
    // no staging directory, nothing guessed.
    let created: Vec<String> = std::fs::read_dir(temp.path())
        .expect("read temp dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    for name in &created {
        assert!(
            name.starts_with("seedmedic.db"),
            "unexpected file created under the temp directory: {name} ({created:?})"
        );
    }
}

/// A repair that gets far enough to need staging — with everything else
/// configured but `staging.root` left unset — parks for review naming
/// exactly that setting, instead of creating a directory or crashing.
#[tokio::test]
async fn a_job_that_reaches_matched_parks_for_review_naming_staging_root() {
    let harness = support::Harness::new().await;

    let deps = Arc::new(RepairDeps {
        store: harness.deps.store.clone(),
        trackers: harness.deps.trackers.clone(),
        inspector: harness.deps.inspector.clone(),
        candidate_sources: harness.deps.candidate_sources.clone(),
        staging: Arc::new(UnconfiguredStaging),
        client: harness.deps.client.clone(),
        clock: harness.deps.clock.clone(),
        policy: harness.deps.policy,
        category: harness.deps.category.clone(),
        worker_health: Arc::new(WorkerHealth::default()),
        diagnostics: Arc::new(Diagnostics::new(std::iter::empty())),
        client_is_stub: harness.deps.client_is_stub,
        #[cfg(feature = "metrics")]
        metrics: Arc::new(seedmedic::metrics::Metrics::default()),
        notifier: harness.deps.notifier.clone(),
        tracker_unreachable_threshold: harness.deps.tracker_unreachable_threshold,
    });
    let worker = support::worker_for(deps);

    harness.discover().await;
    let job = harness
        .run_until_with(&worker, 20, |job| {
            job.state == seedmedic::repair::RepairState::AwaitingReview
        })
        .await;

    assert_eq!(job.review_reason, Some(ReviewReason::AdapterNotImplemented));
    let history = harness.store.history(job.id).await.expect("history");
    let last_detail = history
        .last()
        .and_then(|record| record.detail.as_ref())
        .expect("a detail recording why the job parked");
    assert!(
        last_detail.to_string().contains("staging.root"),
        "{last_detail:?}"
    );

    // The real staging directory the harness set up (but this deps object
    // never touches) must still be empty: nothing was guessed or created.
    let staged: Vec<_> = std::fs::read_dir(&harness.staging_root)
        .expect("read staging root")
        .collect();
    assert!(staged.is_empty(), "{staged:?}");
}
