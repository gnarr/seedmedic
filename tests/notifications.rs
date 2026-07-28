//! The generic webhook for the events an operator cares about: parked for
//! review, completed, tracker unreachable — see
//! `docs/todos/0012-observability.md`.

mod support;

use std::sync::Arc;

use seedmedic::{
    notify::adapters::webhook::WebhookNotifier,
    repair::{AutoResume, RepairState, SafetyPolicy},
    tracker::TrackerError,
};
use support::{Harness, default_policy};
use url::Url;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method},
};

fn webhook_notifier(server: &MockServer) -> Arc<WebhookNotifier> {
    Arc::new(WebhookNotifier::new(
        Url::parse(&server.uri()).expect("mock server URI parses"),
        reqwest::Client::new(),
    ))
}

#[tokio::test]
async fn a_repair_parked_for_review_notifies() {
    let harness = Harness::with_policy(SafetyPolicy {
        auto_resume: AutoResume::Never,
        ..default_policy()
    });
    let harness = harness.await;
    let server = MockServer::start().await;
    let expectation = Mock::given(method("POST"))
        .and(body_partial_json(serde_json::json!({
            "event": "parked_for_review",
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    harness.discover().await;
    let worker = harness.worker_with_notifier(webhook_notifier(&server));
    let mut job = harness.only_job().await;
    for _ in 0..40 {
        if job.state == RepairState::AwaitingReview {
            break;
        }
        worker.tick().await;
        harness.clock.advance(chrono::Duration::seconds(30));
        job = harness.job(job.id).await;
    }
    assert_eq!(job.state, RepairState::AwaitingReview);

    drop(expectation);
}

#[tokio::test]
async fn a_completed_repair_notifies() {
    let harness = Harness::new().await;
    let server = MockServer::start().await;
    let expectation = Mock::given(method("POST"))
        .and(body_partial_json(serde_json::json!({
            "event": "completed",
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    harness.discover().await;
    let worker = harness.worker_with_notifier(webhook_notifier(&server));
    let mut job = harness.only_job().await;
    for _ in 0..60 {
        if job.state == RepairState::Completed {
            break;
        }
        worker.tick().await;
        harness.clock.advance(chrono::Duration::seconds(30));
        job = harness.job(job.id).await;
        if job.state == RepairState::Seeding {
            harness.tracker.clear_hit_and_run(&harness.torrent_id);
        }
    }
    assert_eq!(job.state, RepairState::Completed);

    drop(expectation);
}

#[tokio::test]
async fn a_tracker_unreachable_past_the_threshold_notifies_once() {
    let harness = Harness::new().await;
    // Establish a baseline success before the outage begins.
    harness.discover().await;

    let server = MockServer::start().await;
    let expectation = Mock::given(method("POST"))
        .and(body_partial_json(serde_json::json!({
            "event": "tracker_unreachable",
        })))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount_as_scoped(&server)
        .await;

    let worker = harness.worker_with_notifier(webhook_notifier(&server));

    // Past the default threshold, failing every poll along the way. Two
    // polls: the first is the one that crosses the threshold and notifies;
    // the second, still failing, must not notify again.
    harness.clock.advance(chrono::Duration::seconds(1900));
    harness
        .tracker
        .fail_next_call_with(TrackerError::Transport("down".into()));
    worker.discover().await;
    harness
        .tracker
        .fail_next_call_with(TrackerError::Transport("still down".into()));
    worker.discover().await;

    drop(expectation);
}
