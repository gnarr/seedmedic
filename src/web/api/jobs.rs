//! `GET /api/v1/jobs` and `GET /api/v1/jobs/{id}`.

use axum::{
    Json,
    extract::{Path, Query, State},
};
use serde::Serialize;

use crate::{
    bootstrap::Runtime,
    repair::{
        JobCursor, JobFilter, JobId, JobSort, RepairJob, RepairState, ReviewReason, StoreError,
    },
    tracker::TrackerId,
    web::AppState,
};

use super::{
    error::ApiError,
    view::{Action, Actions, FileView, HistoryView, JobView},
};

/// Bound on `limit`, matching the cap on a bulk action's id list: one request
/// should not be able to ask for an unbounded amount of work.
const MAX_LIMIT: i64 = 200;
const DEFAULT_LIMIT: i64 = 50;

/// The parsed query string.
///
/// **Built from a pair list, never `Query<ListQuery>` with `Vec` fields.**
/// `serde_urlencoded`'s per-value deserializer forwards `seq` to
/// `deserialize_any`, which only ever calls `visit_str` — so a *field* typed as
/// `Vec` fails to decode the moment two `state=` pairs appear, and axum rejects
/// the request before the handler runs, with an **empty body**. That is the same
/// trap `src/web/AGENTS.md` documents for `Form<Vec<(String, String)>>`, which
/// already shipped once as a silent 422 on `POST /jobs/bulk/retry`. A top-level
/// sequence of pairs decodes correctly, in order, duplicates included — so the
/// filters that are genuinely repeatable are collected from that.
#[derive(Debug, Default)]
pub struct ListQuery {
    /// Repeatable. `?state=awaiting_review&state=failed`.
    state: Vec<String>,
    reason: Vec<String>,
    tracker: Vec<String>,
    q: Option<String>,
    sort: Option<String>,
    order: Option<String>,
    limit: Option<i64>,
    cursor: Option<String>,
}

impl ListQuery {
    /// Last value wins for the single-valued keys, matching
    /// `settings::save::last_value_wins` — one convention for duplicate keys
    /// across the whole module rather than two.
    fn from_pairs(pairs: Vec<(String, String)>) -> Self {
        let mut query = Self::default();
        for (key, value) in pairs {
            if value.is_empty() {
                continue;
            }
            match key.as_str() {
                "state" => query.state.push(value),
                "reason" => query.reason.push(value),
                "tracker" => query.tracker.push(value),
                "q" => query.q = Some(value),
                "sort" => query.sort = Some(value),
                "order" => query.order = Some(value),
                // A non-numeric limit falls back to the default rather than
                // failing the request: the operator asked for a list, and the
                // page size is not the point of their request.
                "limit" => query.limit = value.parse().ok(),
                "cursor" => query.cursor = Some(value),
                // Unknown *query* keys are ignored, unlike unknown settings
                // keys: a stale bookmark carrying `?foo=1` should still show the
                // list. An unknown *value* for a known key is still rejected —
                // that one changes which jobs come back.
                _ => {}
            }
        }
        query
    }

    /// Unknown filter values are rejected rather than ignored.
    ///
    /// Silently dropping `?state=nonsense` would answer a different question
    /// than the one asked and look like "no such jobs" — the client would show
    /// an empty list and the operator would believe it.
    fn to_filter(&self) -> Result<JobFilter, ApiError> {
        let states = self
            .state
            .iter()
            .map(|value| {
                RepairState::parse(value)
                    .map_err(|_| ApiError::UnknownField(format!("state={value}")))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let review_reasons = self
            .reason
            .iter()
            .map(|value| {
                ReviewReason::parse(value)
                    .ok_or_else(|| ApiError::UnknownField(format!("reason={value}")))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let sort = match self.sort.as_deref() {
            None | Some("updated_at") => JobSort::UpdatedAt,
            Some("created_at") => JobSort::CreatedAt,
            Some(other) => return Err(ApiError::UnknownField(format!("sort={other}"))),
        };
        let descending = match self.order.as_deref() {
            None | Some("desc") => true,
            Some("asc") => false,
            Some(other) => return Err(ApiError::UnknownField(format!("order={other}"))),
        };

        Ok(JobFilter {
            states,
            review_reasons,
            trackers: self.tracker.iter().map(TrackerId::new).collect(),
            search: self.q.clone(),
            sort,
            descending,
            after: self.cursor.as_deref().and_then(decode_cursor),
            limit: self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        })
    }
}

/// A cursor is `<sort value>|<id>`, base64url-free: the sort value is an RFC
/// 3339 timestamp and the id an integer, neither of which can contain `|`. An
/// opaque encoding would only hide that from the operator reading their own URL.
fn decode_cursor(raw: &str) -> Option<JobCursor> {
    let (sort_value, id) = raw.rsplit_once('|')?;
    Some(JobCursor {
        sort_value: sort_value.to_owned(),
        id: JobId(id.parse().ok()?),
    })
}

fn encode_cursor(job: &RepairJob, sort: JobSort) -> String {
    let value = match sort {
        JobSort::UpdatedAt => job.updated_at,
        JobSort::CreatedAt => job.created_at,
    };
    format!(
        "{}|{}",
        value.to_rfc3339_opts(chrono::SecondsFormat::Micros, true),
        job.id.0
    )
}

#[derive(Serialize)]
pub struct ListResponse<'a> {
    jobs: Vec<JobView<'a>>,
    /// `null` on the last page. Present so the client never has to guess from
    /// `jobs.len() < limit`, which is wrong when the last page is exactly full.
    next_cursor: Option<String>,
    /// Approximate: a second query, so it can disagree with the page by a row
    /// while the worker is writing. The UI presents it as such.
    total_matching: i64,
}

pub async fn list(
    State(state): State<AppState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runtime = state.runtime.current();
    let filter = ListQuery::from_pairs(pairs).to_filter()?;

    let jobs = runtime.deps.store.find_jobs(&filter).await?;
    let total_matching = runtime.deps.store.count_jobs(&filter).await?;

    // A full page implies there may be more; a short one is definitive.
    let next_cursor = (jobs.len() as i64 == filter.limit)
        .then(|| jobs.last().map(|job| encode_cursor(job, filter.sort)))
        .flatten();

    let response = ListResponse {
        jobs: jobs.iter().map(JobView::new).collect(),
        next_cursor,
        total_matching,
    };
    // Serialised eagerly because `JobView` borrows from `jobs`, which does not
    // outlive this function.
    Ok(Json(serde_json::to_value(&response).map_err(|error| {
        tracing::error!(%error, "could not serialise the job list");
        ApiError::Internal("Could not render the repair list.")
    })?))
}

#[derive(Serialize)]
struct DetailResponse<'a> {
    job: JobView<'a>,
    files: Vec<FileView>,
    history: Vec<HistoryView<'a>>,
    /// What is really on disk for this job, measured — one filesystem walk, on a
    /// page an operator opened deliberately. The dashboard uses the store's
    /// declared total instead; see `RepairStore::staged_bytes_declared`.
    staged_bytes: Option<u64>,
    actions: Actions,
}

pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let runtime = state.runtime.current();
    let id = JobId(id);

    let job = runtime
        .deps
        .store
        .job(id)
        .await?
        .ok_or(ApiError::NotFound("No such repair job."))?;
    let files = runtime.deps.store.planned_files(id).await?;
    let history = runtime.deps.store.history(id).await?;

    // Widened from "only when completed": "how much is this repair holding" is
    // useful before completion too, and this is the one page where the walk is
    // worth its cost.
    let staged_bytes = match &job.staging_dir {
        Some(dir) => runtime.deps.staging.usage(dir).await.ok(),
        None => None,
    };

    let file_views: Vec<FileView> = files
        .iter()
        .map(|file| {
            // The same resolver the write path uses, so the operator can only
            // ever be offered something matching itself discovered.
            let candidates =
                crate::web::jobs::ambiguous_candidates(&history, file.torrent_path.as_str());
            FileView::new(file, &candidates)
        })
        .collect();

    let response = DetailResponse {
        job: JobView::new(&job),
        actions: actions_for(&job, &files),
        files: file_views,
        history: history.iter().map(HistoryView::new).collect(),
        staged_bytes,
    };

    Ok(Json(serde_json::to_value(&response).map_err(|error| {
        tracing::error!(%error, "could not serialise a job");
        ApiError::Internal("Could not render this repair.")
    })?))
}

/// Which actions are legal, computed from the same guards the handlers enforce.
///
/// Every `why` is the message the action itself returns on refusal, so a
/// disabled control's tooltip and the 409 body an operator would get by forcing
/// it cannot say different things.
pub fn actions_for(job: &RepairJob, files: &[crate::repair::PlannedFile]) -> Actions {
    let parked = job.state == RepairState::AwaitingReview;
    let unresolved = files.iter().filter(|file| file.source.is_none()).count();

    Actions {
        retry: match (parked, job.review_from_state) {
            (true, Some(state)) => Action::resuming_at(state),
            (true, None) => Action::unavailable(
                "This job does not record which step it stopped at, so it cannot be \
                 retried. Start it over instead.",
            ),
            (false, _) => {
                Action::unavailable("Only a repair waiting for a decision can be retried.")
            }
        },

        // The fix for the failed-job dead end: `validate_transition` has always
        // permitted `Failed -> Discovered`, and the maud UI simply never
        // rendered the button because it only rendered the panel for parked
        // jobs.
        restart: if parked || job.state == RepairState::Failed {
            Action::available()
        } else {
            Action::unavailable("Only a parked or failed repair can be started over.")
        },

        abandon: if parked {
            Action::available()
        } else {
            Action::unavailable("Only a repair waiting for a decision can be abandoned.")
        },

        abandon_and_discard: if parked && job.staging_dir.is_some() {
            Action::available()
        } else if parked {
            Action::unavailable("This repair has nothing staged to discard.")
        } else {
            Action::unavailable("Only a repair waiting for a decision can be abandoned.")
        },

        // Double-guarded, exactly as `review::approve_resume` is: the one
        // genuinely dangerous action must not become available because a client
        // asked nicely.
        approve_resume: if parked && job.review_reason == Some(ReviewReason::AutoResumeDisabled) {
            Action::available()
        } else {
            Action::unavailable(
                "This repair is not held up only by the auto-resume policy, so there is \
                 nothing to approve.",
            )
        },

        choose_candidate: if parked && unresolved > 0 {
            Action::with_unresolved(unresolved)
        } else if parked {
            Action::unavailable("Every file in this repair already has a library file chosen.")
        } else {
            Action::unavailable("Only a repair waiting for a decision needs a file chosen.")
        },

        // New in 0021. Completed only: the tracker has cleared the hit-and-run,
        // so the staged copy has done its job. Discarding it stops that torrent
        // seeding, which the confirmation says out loud.
        discard_staging: if job.state == RepairState::Completed && job.staging_dir.is_some() {
            Action::available()
        } else if job.state == RepairState::Completed {
            Action::unavailable("This repair has nothing staged to discard.")
        } else {
            Action::unavailable(
                "Staged files can only be discarded once the tracker has cleared the \
                 hit-and-run.",
            )
        },
    }
}

/// A `StoreError` that means "no such job" rather than a real failure.
pub fn missing(error: &StoreError) -> bool {
    matches!(error, StoreError::Missing(_))
}

/// Load a job or 404. Shared by every action handler.
pub async fn load(runtime: &Runtime, id: JobId) -> Result<RepairJob, ApiError> {
    runtime
        .deps
        .store
        .job(id)
        .await?
        .ok_or(ApiError::NotFound("No such repair job."))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Through the real pair-list decoder, so these tests exercise the path a
    /// request actually takes.
    fn query(states: &[&str], reason: &[&str], extra: Extra) -> ListQuery {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for state in states {
            pairs.push(("state".to_owned(), (*state).to_owned()));
        }
        for value in reason {
            pairs.push(("reason".to_owned(), (*value).to_owned()));
        }
        if let Some(sort) = extra.sort {
            pairs.push(("sort".to_owned(), sort.to_owned()));
        }
        if let Some(order) = extra.order {
            pairs.push(("order".to_owned(), order.to_owned()));
        }
        if let Some(limit) = extra.limit {
            pairs.push(("limit".to_owned(), limit.to_string()));
        }
        ListQuery::from_pairs(pairs)
    }

    #[derive(Default)]
    struct Extra {
        sort: Option<&'static str>,
        order: Option<&'static str>,
        limit: Option<i64>,
    }

    /// The regression that motivated the pair list. Two values for one key must
    /// decode as two values, not fail the request with an empty body.
    #[test]
    fn a_repeated_filter_key_decodes_as_a_list() {
        let query = ListQuery::from_pairs(vec![
            ("state".to_owned(), "seeding".to_owned()),
            ("state".to_owned(), "completed".to_owned()),
        ]);
        assert_eq!(query.state, vec!["seeding", "completed"]);
    }

    #[test]
    fn an_unknown_query_key_is_ignored_so_a_stale_bookmark_still_works() {
        let query = ListQuery::from_pairs(vec![
            ("nonsense".to_owned(), "1".to_owned()),
            ("state".to_owned(), "failed".to_owned()),
        ]);
        assert_eq!(query.state, vec!["failed"]);
        assert!(query.to_filter().is_ok());
    }

    #[test]
    fn an_empty_value_is_treated_as_absent() {
        // A form or a URL builder that emits `?q=&state=` should mean "no
        // filter", not "match the empty string".
        let query = ListQuery::from_pairs(vec![
            ("q".to_owned(), String::new()),
            ("state".to_owned(), String::new()),
        ]);
        assert!(query.q.is_none());
        assert!(query.state.is_empty());
    }

    #[test]
    fn an_unknown_state_filter_is_rejected_rather_than_ignored() {
        // Ignoring it would answer a different question and read as "no such
        // jobs" — an empty list the operator would believe.
        let error = query(&["nonsense"], &[], Extra::default())
            .to_filter()
            .expect_err("rejected");
        assert!(matches!(error, ApiError::UnknownField(_)), "{error:?}");

        let filter = query(&["awaiting_review", "failed"], &[], Extra::default())
            .to_filter()
            .expect("both are real states");
        assert_eq!(
            filter.states,
            vec![RepairState::AwaitingReview, RepairState::Failed]
        );
    }

    #[test]
    fn an_unknown_sort_or_order_is_rejected() {
        let sorted_by = |sort| {
            query(
                &[],
                &[],
                Extra {
                    sort: Some(sort),
                    ..Extra::default()
                },
            )
            .to_filter()
        };
        assert!(sorted_by("deadline").is_err(), "an unasked-for sort column");
        assert!(sorted_by("created_at").is_ok());
        assert!(
            query(
                &[],
                &[],
                Extra {
                    order: Some("sideways"),
                    ..Extra::default()
                }
            )
            .to_filter()
            .is_err()
        );
    }

    #[test]
    fn the_limit_is_clamped_rather_than_refused() {
        let with_limit = |limit| {
            query(
                &[],
                &[],
                Extra {
                    limit,
                    ..Extra::default()
                },
            )
            .to_filter()
            .expect("a limit is never itself a rejection")
            .limit
        };
        assert_eq!(with_limit(Some(100_000)), 200);
        assert_eq!(with_limit(Some(0)), 1);
        assert_eq!(with_limit(None), 50);
    }

    #[test]
    fn a_cursor_round_trips() {
        let job = RepairJob {
            id: JobId(42),
            tracker: TrackerId::new("demo"),
            torrent_id: crate::tracker::TrackerTorrentId::new("t"),
            torrent_name: "n".to_owned(),
            state: RepairState::Seeding,
            review_from_state: None,
            review_reason: None,
            failure_reason: None,
            info_hash: None,
            total_bytes: None,
            staging_dir: None,
            materialization: None,
            deadline: None,
            uploaded_bytes: None,
            seeding_seconds: None,
            rechecking_started_at: None,
            consecutive_unknown_tracker_status: 0,
            resume_approved: false,
            attempts: 0,
            next_attempt_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };

        let encoded = encode_cursor(&job, JobSort::UpdatedAt);
        let decoded = decode_cursor(&encoded).expect("round trips");
        assert_eq!(decoded.id, JobId(42));
        assert_eq!(
            decoded.sort_value,
            job.updated_at
                .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
        );
    }

    #[test]
    fn a_malformed_cursor_is_ignored_rather_than_failing_the_request() {
        // A truncated or hand-edited cursor should show page one, not an error:
        // the worst case is the operator seeing the top of the list again.
        assert!(decode_cursor("nonsense").is_none());
        assert!(decode_cursor("2026-01-01T00:00:00Z|notanumber").is_none());
    }
}
