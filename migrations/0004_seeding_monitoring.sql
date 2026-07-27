-- Fields needed to monitor a job while it sits in `seeding`, potentially for
-- days: docs/todos/0009-tracker-confirmation.md.
--
-- `consecutive_unknown_tracker_status` counts consecutive `Unknown` answers
-- from the tracker so a broken adapter escalates instead of polling forever;
-- it resets to 0 on any `Active` or `Cleared` answer.
ALTER TABLE repair_jobs ADD COLUMN consecutive_unknown_tracker_status INTEGER NOT NULL DEFAULT 0;

-- When the hit-and-run warning becomes a penalty, if the tracker said so at
-- discovery (HitAndRun::deadline). Drives both the adaptive tracker-poll
-- backoff and parking the job once a deadline passes without clearing.
ALTER TABLE repair_jobs ADD COLUMN deadline TEXT;
