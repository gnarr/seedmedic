-- Per-file completeness from the most recent recheck. Additional evidence for
-- the review page (src/repair/AGENTS.md), never an input to the resume gate:
-- absence of this column leaves `assess_data`/`decide_resume` unchanged.
ALTER TABLE repair_job_files ADD COLUMN recheck_progress REAL;
