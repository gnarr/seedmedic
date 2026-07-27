# 0001 — Worker loop hardening and deeper reconciliation

**Status:** Done
**Depends on:** nothing
**Blocks:** 0012

## Problem

The worker loop is correct for the happy path and for the failure modes the
tests cover, but three things will hurt in production:

1. **Leases do not renew.** A step that takes longer than `worker.lease` (300s
   by default) — a recheck of a 60 GB torrent, a slow tracker — lets the lease
   expire while the worker is still working. Another tick can then claim the same
   job. The compare-and-swap prevents corruption, but the duplicated work is
   wasted and the logs become confusing.

2. **Backoff has no jitter.** Several jobs failing against the same down tracker
   retry in lockstep, hammering it in bursts at exactly the moments it is least
   able to cope.

3. **Reconciliation ignores parked jobs.** `unfinished()` only returns actionable
   states, so a job sitting in `awaiting_review` is never reconciled. When an
   operator retries it, it resumes into a state that may no longer match reality
   — which is exactly what the mid-flight `Rewind` was added to handle, but it
   costs a round trip and a confusing log line first.

## Architectural context

`src/repair/worker.rs` claims jobs with an expiring lease and drives each one
through as many steps as it can before releasing. `src/repair/reconcile.rs` runs
once at startup. Both are described in `src/repair/AGENTS.md`.

Leases are cooperative: the only enforcement is the `lease_expires_at` predicate
in the claim query, plus the compare-and-swap on every transition.

## Expected behaviour

- A worker holding a lease renews it while it is still working, so a long step
  cannot lose its claim.
- Retry delays are spread out, so simultaneous failures do not resynchronise.
- Parked jobs are reconciled at startup by correcting `review_from_state`, so
  an operator's retry resumes at a step that is still true.
- A job whose lease is renewed but whose worker then dies still becomes
  claimable within one lease period.

## Implementation steps

1. **Lease renewal.** Add `RepairStore::renew_lease(id, owner, lease)` that
   extends `lease_expires_at` only where `lease_owner` still matches — a worker
   that lost its lease must not silently take it back. In
   `RepairWorker::drive`, renew after each step that returns `Advance`, and
   spawn nothing: renewing between steps is enough if no single step outlives a
   lease. Log at `warn` if a renewal affects zero rows, because that means we
   have been working on a job somebody else now owns; abandon the job for this
   tick when that happens.

2. **Jitter.** Change `policy::retry_delay` to return a range, or add
   `retry_delay_with_jitter(attempts, policy, seed)`. Keep it a pure function —
   pass the jitter source in rather than calling a random generator inside, so
   the existing tests stay deterministic. Full jitter (uniform over
   `[0, computed]`) is the usual recommendation; decide and record which.

3. **Reconcile parked jobs.** Extend `RepairStore::unfinished()` to include
   `awaiting_review`, or add a separate query. For a parked job, reconcile
   `review_from_state` rather than `state`: if the recorded resume point is past
   what reality supports, move it back. This needs a store method —
   `set_review_resume_point(id, state)` — that writes an audit row with reason
   `reconciliation` so the change is visible.

4. **Sanity bound on `drive`.** The loop bound is currently
   `PROGRESSION.len() * 2`. Add a `debug_assert!` or a `warn!` when it is hit, so
   a rewind/advance oscillation shows up rather than silently capping.

## Invariants and safety constraints

- Renewal must be owner-scoped. A worker that lost its lease must not reacquire
  it by renewing.
- Reconciling a parked job must not un-park it. Only an operator moves a job out
  of `awaiting_review`.
- Jitter must never produce a delay of zero, or a failing job becomes a hot loop.
- Backwards-only still applies: correcting a resume point may only move it
  earlier.

## Likely files

- `src/repair/worker.rs`
- `src/repair/reconcile.rs`
- `src/repair/policy.rs`
- `src/repair/ports.rs`, `src/repair/adapters/sqlite.rs`
- `tests/crash_recovery.rs`

## Required tests

- Renewal extends the lease and a second claim still fails.
- Renewal by a non-owner affects nothing and the worker gives the job up.
- A worker that renews and then stops still releases the job after one lease
  period.
- `retry_delay` with a fixed jitter source is deterministic and never zero.
- A parked job whose staged data vanished has its resume point moved back, and
  an operator retry lands in `matched` rather than `verified`.
- Reconciling a parked job leaves it parked.

## Acceptance criteria

- All existing tests still pass unchanged, except where the new behaviour is the
  point.
- A step that sleeps past the lease period keeps its claim.
- Fifty jobs failing against one dead tracker retry with visibly spread delays.
- `rtk cargo clippy --all-targets --all-features -- -D warnings` is clean.

## Out of scope

- Multiple worker processes. The lease design supports it, but nothing else has
  been thought through and it is not a requirement.
- Prioritising jobs by tracker deadline. Worth doing eventually; it is a change
  to the claim query's `ORDER BY` and belongs with 0012's operational work.
- Per-tracker concurrency limits.

## Open questions

- Should renewal happen inside `apply` (free, since it already writes the row)
  or as a separate call? Inside is fewer round trips but conflates two concerns
  and makes `apply` care about the lease, which it currently does not.
- Full jitter or decorrelated jitter? Full is simpler; decorrelated recovers
  faster from long outages.
- Should a job that repeatedly oscillates between rewind and advance be parked
  automatically? It indicates a disagreement between our state and the client's
  that a human should probably see.
