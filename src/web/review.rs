//! Manual review actions.
//!
//! Each review action is one validated transition, recorded in the audit trail
//! with `operator_*` as the reason so it is obvious later that a human did it.
//!
//! Richer review — overriding a candidate, editing the file plan — is
//! `docs/todos/0010-manual-review.md`.

use axum::{
    Form,
    extract::{Path, State},
    response::{IntoResponse, Redirect, Response},
};
use maud::html;
use serde::Deserialize;
use serde_json::json;
use tracing::{info, warn};

use crate::{
    library::{MatchConfidence, MatchEvidence},
    repair::{
        JobId, JobPatch, RepairJob, RepairState, ReviewReason, StoreError, TransitionReason,
        TransitionUpdate,
    },
    torrent::SafeRelativePath,
};

use super::{AppState, error::WebError, jobs::ambiguous_candidates, layout};

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

/// Approve a resume held only by `policy.auto_resume = "never"`.
///
/// This is the one review action that changes what an automated decision
/// would otherwise do, rather than just moving the job — so it is scoped as
/// tightly as the state machine can make it: only offered (see
/// [`super::jobs::review_panel`]) and only accepted when the job parked
/// specifically for [`ReviewReason::AutoResumeDisabled`], never for any other
/// reason a job might be parked. The approval itself is a `resume_approved`
/// flag on this job alone; see [`crate::repair::policy::decide_resume`] for
/// the one thing it is allowed to override.
pub async fn approve_resume(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let job = load(&state, id).await?;
    let Some(resume_to) = job.review_from_state else {
        return Err(WebError::Refused(
            "This job does not record which step it stopped at, so the resume cannot be approved."
                .to_owned(),
        ));
    };
    if job.review_reason != Some(ReviewReason::AutoResumeDisabled) {
        return Err(WebError::Refused(
            "This job is not parked only because auto-resume is disabled, so there is nothing \
             to approve."
                .to_owned(),
        ));
    }

    apply(
        &state,
        &job,
        resume_to,
        TransitionReason::OperatorRetry,
        TransitionUpdate::with_detail(
            json!({ "operator": "approve_resume", "resumed_at": resume_to.as_str() }),
        )
        .patch(JobPatch {
            resume_approved: Some(true),
            ..JobPatch::default()
        }),
    )
    .await
}

#[derive(Deserialize)]
pub struct ChooseCandidateForm {
    torrent_path: String,
    candidate_index: usize,
}

/// Resolve one file a matching step could not decide on, from among the
/// candidates it actually considered.
///
/// The candidate is chosen by index into the list recorded on the transition
/// that parked the job — never a path taken straight from the request — so an
/// operator can only ever pick something matching itself already discovered
/// and offered; see `docs/todos/0010-manual-review.md`'s "editing torrent
/// paths... beyond choosing among discovered candidates" scope note.
///
/// Only patches `repair_job_files` until every file has a source: a repair
/// needs all of them, so the job stays parked, un-transitioned, until the
/// last file is resolved — at which point this is what completes matching in
/// the operator's stead, moving the job on to staging and a full recheck like
/// any other match.
pub async fn choose_candidate(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<ChooseCandidateForm>,
) -> Result<Response, WebError> {
    let job = load(&state, id).await?;
    let torrent_path = SafeRelativePath::parse(&form.torrent_path)
        .map_err(|error| WebError::Refused(format!("not a valid torrent path: {error}")))?;

    let history = state.deps.store.history(job.id).await?;
    let candidates = ambiguous_candidates(&history, torrent_path.as_str());
    let chosen = candidates.get(form.candidate_index).ok_or_else(|| {
        WebError::Refused(
            "That candidate is no longer on offer for this file. The job may have moved since \
             this page was loaded."
                .to_owned(),
        )
    })?;

    let mut files = state.deps.store.planned_files(job.id).await?;
    let file = files
        .iter_mut()
        .find(|file| file.torrent_path == torrent_path)
        .ok_or_else(|| WebError::Refused("This job has no such file in its plan.".to_owned()))?;
    file.source = Some(chosen.path.clone());
    file.confidence = Some(MatchConfidence::Operator);
    file.evidence = Some(MatchEvidence {
        size_matches: true,
        name_matches: false,
        candidates_with_matching_size: candidates.len(),
        piece_verified: false,
    });

    let detail = json!({
        "operator": "choose_candidate",
        "torrent_path": torrent_path.as_str(),
        "chosen": chosen,
    });

    if files.iter().any(|file| file.source.is_none()) {
        // Other files still need a choice; nothing to transition yet.
        state
            .deps
            .store
            .record_progress(
                job.id,
                JobPatch {
                    files: Some(files),
                    ..JobPatch::default()
                },
            )
            .await?;
        info!(job = %job.id, path = %torrent_path, "operator chose a candidate; job still parked");
        return Ok(Redirect::to(&format!("/jobs/{}", job.id)).into_response());
    }

    apply(
        &state,
        &job,
        RepairState::Matched,
        TransitionReason::OperatorChooseCandidate,
        TransitionUpdate::with_detail(detail).patch(JobPatch {
            staging_dir: Some(job.default_staging_dir()),
            files: Some(files),
            ..JobPatch::default()
        }),
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

/// Abandon a job and reclaim what it staged.
///
/// The torrent must be gone from the client before its staged data is
/// deleted — the same rule `restart` follows, and the reason is the same:
/// deleting files out from under a torrent the client still thinks it is
/// seeding is exactly the aliased-data danger `assess_data` exists to avoid.
/// `remove` never passes `delete_files`, so this only ever touches the job's
/// own staging directory, never library files.
pub async fn abandon_and_discard(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, WebError> {
    let job = load(&state, id).await?;

    if let Some(info_hash) = job.info_hash
        && let Err(error) = state.deps.client.remove(info_hash, false).await
    {
        // Not fatal: the torrent may never have been added, or may already
        // be gone. Startup reconciliation and the recheck step both cope
        // with a stale entry the same way.
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
        RepairState::Failed,
        TransitionReason::OperatorAbandon,
        TransitionUpdate::with_detail(json!({ "operator": "abandon", "staging_discarded": true }))
            .failed_because("abandoned by operator, staging discarded"),
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

#[derive(Deserialize)]
pub struct BulkForm {
    id: Vec<i64>,
}

/// One job's outcome from a bulk action — never an error response for the
/// whole request, since twenty jobs sharing one problem must not stop the
/// other nineteen from being fixed.
struct BulkOutcome {
    id: i64,
    result: Result<(), String>,
}

/// Retry every selected job, each resuming its own recorded step.
///
/// No new transition semantics: this is the same validated `OperatorRetry`
/// transition [`retry`] applies to one job, looped over a list and reporting
/// per-job results instead of stopping at the first problem.
pub async fn bulk_retry(
    State(state): State<AppState>,
    Form(form): Form<BulkForm>,
) -> Result<Response, WebError> {
    bulk(&state, &form.id, "retry", |job| {
        let resume_to = job.review_from_state.ok_or_else(|| {
            "does not record which step it stopped at, so it cannot be retried".to_owned()
        })?;
        Ok((
            resume_to,
            TransitionReason::OperatorRetry,
            TransitionUpdate::with_detail(
                json!({ "operator": "bulk_retry", "resumed_at": resume_to.as_str() }),
            ),
        ))
    })
    .await
}

/// Abandon every selected job.
pub async fn bulk_abandon(
    State(state): State<AppState>,
    Form(form): Form<BulkForm>,
) -> Result<Response, WebError> {
    bulk(&state, &form.id, "abandon", |_job| {
        Ok((
            RepairState::Failed,
            TransitionReason::OperatorAbandon,
            TransitionUpdate::with_detail(json!({ "operator": "bulk_abandon" }))
                .failed_because("abandoned by operator"),
        ))
    })
    .await
}

/// Apply `plan`'s transition to every job in `ids`, independently: one job's
/// conflict (it moved since the page was loaded) or missing-ness must not
/// stop the rest of the batch, so nothing here uses `?` to bail out early.
async fn bulk(
    state: &AppState,
    ids: &[i64],
    action: &'static str,
    plan: impl Fn(&RepairJob) -> Result<(RepairState, TransitionReason, TransitionUpdate), String>,
) -> Result<Response, WebError> {
    let mut outcomes = Vec::with_capacity(ids.len());

    for &id in ids {
        let result = async {
            let job = state
                .deps
                .store
                .job(JobId(id))
                .await
                .map_err(|error: StoreError| error.to_string())?
                .ok_or_else(|| "no such job".to_owned())?;
            let (to, reason, update) = plan(&job)?;
            let transition = job
                .plan_transition(to, reason)
                .map_err(|error| error.to_string())?;
            state
                .deps
                .store
                .apply(job.id, transition, update)
                .await
                .map_err(|error| error.to_string())?;
            Ok(())
        }
        .await;

        if let Err(error) = &result {
            warn!(job = id, action, %error, "bulk operator action failed for one job");
        }
        outcomes.push(BulkOutcome { id, result });
    }

    Ok(bulk_summary(action, &outcomes))
}

fn bulk_summary(action: &str, outcomes: &[BulkOutcome]) -> Response {
    let applied = outcomes.iter().filter(|o| o.result.is_ok()).count();
    let body = html! {
        h2 { "Bulk " (action) }
        p { (applied) " of " (outcomes.len()) " jobs updated." }
        table {
            thead { tr { th { "Job" } th { "Result" } } }
            tbody {
                @for outcome in outcomes {
                    tr {
                        td { a href={ "/jobs/" (outcome.id) } { (outcome.id) } }
                        td {
                            @match &outcome.result {
                                Ok(()) => "done",
                                Err(message) => (message),
                            }
                        }
                    }
                }
            }
        }
        p { a href="/" { "Back to repairs" } }
    };

    layout::page("Bulk action", body).into_response()
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
