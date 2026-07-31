//! `/metrics`, present only when built with the `metrics` feature. Still
//! gated at runtime by `metrics.enabled` in config — see `crate::metrics`.

use std::collections::HashMap;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};

use super::AppState;

pub async fn handler(State(state): State<AppState>) -> impl IntoResponse {
    let runtime = state.runtime.current();
    if !runtime.metrics_enabled {
        return (StatusCode::NOT_FOUND, "metrics disabled\n").into_response();
    }

    // Two aggregate queries, not `jobs(i64::MAX)` plus a filesystem walk per
    // staged job. A scrape target is polled on an interval by construction, so
    // this was the worst O(n) offender in the codebase: every scrape read every
    // column of every row, built a `RepairJob` from each, and then `statvfs`'d
    // and walked one directory per job.
    //
    // `staged_bytes` consequently changes meaning: it is now what SeedMedic
    // *recorded* it staged rather than what is on disk this second. That is the
    // right trade for a counter — it is derived from durable state, so it no
    // longer moves because an unrelated process wrote to the staging volume —
    // but it is a change, and `docs/todos/0021-a-react-operator-ui.md` says so.
    let counts = runtime.deps.store.counts().await.unwrap_or_default();
    let staged_bytes = runtime
        .deps
        .store
        .staged_bytes_declared()
        .await
        .unwrap_or_default();

    let repairs_by_state: HashMap<&'static str, i64> = counts
        .by_state
        .iter()
        .map(|(state, count)| (state.as_str(), *count))
        .collect();

    let mut body =
        serde_json::to_value(runtime.deps.metrics.snapshot()).expect("Snapshot always serializes");
    if let serde_json::Value::Object(fields) = &mut body {
        fields.insert(
            "repairs_by_state".to_owned(),
            serde_json::json!(repairs_by_state),
        );
        fields.insert("staged_bytes".to_owned(), serde_json::json!(staged_bytes));
    }

    Json(body).into_response()
}
