-- When the current recheck began (the injected -> rechecking transition
-- timestamp), so a running check can be polled with adaptive backoff and
-- parked once it exceeds policy.recheck_timeout_seconds. Unset outside the
-- rechecking state.
ALTER TABLE repair_jobs ADD COLUMN rechecking_started_at TEXT;
