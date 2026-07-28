-- MatchConfidence::Operator: an operator's chosen candidate for a file a
-- review page could not match automatically. See
-- docs/todos/0010-manual-review.md.
--
-- SQLite cannot alter a CHECK constraint in place, so the table is rebuilt
-- with the widened constraint; nothing else about its shape changes.
-- `repair_job_files` is never referenced as a parent by another table, so
-- this is safe inside the migration's own transaction.
CREATE TABLE repair_job_files_new (
    job_id INTEGER NOT NULL REFERENCES repair_jobs(id) ON DELETE CASCADE,
    torrent_path TEXT NOT NULL,
    length_bytes INTEGER NOT NULL,
    source_path TEXT,
    match_confidence TEXT CHECK (match_confidence IS NULL OR match_confidence IN ('exact', 'operator', 'probable', 'ambiguous')),
    match_evidence TEXT,
    materialized_as TEXT CHECK (materialized_as IS NULL OR materialized_as IN ('reflink', 'hardlink', 'copy')),
    recheck_progress REAL,

    PRIMARY KEY (job_id, torrent_path)
);

INSERT INTO repair_job_files_new
    (job_id, torrent_path, length_bytes, source_path, match_confidence, match_evidence, materialized_as, recheck_progress)
    SELECT job_id, torrent_path, length_bytes, source_path, match_confidence, match_evidence, materialized_as, recheck_progress
    FROM repair_job_files;

DROP TABLE repair_job_files;
ALTER TABLE repair_job_files_new RENAME TO repair_job_files;
