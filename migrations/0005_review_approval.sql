-- Per-job override of `policy.auto_resume = "never"`: docs/todos/0010-manual-review.md.
--
-- Set only by an operator's "approve resume" action. It overrides nothing
-- else — `assess_data`'s incomplete/aliased-data check runs first regardless
-- and is never affected by this flag. Cleared whenever the job is parked for
-- review again, so an approval does not silently survive a rewind onto
-- different data.
ALTER TABLE repair_jobs ADD COLUMN resume_approved INTEGER NOT NULL DEFAULT 0 CHECK (resume_approved IN (0, 1));
