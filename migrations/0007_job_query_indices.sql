-- Indices for the operator UI's job list and its "may be oscillating" check.
-- Purely additive: no new column, no CHECK constraint, so `job_columns!()` and
-- the two schema-drift tests in src/repair/adapters/sqlite.rs are unaffected.
-- See docs/todos/0021-a-react-operator-ui.md.

-- The list orders by updated_at (the default) or created_at, and pages with a
-- keyset on (column, id) rather than an offset. Without a matching index every
-- page is a full scan plus a sort. The trailing id matches the tie-breaker in
-- the ORDER BY, so the index covers the whole ordering.
CREATE INDEX repair_jobs_updated_at ON repair_jobs (updated_at DESC, id DESC);
CREATE INDEX repair_jobs_created_at ON repair_jobs (created_at DESC, id DESC);

-- repair_job_transitions grows with every transition, forever, and this is the
-- only query that filters on `reason`.
CREATE INDEX repair_job_transitions_reason ON repair_job_transitions (reason, job_id);
