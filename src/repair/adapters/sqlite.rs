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
            Applied, Discovered, JobPatch, PlannedFile, RepairStore, StoreError, TransitionUpdate,
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
}
