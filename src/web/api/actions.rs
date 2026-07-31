//! Operator actions over JSON.
//!
//! Every guard lives in [`crate::web::review`] and is called from here — not
//! reimplemented. That is deliberate: `approve_resume`'s double guard, the
//! remove-then-discard order, and "if the discard fails the job is left alone"
//! are safety rules, and a second copy of a safety rule is a second thing that
//! can be wrong.
//!
//! Each handler returns the **refreshed job**, so the client needs no follow-up
//! `GET` and cannot render a stale chip next to a fresh success message.

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};

use crate::{
    repair::{JobId, RepairState},
    web::{AppState, review},
};

use super::{
    error::ApiError,
    jobs::{actions_for, load},
    view::{Actions, JobView},
};

/// Cap on a bulk action, matching the job list's page cap: one request must not
/// be able to ask for an unbounded number of transactions.
const MAX_BULK: usize = 200;

/// What every single-job action returns.
#[derive(Serialize)]
struct Outcome<'a> {
    job: JobView<'a>,
    /// Recomputed after the transition, so a client that disables a button on
    /// `available: false` never has to guess which ones changed.
    actions: Actions,
    /// Only meaningful for `choose-candidate`: whether that was the last file
    /// needing a decision. `null` for every other action.
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved: Option<bool>,
}

async fn outcome(
    state: &AppState,
    id: JobId,
    resolved: Option<bool>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runtime = state.runtime.current();
    let job = load(&runtime, id).await?;
    let files = runtime.deps.store.planned_files(id).await?;

    let body = Outcome {
        job: JobView::new(&job),
        actions: actions_for(&job, &files),
        resolved,
    };
    Ok(Json(serde_json::to_value(&body).map_err(|error| {
        tracing::error!(%error, "could not serialise an action outcome");
        ApiError::Internal("The action succeeded but could not be reported.")
    })?))
}

macro_rules! simple_action {
    ($name:ident, $action:ident) => {
        pub async fn $name(
            State(state): State<AppState>,
            Path(id): Path<i64>,
        ) -> Result<Json<serde_json::Value>, ApiError> {
            let runtime = state.runtime.current();
            review::$action(&runtime, id).await?;
            outcome(&state, JobId(id), None).await
        }
    };
}

simple_action!(retry, retry_action);
simple_action!(restart, restart_action);
simple_action!(abandon, abandon_action);
simple_action!(abandon_and_discard, abandon_and_discard_action);
simple_action!(approve_resume, approve_resume_action);

#[derive(Deserialize)]
pub struct ChooseCandidate {
    /// The torrent-relative path this decision is about. Parsed through
    /// `SafeRelativePath::parse` by the action, never joined onto a directory
    /// before that.
    torrent_path: String,
    /// An index into the candidate list the server itself recorded when it
    /// parked the job — **not** a path. An operator can therefore only ever pick
    /// something matching already discovered and offered.
    candidate_index: usize,
}

pub async fn choose_candidate(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ChooseCandidate>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runtime = state.runtime.current();
    let chosen =
        review::choose_candidate_action(&runtime, id, &body.torrent_path, body.candidate_index)
            .await?;
    outcome(&state, JobId(id), Some(chosen.resolved)).await
}

/// Reclaim a completed repair's staging directory.
///
/// New in `docs/todos/0021-a-react-operator-ui.md`. Completed only: the tracker
/// has cleared the hit-and-run, so the staged copy has done the job it existed
/// for, and the UI previously just said "deleting it is not automatic yet".
///
/// The sequence is `abandon_and_discard`'s, for the same reason: the torrent
/// leaves the client *before* its data is deleted, because deleting files out
/// from under a torrent the client still believes it is seeding is exactly the
/// aliased-data hazard `assess_data` exists to prevent. `remove` never passes
/// `delete_files`, so this only ever touches the job's own staging directory —
/// never a library file. **It stops that torrent seeding**, which the client's
/// confirmation says out loud.
///
/// Unlike the other actions this writes no transition: the job is already
/// `completed`, which is terminal, and inventing a state to move it to would be
/// worse than recording the fact. It goes in the audit trail as a progress row.
pub async fn discard_staging(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runtime = state.runtime.current();
    let job = load(&runtime, JobId(id)).await?;

    if job.state != RepairState::Completed {
        return Err(ApiError::Conflict(
            "Staged files can only be discarded once the tracker has cleared the \
             hit-and-run."
                .to_owned(),
        ));
    }
    let Some(staging_dir) = job.staging_dir.clone() else {
        return Err(ApiError::Conflict(
            "This repair has nothing staged to discard.".to_owned(),
        ));
    };

    if let Some(info_hash) = job.info_hash
        && let Err(error) = runtime.deps.client.remove(info_hash, false).await
    {
        // Not fatal, exactly as in `abandon_and_discard`: the torrent may
        // already be gone.
        tracing::warn!(job = %job.id, %error, "could not remove torrent from the download client");
    }

    // A failed discard leaves the job completely alone — no half-done state, and
    // the operator can try again once they have fixed whatever blocked it.
    if let Err(error) = runtime.deps.staging.discard(&staging_dir).await {
        return Err(ApiError::Conflict(format!(
            "Could not clear the staging directory, so nothing was changed: {error}"
        )));
    }

    tracing::info!(job = %job.id, action = "discard_staging", "operator action");
    runtime
        .deps
        .events
        .publish(crate::events::EventKind::JobProgress { job: job.id });

    outcome(&state, JobId(id), None).await
}

#[derive(Deserialize)]
pub struct BulkRequest {
    /// A real JSON array. The `Form<Vec<i64>>` workaround this replaces existed
    /// only because `serde_urlencoded` cannot decode a field-level sequence —
    /// a trap that already shipped once as a silent 422 on
    /// `POST /jobs/bulk/retry`.
    ids: Vec<i64>,
}

#[derive(Serialize)]
pub struct BulkResponse {
    action: &'static str,
    applied: usize,
    total: usize,
    results: Vec<BulkResult>,
}

#[derive(Serialize)]
struct BulkResult {
    id: i64,
    ok: bool,
    /// The server's own refusal text for this job, so a summary dialog can show
    /// twenty different reasons rather than one generic failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// 200 with a per-job outcome array, not 207.
///
/// Multi-Status is WebDAV; no `fetch` client special-cases it, and an explicit
/// per-item array is what the HTML result table already was. Each job is applied
/// independently — twenty jobs sharing one problem must not stop the other
/// nineteen from being fixed.
async fn bulk(
    state: AppState,
    action: &'static str,
    ids: Vec<i64>,
    run: impl Fn(
        std::sync::Arc<crate::bootstrap::Runtime>,
        i64,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<(), crate::web::error::WebError>> + Send>,
    >,
) -> Result<Json<BulkResponse>, ApiError> {
    if ids.len() > MAX_BULK {
        return Err(ApiError::Conflict(format!(
            "That is {} repairs; {MAX_BULK} is the most one request may change at once.",
            ids.len()
        )));
    }

    let runtime = state.runtime.current();
    let mut results = Vec::with_capacity(ids.len());
    let mut applied = 0;
    let mut changed = Vec::new();

    for id in &ids {
        match run(runtime.clone(), *id).await {
            Ok(()) => {
                applied += 1;
                changed.push(JobId(*id));
                results.push(BulkResult {
                    id: *id,
                    ok: true,
                    message: None,
                });
            }
            Err(error) => {
                let message = match ApiError::from(error) {
                    ApiError::Conflict(message) | ApiError::Invalid { message, .. } => message,
                    ApiError::NotFound(message) | ApiError::Internal(message) => message.to_owned(),
                    ApiError::UnknownField(key) => key,
                };
                results.push(BulkResult {
                    id: *id,
                    ok: false,
                    message: Some(message),
                });
            }
        }
    }

    // One event for the batch. Two hundred events through a 256-slot channel
    // would lag every subscriber straight off it.
    if !changed.is_empty() {
        runtime
            .deps
            .events
            .publish(crate::events::EventKind::JobsChanged { jobs: changed });
    }

    Ok(Json(BulkResponse {
        action,
        applied,
        total: ids.len(),
        results,
    }))
}

pub async fn bulk_retry(
    State(state): State<AppState>,
    Json(body): Json<BulkRequest>,
) -> Result<Json<BulkResponse>, ApiError> {
    bulk(state, "retry", body.ids, |runtime, id| {
        Box::pin(async move { review::retry_action(&runtime, id).await })
    })
    .await
}

pub async fn bulk_abandon(
    State(state): State<AppState>,
    Json(body): Json<BulkRequest>,
) -> Result<Json<BulkResponse>, ApiError> {
    bulk(state, "abandon", body.ids, |runtime, id| {
        Box::pin(async move { review::abandon_action(&runtime, id).await })
    })
    .await
}
