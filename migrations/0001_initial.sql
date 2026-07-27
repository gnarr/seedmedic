-- One row per hit-and-run being repaired. `state` is the durable repair state
-- machine (src/repair/domain.rs); every change to it is a compare-and-swap that
-- also appends to repair_job_transitions in the same SQLite transaction.
CREATE TABLE repair_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,

    -- Identity. The unique constraint is what makes discovery idempotent:
    -- re-observing the same hit-and-run updates the existing job.
    tracker_id TEXT NOT NULL,
    tracker_torrent_id TEXT NOT NULL,
    torrent_name TEXT NOT NULL,

    state TEXT NOT NULL CHECK (state IN (
        'discovered', 'torrent_fetched', 'matched', 'staged', 'injected',
        'rechecking', 'verified', 'seeding', 'completed', 'awaiting_review', 'failed'
    )),
    -- Only set while state = 'awaiting_review'. Records the state the job must
    -- return to when an operator retries, so review cannot be used to skip work.
    review_from_state TEXT CHECK (review_from_state IS NULL OR review_from_state IN (
        'discovered', 'torrent_fetched', 'matched', 'staged', 'injected',
        'rechecking', 'verified', 'seeding'
    )),
    review_reason TEXT,
    failure_reason TEXT,

    -- Populated as the job advances.
    info_hash TEXT,
    -- The .torrent itself. Stored inline (they are tens of kilobytes) so that
    -- acquiring it is atomic with the state transition that records it, and so
    -- there is no second store to reconcile after a crash.
    torrent_file BLOB,
    total_bytes INTEGER,
    staging_dir TEXT,
    materialization TEXT CHECK (materialization IS NULL OR materialization IN ('reflink', 'hardlink', 'copy')),

    -- Retry accounting. `attempts` counts consecutive failed attempts at the
    -- current state and is reset on every successful advance.
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,

    -- Cooperative lease. A crashed worker's jobs become claimable again once
    -- lease_expires_at passes; there is no queue to rebuild.
    lease_owner TEXT,
    lease_expires_at TEXT,

    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,

    UNIQUE (tracker_id, tracker_torrent_id)
);

CREATE INDEX repair_jobs_claimable ON repair_jobs (state, next_attempt_at, lease_expires_at);
CREATE INDEX repair_jobs_info_hash ON repair_jobs (info_hash);

-- Append-only audit trail. Every automated decision must be explainable from
-- this table alone, so `detail` carries the evidence as JSON.
CREATE TABLE repair_job_transitions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES repair_jobs(id) ON DELETE CASCADE,
    from_state TEXT NOT NULL,
    to_state TEXT NOT NULL,
    reason TEXT NOT NULL,
    detail TEXT,
    occurred_at TEXT NOT NULL
);

CREATE INDEX repair_job_transitions_job ON repair_job_transitions (job_id, id);

-- The per-file repair plan: what the torrent wants, what we chose from the
-- library, why we chose it, and how it was materialised.
CREATE TABLE repair_job_files (
    job_id INTEGER NOT NULL REFERENCES repair_jobs(id) ON DELETE CASCADE,
    -- Validated relative path (torrent::path::SafeRelativePath). Never trusted
    -- straight from the .torrent.
    torrent_path TEXT NOT NULL,
    length_bytes INTEGER NOT NULL,
    source_path TEXT,
    match_confidence TEXT CHECK (match_confidence IS NULL OR match_confidence IN ('exact', 'probable', 'ambiguous')),
    match_evidence TEXT,
    materialized_as TEXT CHECK (materialized_as IS NULL OR materialized_as IN ('reflink', 'hardlink', 'copy')),

    PRIMARY KEY (job_id, torrent_path)
);
