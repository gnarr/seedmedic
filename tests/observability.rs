//! Logging carries enough context to follow one repair with a single `grep`
//! on its job id — see `docs/todos/0012-observability.md`.

mod support;

use tracing_test::traced_test;

#[tokio::test]
#[traced_test]
async fn a_step_s_log_line_carries_the_job_id_from_the_span() {
    let harness = support::Harness::new().await;
    let job = harness.discover().await;

    harness.tick().await;

    let needle = format!("job={}", job.id);
    assert!(
        logs_contain(&needle),
        "expected a log line carrying `{needle}`"
    );
}
