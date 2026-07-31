//! Durable repair state in SQLite.
//!
//! The one thing to understand before changing this file: [`apply`] is a
//! compare-and-swap. It updates the job only if it is still in the state the
//! transition came from, and it writes the audit row in the same database
//! transaction. That is what lets every step be replayed after a crash.
//!
//! [`apply`]: RepairStore::apply

use std::{path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool, sqlite::SqliteRow};

use crate::{
    clock::Clock,
    library::{MatchConfidence, MatchEvidence},
    repair::{
        domain::{
            JobId, RepairJob, RepairState, ReviewReason, Transition, TransitionReason,
            TransitionRecord,
        },
        ports::{
            Applied, Discovered, JobCounts, JobFilter, JobPatch, JobSort, PlannedFile, RepairStore,
            StoreError, TransitionUpdate,
        },
    },
    staging::MaterializationStrategy,
    torrent::{InfoHash, SafeRelativePath},
    tracker::{HitAndRun, TrackerId, TrackerTorrentId},
};

/// Every column of `repair_jobs` except `torrent_file` and the lease bookkeeping.
///
/// A macro rather than a `const` because sqlx 0.9 only accepts literal SQL, and
/// rightly so. Listing the columns instead of `SELECT *` keeps the `.torrent`
/// blob out of every list query.
macro_rules! job_columns {
    () => {
        "id, tracker_id, tracker_torrent_id, torrent_name, state, review_from_state, \
         review_reason, failure_reason, info_hash, total_bytes, staging_dir, materialization, \
         rechecking_started_at, consecutive_unknown_tracker_status, deadline, uploaded_bytes, \
         seeding_seconds, resume_approved, attempts, next_attempt_at, created_at, updated_at"
    };
}

/// The states the worker acts on. Kept in step with [`RepairState`] by
/// `tests::the_actionable_state_list_matches_the_lifecycle`.
macro_rules! actionable_states {
    () => {
        "'discovered', 'torrent_fetched', 'matched', 'staged', 'injected', 'rechecking', \
         'verified', 'seeding'"
    };
}

/// The states in which a job's `total_bytes` really is staged on disk.
///
/// Everything from `staged` onwards, minus the two states a discard leaves a job
/// in: `failed` (abandon-and-discard) and `discovered` (start over, which
/// re-stages from scratch). Neither discard records itself on the job, so
/// without this list `staged_bytes_declared` would keep counting bytes that were
/// deleted. Kept in step with [`RepairState`] by
/// `tests::the_staged_state_list_matches_the_lifecycle`.
macro_rules! staged_states {
    () => {
        "'staged', 'injected', 'rechecking', 'verified', 'seeding', 'completed'"
    };
}

/// The `WHERE` clause of both job-list queries, as one literal shared by them
/// so a filter can never mean one thing to the page and another to its count.
///
/// Every clause is `?n IS NULL OR <predicate>`, so an absent filter costs
/// nothing and the SQL stays a literal with fixed arity — `migrations/AGENTS.md`
/// requires that anything assembled be a `macro_rules!` producing a literal, and
/// a dynamic `state IN (?, ?, ?)` cannot be one. `json_each` is what makes a
/// variable-length list fit that rule: JSON1 is always compiled in, and the list
/// arrives as a single bound parameter.
macro_rules! job_filter_predicates {
    () => {
        "(?1 IS NULL OR state IN (SELECT value FROM json_each(?1))) \
         AND (?2 IS NULL OR review_reason IN (SELECT value FROM json_each(?2))) \
         AND (?3 IS NULL OR tracker_id IN (SELECT value FROM json_each(?3))) \
         AND (?4 IS NULL OR torrent_name LIKE ?4 ESCAPE '\\') \
         AND (?5 IS NULL OR info_hash = ?5)"
    };
}

/// One page of jobs, for one sort column and direction.
///
/// The keyset predicate and the `ORDER BY` are written together, in one place,
/// per combination — because if they ever disagree a page boundary silently
/// drops or repeats a row, which is the kind of bug that shows up as "a job
/// vanished" months later. `(a, b) < (c, d)` is SQLite's row-value comparison
/// (3.15+; the bundled build is far newer), which expresses a keyset in one
/// predicate instead of `col < ? OR (col = ? AND id < ?)`.
macro_rules! page_sql {
    ($col:literal, $cmp:literal, $dir:literal) => {
        concat!(
            "SELECT ",
            job_columns!(),
            " FROM repair_jobs WHERE ",
            job_filter_predicates!(),
            " AND (?6 IS NULL OR (",
            $col,
            ", id) ",
            $cmp,
            " (?6, ?7)) ORDER BY ",
            $col,
            " ",
            $dir,
            ", id ",
            $dir,
            " LIMIT ?8"
        )
    };
}

/// Four arms, not eight: `JobSort` has two variants by design — see its doc
/// comment for why `deadline` and `attempts` are not there.
macro_rules! page_of_jobs {
    (updated_at, desc) => {
        page_sql!("updated_at", "<", "DESC")
    };
    (updated_at, asc) => {
        page_sql!("updated_at", ">", "ASC")
    };
    (created_at, desc) => {
        page_sql!("created_at", "<", "DESC")
    };
    (created_at, asc) => {
        page_sql!("created_at", ">", "ASC")
    };
}

pub struct SqliteRepairStore {
    pool: SqlitePool,
    clock: Arc<dyn Clock>,
}

impl SqliteRepairStore {
    pub fn new(pool: SqlitePool, clock: Arc<dyn Clock>) -> Self {
        Self { pool, clock }
    }
}

#[async_trait]
impl RepairStore for SqliteRepairStore {
    async fn record_discovery(&self, hit_and_run: &HitAndRun) -> Result<Discovered, StoreError> {
        let now = timestamp(self.clock.now());
        let mut tx = self.pool.begin().await.map_err(database)?;

        // The unique index on (tracker, torrent) is what makes rediscovery a
        // no-op; nothing here decides whether the job is new.
        let inserted = sqlx::query(
            "INSERT INTO repair_jobs \
             (tracker_id, tracker_torrent_id, torrent_name, state, info_hash, total_bytes, \
              deadline, created_at, updated_at) \
             VALUES (?, ?, ?, 'discovered', ?, ?, ?, ?, ?) \
             ON CONFLICT (tracker_id, tracker_torrent_id) DO NOTHING \
             RETURNING id",
        )
        .bind(hit_and_run.tracker.as_str())
        .bind(hit_and_run.torrent_id.as_str())
        .bind(&hit_and_run.torrent_name)
        .bind(hit_and_run.info_hash.map(InfoHash::to_hex))
        .bind(as_i64(hit_and_run.size_bytes))
        .bind(hit_and_run.deadline.map(timestamp))
        .bind(&now)
        .bind(&now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(database)?;

        let created = inserted.is_some();
        let id = match inserted {
            Some(row) => {
                let id = JobId(row.try_get("id").map_err(database)?);
                // Open the audit trail with the observation that started it.
                sqlx::query(
                    "INSERT INTO repair_job_transitions \
                     (job_id, from_state, to_state, reason, detail, occurred_at) \
                     VALUES (?, 'discovered', 'discovered', 'discovered', ?, ?)",
                )
                .bind(id.0)
                .bind(
                    serde_json::json!({
                        "tracker": hit_and_run.tracker.as_str(),
                        "torrent_id": hit_and_run.torrent_id.as_str(),
                        "size_bytes": hit_and_run.size_bytes,
                        "deadline": hit_and_run.deadline,
                    })
                    .to_string(),
                )
                .bind(&now)
                .execute(&mut *tx)
                .await
                .map_err(database)?;
                id
            }
            None => {
                let row = sqlx::query(
                    "SELECT id FROM repair_jobs WHERE tracker_id = ? AND tracker_torrent_id = ?",
                )
                .bind(hit_and_run.tracker.as_str())
                .bind(hit_and_run.torrent_id.as_str())
                .fetch_one(&mut *tx)
                .await
                .map_err(database)?;
                JobId(row.try_get("id").map_err(database)?)
            }
        };

        tx.commit().await.map_err(database)?;
        Ok(Discovered { id, created })
    }

    async fn job(&self, id: JobId) -> Result<Option<RepairJob>, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            job_columns!(),
            " FROM repair_jobs WHERE id = ?"
        ))
        .bind(id.0)
        .fetch_optional(&self.pool)
        .await
        .map_err(database)?;

        row.map(read_job).transpose()
    }

    async fn jobs(&self, limit: i64) -> Result<Vec<RepairJob>, StoreError> {
        sqlx::query(concat!(
            "SELECT ",
            job_columns!(),
            " FROM repair_jobs ORDER BY updated_at DESC, id DESC LIMIT ?"
        ))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(database)?
        .into_iter()
        .map(read_job)
        .collect()
    }

    async fn find_jobs(&self, filter: &JobFilter) -> Result<Vec<RepairJob>, StoreError> {
        let sql = match (filter.sort, filter.descending) {
            (JobSort::UpdatedAt, true) => page_of_jobs!(updated_at, desc),
            (JobSort::UpdatedAt, false) => page_of_jobs!(updated_at, asc),
            (JobSort::CreatedAt, true) => page_of_jobs!(created_at, desc),
            (JobSort::CreatedAt, false) => page_of_jobs!(created_at, asc),
        };

        bind_filter(sqlx::query(sql), filter)
            .bind(filter.after.as_ref().map(|after| after.sort_value.clone()))
            .bind(filter.after.as_ref().map(|after| after.id.0))
            .bind(filter.limit)
            .fetch_all(&self.pool)
            .await
            .map_err(database)?
            .into_iter()
            .map(read_job)
            .collect()
    }

    async fn count_jobs(&self, filter: &JobFilter) -> Result<i64, StoreError> {
        // `?6`/`?7` (the cursor) and `?8` (the limit) are deliberately absent:
        // a count is of everything the filter matches, not of one page. The
        // shared predicate macro is what keeps the two in agreement.
        let row = bind_filter(
            sqlx::query(concat!(
                "SELECT COUNT(*) AS n FROM repair_jobs WHERE ",
                job_filter_predicates!()
            )),
            filter,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(database)?;

        row.try_get("n").map_err(database)
    }

    async fn counts(&self) -> Result<JobCounts, StoreError> {
        let by_state = sqlx::query("SELECT state, COUNT(*) AS n FROM repair_jobs GROUP BY state")
            .fetch_all(&self.pool)
            .await
            .map_err(database)?;

        let mut counts = JobCounts::default();
        for row in by_state {
            let text: String = row.try_get("state").map_err(database)?;
            let n: i64 = row.try_get("n").map_err(database)?;
            // A state the CHECK constraint allows but this build's enum does not
            // is a downgrade, not a corrupt row: report it rather than failing
            // the whole dashboard. The counts stop adding up to `total`, which
            // is the honest outcome.
            let Ok(state) = RepairState::parse(&text) else {
                tracing::warn!(state = %text, "unrecognised repair state in counts");
                continue;
            };
            counts.total += n;
            counts.by_state.push((state, n));
        }
        counts.by_state.sort_by_key(|(state, _)| state.as_str());

        let by_reason = sqlx::query(
            "SELECT review_reason, COUNT(*) AS n FROM repair_jobs \
             WHERE state = 'awaiting_review' GROUP BY review_reason",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(database)?;

        for row in by_reason {
            let text: Option<String> = row.try_get("review_reason").map_err(database)?;
            let n: i64 = row.try_get("n").map_err(database)?;
            counts
                .by_review_reason
                .push((text.as_deref().and_then(ReviewReason::parse), n));
        }
        // Biggest group first: twenty repairs blocked on one thing is one
        // problem, and should read as one — the ordering `/` already applies.
        counts.by_review_reason.sort_by_key(|(_, n)| -n);

        Ok(counts)
    }

    async fn staged_bytes_declared(&self) -> Result<u64, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT COALESCE(SUM(total_bytes), 0) AS n FROM repair_jobs \
             WHERE staging_dir IS NOT NULL AND state IN (",
            staged_states!(),
            ")"
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(database)?;

        let bytes: i64 = row.try_get("n").map_err(database)?;
        Ok(bytes.max(0).unsigned_abs())
    }

    async fn rewind_counts(&self, at_least: i64) -> Result<Vec<(JobId, i64)>, StoreError> {
        sqlx::query(
            "SELECT job_id, COUNT(*) AS n FROM repair_job_transitions \
             WHERE reason = 'reconciliation' GROUP BY job_id HAVING COUNT(*) >= ?",
        )
        .bind(at_least)
        .fetch_all(&self.pool)
        .await
        .map_err(database)?
        .into_iter()
        .map(|row| {
            Ok((
                JobId(row.try_get("job_id").map_err(database)?),
                row.try_get("n").map_err(database)?,
            ))
        })
        .collect()
    }

    async fn unfinished_by_tracker(&self) -> Result<Vec<(TrackerId, i64)>, StoreError> {
        // Deliberately the same state list as `unfinished`, because the number
        // shown before removing a tracker has to be the number
        // `runtime::check_refusals` will refuse over. A different list here
        // would let the UI say "0 repairs affected" about a save that is then
        // refused.
        sqlx::query(concat!(
            "SELECT tracker_id, COUNT(*) AS n FROM repair_jobs WHERE state IN (",
            actionable_states!(),
            ") GROUP BY tracker_id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(database)?
        .into_iter()
        .map(|row| {
            let id: String = row.try_get("tracker_id").map_err(database)?;
            Ok((TrackerId::new(id), row.try_get("n").map_err(database)?))
        })
        .collect()
    }

    async fn unfinished(&self) -> Result<Vec<RepairJob>, StoreError> {
        sqlx::query(concat!(
            "SELECT ",
            job_columns!(),
            " FROM repair_jobs WHERE state IN (",
            actionable_states!(),
            ") ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(database)?
        .into_iter()
        .map(read_job)
        .collect()
    }

    async fn parked(&self) -> Result<Vec<RepairJob>, StoreError> {
        sqlx::query(concat!(
            "SELECT ",
            job_columns!(),
            " FROM repair_jobs WHERE state = 'awaiting_review' ORDER BY id"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(database)?
        .into_iter()
        .map(read_job)
        .collect()
    }

    async fn torrent_file(&self, id: JobId) -> Result<Option<Vec<u8>>, StoreError> {
        let row = sqlx::query("SELECT torrent_file FROM repair_jobs WHERE id = ?")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(database)?
            .ok_or(StoreError::Missing(id))?;

        row.try_get::<Option<Vec<u8>>, _>("torrent_file")
            .map_err(database)
    }

    async fn planned_files(&self, id: JobId) -> Result<Vec<PlannedFile>, StoreError> {
        sqlx::query(
            "SELECT torrent_path, length_bytes, source_path, match_confidence, match_evidence, \
             materialized_as, recheck_progress FROM repair_job_files WHERE job_id = ? \
             ORDER BY torrent_path",
        )
        .bind(id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(database)?
        .into_iter()
        .map(|row| read_planned_file(id, &row))
        .collect()
    }

    async fn history(&self, id: JobId) -> Result<Vec<TransitionRecord>, StoreError> {
        sqlx::query(
            "SELECT from_state, to_state, reason, detail, occurred_at \
             FROM repair_job_transitions WHERE job_id = ? ORDER BY id",
        )
        .bind(id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(database)?
        .into_iter()
        .map(|row| read_transition(id, &row))
        .collect()
    }

    async fn apply(
        &self,
        id: JobId,
        transition: Transition,
        update: TransitionUpdate,
    ) -> Result<Applied, StoreError> {
        let now = timestamp(self.clock.now());
        let mut tx = self.pool.begin().await.map_err(database)?;

        let parking_for_review = transition.to() == RepairState::AwaitingReview;
        let review_from = parking_for_review.then(|| transition.from().as_str().to_owned());
        let review_reason = match transition.reason() {
            TransitionReason::Review(reason) => Some(reason.as_str()),
            _ => None,
        };

        // The compare-and-swap. `attempts` resets because a transition means
        // the previous step is finished, one way or another. An approval is
        // per parked episode: parking for review again clears it, so a
        // rewound job onto different data does not stay silently approved —
        // see `RepairJob::resume_approved`.
        let changed = sqlx::query(
            "UPDATE repair_jobs SET \
                state = ?, review_from_state = ?, review_reason = ?, failure_reason = ?, \
                resume_approved = CASE WHEN ? THEN 0 ELSE resume_approved END, \
                attempts = 0, next_attempt_at = NULL, updated_at = ? \
             WHERE id = ? AND state = ?",
        )
        .bind(transition.to().as_str())
        .bind(review_from)
        .bind(review_reason)
        .bind(update.failure_reason.as_deref())
        .bind(parking_for_review)
        .bind(&now)
        .bind(id.0)
        .bind(transition.from().as_str())
        .execute(&mut *tx)
        .await
        .map_err(database)?
        .rows_affected();

        if changed == 0 {
            let current = sqlx::query("SELECT state FROM repair_jobs WHERE id = ?")
                .bind(id.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(database)?
                .ok_or(StoreError::Missing(id))?;
            let actual = parse_state(id, current.try_get("state").map_err(database)?)?;

            // Already where we were trying to go: a replayed step. Not an
            // error, and deliberately no second audit row.
            if actual == transition.to() {
                return Ok(Applied::AlreadyInTargetState);
            }
            return Err(StoreError::Conflict {
                id,
                expected: transition.from(),
                actual,
            });
        }

        apply_patch(&mut tx, id, &update.patch, &now).await?;

        sqlx::query(
            "INSERT INTO repair_job_transitions \
             (job_id, from_state, to_state, reason, detail, occurred_at) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id.0)
        .bind(transition.from().as_str())
        .bind(transition.to().as_str())
        .bind(transition.reason().as_str())
        .bind(update.detail.as_ref().map(ToString::to_string))
        .bind(&now)
        .execute(&mut *tx)
        .await
        .map_err(database)?;

        tx.commit().await.map_err(database)?;
        Ok(Applied::Applied)
    }

    async fn set_review_resume_point(
        &self,
        id: JobId,
        state: RepairState,
    ) -> Result<(), StoreError> {
        let now = timestamp(self.clock.now());
        let mut tx = self.pool.begin().await.map_err(database)?;

        let row = sqlx::query("SELECT state, review_from_state FROM repair_jobs WHERE id = ?")
            .bind(id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(database)?
            .ok_or(StoreError::Missing(id))?;

        let current_state = parse_state(id, row.try_get("state").map_err(database)?)?;
        if current_state != RepairState::AwaitingReview {
            // An operator's retry already got here first; there is nothing
            // parked left to correct.
            return Ok(());
        }

        let previous = row
            .try_get::<Option<String>, _>("review_from_state")
            .map_err(database)?
            .ok_or_else(|| StoreError::Corrupt {
                id,
                reason: "awaiting_review job has no review_from_state".to_owned(),
            })?;

        if previous == state.as_str() {
            return Ok(());
        }

        sqlx::query("UPDATE repair_jobs SET review_from_state = ?, updated_at = ? WHERE id = ?")
            .bind(state.as_str())
            .bind(&now)
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(database)?;

        sqlx::query(
            "INSERT INTO repair_job_transitions \
             (job_id, from_state, to_state, reason, detail, occurred_at) VALUES (?, ?, ?, 'reconciliation', ?, ?)",
        )
        .bind(id.0)
        .bind(&previous)
        .bind(state.as_str())
        .bind(
            serde_json::json!({
                "note": "parked repair's resume point moved back to match reality"
            })
            .to_string(),
        )
        .bind(&now)
        .execute(&mut *tx)
        .await
        .map_err(database)?;

        tx.commit().await.map_err(database)?;
        Ok(())
    }

    async fn claim(
        &self,
        owner: &str,
        lease: Duration,
        limit: i64,
    ) -> Result<Vec<RepairJob>, StoreError> {
        let now = self.clock.now();
        let expires = now + chrono::Duration::from_std(lease).unwrap_or(chrono::Duration::zero());

        sqlx::query(concat!(
            "UPDATE repair_jobs SET lease_owner = ?, lease_expires_at = ? \
             WHERE id IN ( \
                SELECT id FROM repair_jobs \
                WHERE state IN (",
            actionable_states!(),
            ") \
                  AND (next_attempt_at IS NULL OR next_attempt_at <= ?) \
                  AND (lease_expires_at IS NULL OR lease_expires_at <= ?) \
                ORDER BY next_attempt_at IS NOT NULL, next_attempt_at, id \
                LIMIT ? \
             ) \
             RETURNING ",
            job_columns!()
        ))
        .bind(owner)
        .bind(timestamp(expires))
        .bind(timestamp(now))
        .bind(timestamp(now))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(database)?
        .into_iter()
        .map(read_job)
        .collect()
    }

    async fn release(
        &self,
        id: JobId,
        retry_at: Option<DateTime<Utc>>,
        count_attempt: bool,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE repair_jobs SET \
                lease_owner = NULL, lease_expires_at = NULL, next_attempt_at = ?, \
                attempts = attempts + ?, updated_at = ? \
             WHERE id = ?",
        )
        .bind(retry_at.map(timestamp))
        .bind(i64::from(count_attempt))
        .bind(timestamp(self.clock.now()))
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(database)?;

        Ok(())
    }

    async fn renew_lease(
        &self,
        id: JobId,
        owner: &str,
        lease: Duration,
    ) -> Result<bool, StoreError> {
        let now = self.clock.now();
        let expires = now + chrono::Duration::from_std(lease).unwrap_or(chrono::Duration::zero());

        let renewed = sqlx::query(
            "UPDATE repair_jobs SET lease_expires_at = ? WHERE id = ? AND lease_owner = ?",
        )
        .bind(timestamp(expires))
        .bind(id.0)
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(database)?
        .rows_affected();

        Ok(renewed > 0)
    }

    async fn record_progress(&self, id: JobId, patch: JobPatch) -> Result<(), StoreError> {
        let now = timestamp(self.clock.now());
        update_job_fields(&self.pool, id, &patch, &now).await
    }

    async fn clear_stale_leases(&self, owner: &str) -> Result<u64, StoreError> {
        let cleared = sqlx::query(
            "UPDATE repair_jobs SET lease_owner = NULL, lease_expires_at = NULL \
             WHERE lease_expires_at IS NOT NULL AND (lease_expires_at <= ? OR lease_owner = ?)",
        )
        .bind(timestamp(self.clock.now()))
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(database)?
        .rows_affected();

        Ok(cleared)
    }

    async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(database)?;
        Ok(())
    }

    async fn has_active_lease(&self) -> Result<bool, StoreError> {
        let row = sqlx::query(
            "SELECT 1 FROM repair_jobs WHERE lease_expires_at IS NOT NULL AND \
             lease_expires_at > ? LIMIT 1",
        )
        .bind(timestamp(self.clock.now()))
        .fetch_optional(&self.pool)
        .await
        .map_err(database)?;

        Ok(row.is_some())
    }
}

/// The scalar `repair_jobs` columns a [`JobPatch`] may update, shared between
/// [`apply_patch`] (inside the transition's transaction) and
/// [`SqliteRepairStore::record_progress`] (its own statement, no transition).
///
/// `COALESCE(?, column)` is exactly the semantics of `Option`: a `None` bind
/// leaves the column alone. One literal statement instead of a dynamic SET.
async fn update_job_fields<'e, E>(
    executor: E,
    id: JobId,
    patch: &JobPatch,
    now: &str,
) -> Result<(), StoreError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "UPDATE repair_jobs SET \
            info_hash = COALESCE(?, info_hash), \
            torrent_file = COALESCE(?, torrent_file), \
            total_bytes = COALESCE(?, total_bytes), \
            staging_dir = COALESCE(?, staging_dir), \
            materialization = COALESCE(?, materialization), \
            rechecking_started_at = COALESCE(?, rechecking_started_at), \
            consecutive_unknown_tracker_status = COALESCE(?, consecutive_unknown_tracker_status), \
            uploaded_bytes = COALESCE(?, uploaded_bytes), \
            seeding_seconds = COALESCE(?, seeding_seconds), \
            resume_approved = COALESCE(?, resume_approved), \
            updated_at = ? \
         WHERE id = ?",
    )
    .bind(patch.info_hash.map(InfoHash::to_hex))
    .bind(patch.torrent_file.clone())
    .bind(patch.total_bytes.map(as_i64))
    .bind(patch.staging_dir.as_ref().map(ToString::to_string))
    .bind(patch.materialization.map(MaterializationStrategy::as_str))
    .bind(patch.rechecking_started_at.map(timestamp))
    .bind(patch.consecutive_unknown_tracker_status.map(i64::from))
    .bind(patch.uploaded_bytes.map(as_i64))
    .bind(patch.seeding_seconds.map(as_i64))
    .bind(patch.resume_approved.map(i64::from))
    .bind(now)
    .bind(id.0)
    .execute(executor)
    .await
    .map_err(database)?;
    Ok(())
}

async fn apply_patch(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: JobId,
    patch: &JobPatch,
    now: &str,
) -> Result<(), StoreError> {
    update_job_fields(&mut **tx, id, patch, now).await?;

    if let Some(files) = &patch.files {
        sqlx::query("DELETE FROM repair_job_files WHERE job_id = ?")
            .bind(id.0)
            .execute(&mut **tx)
            .await
            .map_err(database)?;

        for file in files {
            sqlx::query(
                "INSERT INTO repair_job_files \
                 (job_id, torrent_path, length_bytes, source_path, match_confidence, \
                  match_evidence, materialized_as) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id.0)
            .bind(file.torrent_path.as_str())
            .bind(as_i64(file.length))
            .bind(
                file.source
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned()),
            )
            .bind(file.confidence.map(confidence_str))
            .bind(
                file.evidence
                    .as_ref()
                    .map(|evidence| serde_json::to_string(evidence).unwrap_or_default()),
            )
            .bind(file.materialized_as.map(MaterializationStrategy::as_str))
            .execute(&mut **tx)
            .await
            .map_err(database)?;
        }
    }

    if let Some(progress) = &patch.file_progress {
        for entry in progress {
            sqlx::query(
                "UPDATE repair_job_files SET recheck_progress = ? \
                 WHERE job_id = ? AND torrent_path = ?",
            )
            .bind(entry.ratio)
            .bind(id.0)
            .bind(entry.torrent_path.as_str())
            .execute(&mut **tx)
            .await
            .map_err(database)?;
        }
    }

    Ok(())
}

fn read_job(row: SqliteRow) -> Result<RepairJob, StoreError> {
    let id = JobId(row.try_get("id").map_err(database)?);
    let corrupt = |reason: String| StoreError::Corrupt { id, reason };

    let staging_dir = row
        .try_get::<Option<String>, _>("staging_dir")
        .map_err(database)?
        .map(|raw| SafeRelativePath::parse(&raw))
        .transpose()
        .map_err(|error| corrupt(format!("staging_dir: {error}")))?;

    let info_hash = row
        .try_get::<Option<String>, _>("info_hash")
        .map_err(database)?
        .map(|raw| InfoHash::parse_hex(&raw))
        .transpose()
        .map_err(|error| corrupt(format!("info_hash: {error}")))?;

    let materialization = row
        .try_get::<Option<String>, _>("materialization")
        .map_err(database)?
        .map(|raw| raw.parse::<MaterializationStrategy>())
        .transpose()
        .map_err(corrupt)?;

    let review_from_state = row
        .try_get::<Option<String>, _>("review_from_state")
        .map_err(database)?
        .map(|raw| RepairState::parse(&raw))
        .transpose()
        .map_err(|error| corrupt(error.to_string()))?;

    let rechecking_started_at = row
        .try_get::<Option<String>, _>("rechecking_started_at")
        .map_err(database)?
        .map(|raw| {
            parse_time(&raw).ok_or_else(|| {
                corrupt(format!(
                    "rechecking_started_at: `{raw}` is not an RFC 3339 timestamp"
                ))
            })
        })
        .transpose()?;

    let deadline = row
        .try_get::<Option<String>, _>("deadline")
        .map_err(database)?
        .map(|raw| {
            parse_time(&raw)
                .ok_or_else(|| corrupt(format!("deadline: `{raw}` is not an RFC 3339 timestamp")))
        })
        .transpose()?;

    Ok(RepairJob {
        id,
        tracker: TrackerId::new(row.try_get::<String, _>("tracker_id").map_err(database)?),
        torrent_id: TrackerTorrentId::new(
            row.try_get::<String, _>("tracker_torrent_id")
                .map_err(database)?,
        ),
        torrent_name: row.try_get("torrent_name").map_err(database)?,
        state: parse_state(id, row.try_get("state").map_err(database)?)?,
        review_from_state,
        review_reason: row
            .try_get::<Option<String>, _>("review_reason")
            .map_err(database)?
            .and_then(|raw| ReviewReason::parse(&raw)),
        failure_reason: row.try_get("failure_reason").map_err(database)?,
        info_hash,
        total_bytes: row
            .try_get::<Option<i64>, _>("total_bytes")
            .map_err(database)?
            .map(as_u64),
        staging_dir,
        materialization,
        deadline,
        uploaded_bytes: row
            .try_get::<Option<i64>, _>("uploaded_bytes")
            .map_err(database)?
            .map(as_u64),
        seeding_seconds: row
            .try_get::<Option<i64>, _>("seeding_seconds")
            .map_err(database)?
            .map(as_u64),
        resume_approved: row.try_get::<i64, _>("resume_approved").map_err(database)? != 0,
        rechecking_started_at,
        consecutive_unknown_tracker_status: row
            .try_get::<i64, _>("consecutive_unknown_tracker_status")
            .map_err(database)?
            .try_into()
            .unwrap_or(u32::MAX),
        attempts: row
            .try_get::<i64, _>("attempts")
            .map_err(database)?
            .try_into()
            .unwrap_or(u32::MAX),
        next_attempt_at: read_optional_time(&row, "next_attempt_at", id)?,
        created_at: read_time(&row, "created_at", id)?,
        updated_at: read_time(&row, "updated_at", id)?,
    })
}

fn read_planned_file(id: JobId, row: &SqliteRow) -> Result<PlannedFile, StoreError> {
    let corrupt = |reason: String| StoreError::Corrupt { id, reason };

    let torrent_path =
        SafeRelativePath::parse(&row.try_get::<String, _>("torrent_path").map_err(database)?)
            .map_err(|error| corrupt(format!("torrent_path: {error}")))?;

    let materialized_as = row
        .try_get::<Option<String>, _>("materialized_as")
        .map_err(database)?
        .map(|raw| raw.parse::<MaterializationStrategy>())
        .transpose()
        .map_err(corrupt)?;

    Ok(PlannedFile {
        torrent_path,
        length: as_u64(row.try_get::<i64, _>("length_bytes").map_err(database)?),
        source: row
            .try_get::<Option<String>, _>("source_path")
            .map_err(database)?
            .map(PathBuf::from),
        confidence: row
            .try_get::<Option<String>, _>("match_confidence")
            .map_err(database)?
            .and_then(|raw| parse_confidence(&raw)),
        evidence: row
            .try_get::<Option<String>, _>("match_evidence")
            .map_err(database)?
            .and_then(|raw| serde_json::from_str::<MatchEvidence>(&raw).ok()),
        materialized_as,
        recheck_progress: row.try_get("recheck_progress").map_err(database)?,
    })
}

fn read_transition(id: JobId, row: &SqliteRow) -> Result<TransitionRecord, StoreError> {
    Ok(TransitionRecord {
        from: parse_state(id, row.try_get("from_state").map_err(database)?)?,
        to: parse_state(id, row.try_get("to_state").map_err(database)?)?,
        reason: row.try_get("reason").map_err(database)?,
        detail: row
            .try_get::<Option<String>, _>("detail")
            .map_err(database)?
            .and_then(|raw| serde_json::from_str(&raw).ok()),
        occurred_at: read_time(row, "occurred_at", id)?,
    })
}

fn read_time(row: &SqliteRow, column: &str, id: JobId) -> Result<DateTime<Utc>, StoreError> {
    let raw: String = row.try_get(column).map_err(database)?;
    parse_time(&raw).ok_or_else(|| StoreError::Corrupt {
        id,
        reason: format!("{column}: `{raw}` is not an RFC 3339 timestamp"),
    })
}

fn read_optional_time(
    row: &SqliteRow,
    column: &str,
    id: JobId,
) -> Result<Option<DateTime<Utc>>, StoreError> {
    let Some(raw) = row.try_get::<Option<String>, _>(column).map_err(database)? else {
        return Ok(None);
    };
    parse_time(&raw)
        .map(Some)
        .ok_or_else(|| StoreError::Corrupt {
            id,
            reason: format!("{column}: `{raw}` is not an RFC 3339 timestamp"),
        })
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn parse_state(id: JobId, raw: String) -> Result<RepairState, StoreError> {
    RepairState::parse(&raw).map_err(|error| StoreError::Corrupt {
        id,
        reason: error.to_string(),
    })
}

fn confidence_str(confidence: MatchConfidence) -> &'static str {
    match confidence {
        MatchConfidence::Exact => "exact",
        MatchConfidence::Operator => "operator",
        MatchConfidence::Probable => "probable",
        MatchConfidence::Ambiguous => "ambiguous",
    }
}

fn parse_confidence(raw: &str) -> Option<MatchConfidence> {
    match raw {
        "exact" => Some(MatchConfidence::Exact),
        "operator" => Some(MatchConfidence::Operator),
        "probable" => Some(MatchConfidence::Probable),
        "ambiguous" => Some(MatchConfidence::Ambiguous),
        _ => None,
    }
}

/// Bind `?1`–`?5` of [`job_filter_predicates`], in that order.
///
/// One function for both the page query and its count, so the two can never
/// disagree about what a filter meant. An empty list binds `NULL`, which the
/// predicate reads as "no filter" — that is why the list is `Option<String>`
/// rather than an empty JSON array.
fn bind_filter<'q>(
    query: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>,
    filter: &JobFilter,
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments> {
    let (name, hash) = split_search(filter.search.as_deref());
    query
        .bind(json_list(filter.states.iter().map(|state| state.as_str())))
        .bind(json_list(
            filter.review_reasons.iter().map(|reason| reason.as_str()),
        ))
        .bind(json_list(
            filter
                .trackers
                .iter()
                .map(crate::tracker::TrackerId::as_str),
        ))
        .bind(name)
        .bind(hash)
}

/// A JSON array of strings for `json_each`, or `None` when the list is empty.
fn json_list<'a>(values: impl Iterator<Item = &'a str>) -> Option<String> {
    let values: Vec<&str> = values.collect();
    if values.is_empty() {
        return None;
    }
    // Infallible: a Vec<&str> always serialises.
    Some(serde_json::to_string(&values).unwrap_or_else(|_| "[]".to_owned()))
}

/// Split a search term into the `LIKE` pattern and the exact info-hash it might
/// be instead.
///
/// A 40-character hex string is an info-hash, and matching it as a substring of
/// `torrent_name` would find nothing — so it binds the other parameter. Exactly
/// one of the two is ever `Some`.
fn split_search(search: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(term) = search.map(str::trim).filter(|term| !term.is_empty()) else {
        return (None, None);
    };

    if term.len() == 40 && term.chars().all(|c| c.is_ascii_hexdigit()) {
        return (None, Some(term.to_ascii_lowercase()));
    }
    (Some(format!("%{}%", escape_like(term))), None)
}

/// Escape the three characters `LIKE ... ESCAPE '\'` treats specially.
///
/// Without this, a search for `%` matches every job and a search for `_`
/// matches any single character — so the search box would quietly lie.
fn escape_like(term: &str) -> String {
    let mut escaped = String::with_capacity(term.len());
    for character in term.chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// SQLite has no unsigned integers. Sizes never approach the limit, and
/// saturating is better than wrapping into a negative row.
fn as_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn as_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn database(error: impl std::fmt::Display) -> StoreError {
    StoreError::Database(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SQL has to spell the actionable states out; this is what stops it
    /// drifting from the enum when a state is added.
    #[test]
    fn the_actionable_state_list_matches_the_lifecycle() {
        let from_enum = RepairState::PROGRESSION
            .iter()
            .filter(|state| state.is_actionable())
            .map(|state| format!("'{}'", state.as_str()))
            .collect::<Vec<_>>()
            .join(", ");

        let from_sql = actionable_states!()
            .split(',')
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(", ");

        assert_eq!(from_sql, from_enum);
    }

    /// Likewise for the column list, against every migration that has added a
    /// `repair_jobs` column since. Concatenated rather than re-derived from
    /// disk, so this test — like the migrations themselves — only grows.
    #[test]
    fn the_job_column_list_matches_the_schema() {
        let migrations = concat!(
            include_str!("../../../migrations/0001_initial.sql"),
            include_str!("../../../migrations/0003_recheck_started_at.sql"),
            include_str!("../../../migrations/0004_seeding_monitoring.sql"),
            include_str!("../../../migrations/0005_review_approval.sql"),
        );
        for column in job_columns!().split(',').map(str::trim) {
            assert!(
                migrations.contains(column),
                "`{column}` is selected but not in the schema"
            );
        }
    }

    /// `staged_states!()` spells out where a job's `total_bytes` really is on
    /// disk. A new lifecycle state between `staged` and `completed` belongs in
    /// it; a new one before `staged` does not — so this checks the boundary
    /// rather than the list, which is the part that can drift silently.
    #[test]
    fn the_staged_state_list_matches_the_lifecycle() {
        let listed: Vec<&str> = staged_states!()
            .split(',')
            .map(|state| state.trim().trim_matches('\''))
            .collect();

        let staged_rank = RepairState::Staged.rank().expect("staged is on the path");
        for state in RepairState::PROGRESSION {
            let expected = state.rank().is_some_and(|rank| rank >= staged_rank);
            assert_eq!(
                listed.contains(&state.as_str()),
                expected,
                "`{}` is at rank {:?} but staged_states!() {} it",
                state.as_str(),
                state.rank(),
                if expected { "omits" } else { "includes" }
            );
        }

        // The two states a discard leaves behind must never be in the list, or
        // the total keeps counting bytes that were deleted.
        assert!(!listed.contains(&RepairState::Failed.as_str()));
        assert!(!listed.contains(&RepairState::Discovered.as_str()));
    }

    /// `job_filter_predicates!()` is bound positionally by `bind_filter`, and a
    /// mismatch between the two is silent — the wrong list filters the wrong
    /// column. Pin the parameter order.
    #[test]
    fn the_filter_predicates_bind_in_the_order_bind_filter_uses() {
        // One clause per bound parameter, in `bind_filter`'s call order. A
        // mismatch is silent otherwise: the tracker list would filter the
        // torrent name, and every request would just come back empty.
        let expected = [
            ("?1", "state"),
            ("?2", "review_reason"),
            ("?3", "tracker_id"),
            ("?4", "torrent_name"),
            ("?5", "info_hash"),
        ];

        let clauses: Vec<&str> = job_filter_predicates!().split(" AND ").collect();
        assert_eq!(
            clauses.len(),
            expected.len(),
            "a clause was added or removed without updating bind_filter"
        );

        for (clause, (param, column)) in clauses.iter().zip(expected) {
            assert!(
                clause.contains(param) && clause.contains(column),
                "expected the clause binding {param} to filter on {column}, got: {clause}"
            );
        }
    }

    #[test]
    fn a_search_term_containing_a_wildcard_is_escaped() {
        let (pattern, hash) = split_search(Some("100%_real\\thing"));
        assert_eq!(hash, None);
        assert_eq!(pattern.as_deref(), Some(r"%100\%\_real\\thing%"));
    }

    #[test]
    fn a_forty_character_hex_term_is_an_info_hash_not_a_name_search() {
        let hex = "A".repeat(40);
        let (pattern, hash) = split_search(Some(&hex));
        assert_eq!(pattern, None);
        assert_eq!(hash.as_deref(), Some("a".repeat(40).as_str()));

        // One character short, or one non-hex character, is a name again.
        let (pattern, hash) = split_search(Some(&"A".repeat(39)));
        assert!(pattern.is_some());
        assert_eq!(hash, None);
        let (pattern, hash) = split_search(Some(&format!("{}z", "A".repeat(39))));
        assert!(pattern.is_some());
        assert_eq!(hash, None);
    }

    #[test]
    fn a_blank_search_term_is_no_filter_at_all() {
        assert_eq!(split_search(None), (None, None));
        assert_eq!(split_search(Some("   ")), (None, None));
    }

    #[test]
    fn an_empty_filter_list_binds_null_rather_than_an_empty_array() {
        // `[]` would make `state IN (SELECT ... json_each('[]'))` match nothing,
        // silently turning "no filter" into "no results".
        assert_eq!(json_list(std::iter::empty()), None);
        assert_eq!(
            json_list(["a", "b"].into_iter()).as_deref(),
            Some(r#"["a","b"]"#)
        );
    }

    // --- against a real database -------------------------------------------
    //
    // The macros above are literals, so nothing but SQLite can tell whether
    // they are *valid* SQL, whether `json_each` filters what it should, or
    // whether the keyset predicate agrees with its `ORDER BY`.

    use crate::{clock::TestClock, database, repair::ports::JobCursor, tracker::TrackerTorrentId};

    /// A store with one job per name, and the clock that stamped them — which
    /// the caller needs, because `updated_at` is the default sort column and a
    /// clock that never moves would make every row's sort key identical, quietly
    /// reducing the keyset test below to its id tiebreaker.
    async fn store_with(names: &[&str]) -> (SqliteRepairStore, Arc<TestClock>, Vec<JobId>) {
        let clock = Arc::new(TestClock::new(
            DateTime::parse_from_rfc3339("2026-07-30T12:00:00Z")
                .expect("valid")
                .with_timezone(&Utc),
        ));
        let store = SqliteRepairStore::new(database::test_pool().await, clock.clone());

        let mut ids = Vec::new();
        for (index, name) in names.iter().enumerate() {
            clock.advance(chrono::Duration::seconds(60));
            let discovered = store
                .record_discovery(&HitAndRun {
                    tracker: TrackerId::new(if index % 2 == 0 { "alpha" } else { "beta" }),
                    torrent_id: TrackerTorrentId::new(format!("t{index}")),
                    torrent_name: (*name).to_owned(),
                    info_hash: None,
                    size_bytes: 1_000 * (index as u64 + 1),
                    deadline: None,
                    observed_at: Utc::now(),
                })
                .await
                .expect("discovery");
            ids.push(discovered.id);
        }
        (store, clock, ids)
    }

    /// The aggregate must agree with the thing it replaced: `/status` used to
    /// load every job and fold in Rust. If they ever disagree, the dashboard is
    /// lying about numbers nobody can check by eye.
    #[tokio::test]
    async fn counts_agree_with_folding_over_every_job() {
        let (store, clock, ids) = store_with(&["one", "two", "three", "four"]).await;
        let _ = &clock;

        // Move two jobs off `discovered` so more than one state is populated.
        for id in ids.iter().take(2) {
            let job = store.job(*id).await.expect("read").expect("present");
            let advance = job.advance().expect("can advance");
            store
                .apply(*id, advance, TransitionUpdate::default())
                .await
                .expect("applied");
        }

        let counts = store.counts().await.expect("counts");

        let mut folded: std::collections::BTreeMap<&str, i64> = std::collections::BTreeMap::new();
        let all = store.jobs(i64::MAX).await.expect("jobs");
        for job in &all {
            *folded.entry(job.state.as_str()).or_default() += 1;
        }

        let from_sql: std::collections::BTreeMap<&str, i64> = counts
            .by_state
            .iter()
            .map(|(state, n)| (state.as_str(), *n))
            .collect();

        assert_eq!(from_sql, folded);
        assert_eq!(counts.total, all.len() as i64);
    }

    /// The test that earns keyset paging over `OFFSET`.
    ///
    /// Between the two pages a row is touched — which is exactly what the worker
    /// does every few seconds — moving it to the front of a `updated_at DESC`
    /// ordering. With `LIMIT/OFFSET` every row after it shifts by one, so page
    /// two re-serves a row page one already showed. A keyset cursor cannot do
    /// that: it asks for rows strictly past a specific `(updated_at, id)`, so a
    /// row it already passed can never come back.
    ///
    /// What it does *not* promise is that the moved row appears at all. No
    /// pagination scheme can promise that over a mutable sort key — the row is
    /// now on a page the client has already read past. That is a real and
    /// accepted consequence, and it is why the UI refetches on an event rather
    /// than trusting a cursor across mutations. Asserted here explicitly so
    /// nobody later "fixes" it by reaching for an offset, which trades this for
    /// something strictly worse.
    #[tokio::test]
    async fn a_keyset_page_never_repeats_a_row_while_rows_are_updated() {
        let (store, clock, ids) = store_with(&["a", "b", "c", "d", "e", "f"]).await;

        let filter = JobFilter {
            limit: 3,
            ..JobFilter::default()
        };
        let first = store.find_jobs(&filter).await.expect("first page");
        assert_eq!(first.len(), 3);

        let cursor = {
            let last = first.last().expect("non-empty");
            JobCursor {
                sort_value: timestamp(last.updated_at),
                id: last.id,
            }
        };

        // Touch a job from the *first* page, at a later time, so its
        // `updated_at` really does move to the front of the ordering — an
        // offset-based second page would now start one row late and miss one
        // entirely.
        clock.advance(chrono::Duration::seconds(3600));
        let touched = ids[0];
        let job = store.job(touched).await.expect("read").expect("present");
        let advance = job.advance().expect("can advance");
        store
            .apply(touched, advance, TransitionUpdate::default())
            .await
            .expect("applied");

        let second = store
            .find_jobs(&JobFilter {
                after: Some(cursor),
                ..filter
            })
            .await
            .expect("second page");

        let mut seen: Vec<i64> = first.iter().chain(&second).map(|job| job.id.0).collect();
        let before_dedup = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            before_dedup,
            "a row was served twice, which is the failure keyset paging exists \
             to prevent: got {seen:?}"
        );

        // The touched row is the only one absent, and only because it moved
        // ahead of the cursor. Everything else is still accounted for.
        let missing: Vec<i64> = ids
            .iter()
            .map(|id| id.0)
            .filter(|id| !seen.contains(id))
            .collect();
        assert_eq!(
            missing,
            vec![touched.0],
            "only the row whose sort key changed may be missing"
        );
    }

    /// Biggest group first.
    ///
    /// Twenty repairs blocked on one cause is *one* problem and has to read as
    /// one. This ordering used to live in `web::jobs::group_by_review_reason`,
    /// which the React UI replaced; the property moved here with it rather than
    /// being dropped.
    #[tokio::test]
    async fn review_reasons_are_counted_biggest_group_first() {
        let (store, _clock, ids) = store_with(&["a", "b", "c"]).await;

        // Two jobs parked on one reason, one on another.
        for (index, reason) in [
            (0, ReviewReason::AdapterNotImplemented),
            (1, ReviewReason::AdapterNotImplemented),
            (2, ReviewReason::AmbiguousMatch),
        ] {
            let job = store.job(ids[index]).await.expect("read").expect("present");
            let park = job
                .plan_transition(
                    RepairState::AwaitingReview,
                    TransitionReason::Review(reason),
                )
                .expect("can park");
            store
                .apply(job.id, park, TransitionUpdate::default())
                .await
                .expect("parked");
        }

        let counts = store.counts().await.expect("counts");

        assert_eq!(
            counts.by_review_reason,
            vec![
                (Some(ReviewReason::AdapterNotImplemented), 2),
                (Some(ReviewReason::AmbiguousMatch), 1),
            ]
        );
    }

    #[tokio::test]
    async fn filters_compose_as_and() {
        let (store, _clock, ids) = store_with(&["alpha one", "beta two", "alpha three"]).await;

        // ids[0] and ids[2] are tracker `alpha`; move ids[0] off `discovered`.
        let job = store.job(ids[0]).await.expect("read").expect("present");
        let advance = job.advance().expect("can advance");
        store
            .apply(ids[0], advance, TransitionUpdate::default())
            .await
            .expect("applied");

        let matched = store
            .find_jobs(&JobFilter {
                states: vec![RepairState::Discovered],
                trackers: vec![TrackerId::new("alpha")],
                ..JobFilter::default()
            })
            .await
            .expect("find");

        assert_eq!(
            matched.iter().map(|job| job.id).collect::<Vec<_>>(),
            vec![ids[2]],
            "only the alpha job still in `discovered` matches both filters"
        );
    }

    /// Without `ESCAPE`, searching for `%` returns every job — so the search box
    /// would quietly answer a different question than it was asked.
    #[tokio::test]
    async fn a_percent_sign_in_a_search_matches_literally() {
        let (store, _clock, _) = store_with(&["100% Real", "Ordinary Release"]).await;

        let matched = store
            .find_jobs(&JobFilter {
                search: Some("%".to_owned()),
                ..JobFilter::default()
            })
            .await
            .expect("find");

        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].torrent_name, "100% Real");
    }

    #[tokio::test]
    async fn count_jobs_agrees_with_the_page_it_accompanies() {
        let (store, _clock, _) = store_with(&["a", "b", "c", "d", "e"]).await;

        let filter = JobFilter {
            limit: 2,
            ..JobFilter::default()
        };
        let page = store.find_jobs(&filter).await.expect("page");
        let total = store.count_jobs(&filter).await.expect("count");

        assert_eq!(page.len(), 2, "the page is capped by `limit`");
        assert_eq!(total, 5, "the count deliberately ignores `limit`");
    }

    #[tokio::test]
    async fn rewind_counts_reports_only_jobs_at_or_past_the_threshold() {
        let (store, _clock, ids) = store_with(&["oscillating", "steady"]).await;

        for _ in 0..3 {
            let job = store.job(ids[0]).await.expect("read").expect("present");
            let advance = job.advance().expect("can advance");
            store
                .apply(ids[0], advance, TransitionUpdate::default())
                .await
                .expect("advance");
            let job = store.job(ids[0]).await.expect("read").expect("present");
            let rewind = job
                .plan_transition(RepairState::Discovered, TransitionReason::Reconciliation)
                .expect("can rewind");
            store
                .apply(ids[0], rewind, TransitionUpdate::default())
                .await
                .expect("rewind");
        }

        assert_eq!(
            store.rewind_counts(3).await.expect("counts"),
            vec![(ids[0], 3)]
        );
        assert!(store.rewind_counts(4).await.expect("counts").is_empty());
    }

    #[tokio::test]
    async fn unfinished_by_tracker_matches_unfinished() {
        let (store, _clock, _) = store_with(&["a", "b", "c"]).await;

        let by_tracker = store.unfinished_by_tracker().await.expect("by tracker");
        let unfinished = store.unfinished().await.expect("unfinished");

        let total: i64 = by_tracker.iter().map(|(_, n)| n).sum();
        assert_eq!(
            total,
            unfinished.len() as i64,
            "the grouped count must agree with the list `check_refusals` uses, \
             or the UI promises a save that the reload then refuses"
        );
        assert_eq!(by_tracker.len(), 2, "two distinct trackers were seeded");
    }

    #[tokio::test]
    async fn staged_bytes_counts_only_jobs_whose_data_is_really_staged() {
        let (store, _clock, ids) = store_with(&["one", "two"]).await;

        assert_eq!(
            store.staged_bytes_declared().await.expect("bytes"),
            0,
            "a `discovered` job has staged nothing"
        );

        // Walk one job to `staged`, giving it a staging directory on the way.
        let mut job = store.job(ids[0]).await.expect("read").expect("present");
        while job.state != RepairState::Staged {
            let advance = job.advance().expect("can advance");
            let update = if job.state == RepairState::Matched {
                TransitionUpdate::default().patch(JobPatch {
                    staging_dir: Some(job.default_staging_dir()),
                    ..JobPatch::default()
                })
            } else {
                TransitionUpdate::default()
            };
            store.apply(job.id, advance, update).await.expect("applied");
            job = store.job(job.id).await.expect("read").expect("present");
        }

        assert_eq!(
            store.staged_bytes_declared().await.expect("bytes"),
            1_000,
            "the first job's total_bytes, and only that job's"
        );
    }
}
