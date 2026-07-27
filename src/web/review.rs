//! Manual review actions.
//!
//! Three, deliberately: resume the step the job stopped at, start over, or give
//! up. Each is one validated transition, recorded in the audit trail with
//! `operator_*` as the reason so it is obvious later that a human did it.
//!
//! Richer review — overriding a candidate, editing the file plan — is
//! `docs/todos/0010-manual-review.md`.

use axum::{
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use serde_json::json;
use tracing::{info, warn};

use crate::repair::{JobId, RepairJob, RepairState, TransitionReason, TransitionUpdate};

use super::{AppState, error::WebError};

pub async fn retry(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let job = load(&state, id).await?;
    let Some(resume_to) = job.review_from_state else {
        return Err(WebError::Refused(
            "This job does not record which step it stopped at, so it cannot be retried. \
             Start it over instead."
                .to_owned(),
        ));
    };

    apply(
        &state,
        &job,
        resume_to,
        TransitionReason::OperatorRetry,
        TransitionUpdate::with_detail(
            json!({ "operator": "retry", "resumed_at": resume_to.as_str() }),
        ),
    )
    .await
}

pub async fn abandon(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let job = load(&state, id).await?;

    apply(
        &state,
        &job,
        RepairState::Failed,
        TransitionReason::OperatorAbandon,
        TransitionUpdate::with_detail(json!({ "operator": "abandon" }))
            .failed_because("abandoned by operator"),
    )
    .await
}

/// Send a job back to the beginning, discarding everything it staged.
///
/// The only destructive action in the UI, and it is confined to the job's own
/// staging directory. Files are never deleted from the download client, because
/// staged data may be hardlinked to the library.
pub async fn restart(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let job = load(&state, id).await?;

    if let Some(info_hash) = job.info_hash
        && let Err(error) = state.deps.client.remove(info_hash, false).await
    {
        // Not fatal: the torrent may never have been added. Startup
        // reconciliation and the recheck step both cope with a stale entry.
        warn!(job = %job.id, %error, "could not remove torrent from the download client");
    }

    if let Some(staging_dir) = &job.staging_dir
        && let Err(error) = state.deps.staging.discard(staging_dir).await
    {
        return Err(WebError::Refused(format!(
            "Could not clear the staging directory, so the job was left alone: {error}"
        )));
    }

    apply(
        &state,
        &job,
        RepairState::Discovered,
        TransitionReason::OperatorRestart,
        TransitionUpdate::with_detail(json!({ "operator": "restart", "staging_discarded": true })),
    )
    .await
}

async fn load(state: &AppState, id: i64) -> Result<RepairJob, WebError> {
    state
        .deps
        .store
        .job(JobId(id))
        .await?
        .ok_or(WebError::NotFound)
}

async fn apply(
    state: &AppState,
    job: &RepairJob,
    to: RepairState,
    reason: TransitionReason,
    update: TransitionUpdate,
) -> Result<Response, WebError> {
    let transition = job.plan_transition(to, reason).map_err(|error| {
        WebError::Refused(format!(
            "{error}. The job may have moved since this page was loaded."
        ))
    })?;

    state.deps.store.apply(job.id, transition, update).await?;
    info!(job = %job.id, from = %job.state, to = %to, action = reason.as_str(), "operator action");

    Ok(Redirect::to(&format!("/jobs/{}", job.id)).into_response())
}
