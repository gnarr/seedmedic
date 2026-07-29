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

    let jobs = runtime.deps.store.jobs(i64::MAX).await.unwrap_or_default();

    let mut repairs_by_state: HashMap<&'static str, usize> = HashMap::new();
    let mut staged_bytes = 0u64;
    for job in &jobs {
        *repairs_by_state.entry(job.state.as_str()).or_insert(0) += 1;
        if let Some(staging_dir) = &job.staging_dir {
            staged_bytes += runtime
                .deps
                .staging
                .usage(staging_dir)
                .await
                .unwrap_or_default();
        }
    }

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
