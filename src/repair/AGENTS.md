# AGENTS.md — `src/repair`

Supplements the root `AGENTS.md`. This module is the durable state machine;
almost every correctness property of SeedMedic is enforced here.

## The shape of a step

A step is `async fn(&RepairDeps, &RepairJob) -> StepOutcome`. It does the work
for exactly one state and returns what it concluded. **It does not persist
anything.** The worker turns the outcome into exactly one transition.

That split is deliberate: "did the side effect happen?" and "was it recorded?"
must be one question, answered by one compare-and-swap, or crash recovery
becomes guesswork.

The five outcomes and when to use them:

| Outcome | Use when | Costs an attempt |
|---|---|---|
| `Advance` | The step's work is done | No (attempts reset) |
| `Review` | A human has to decide, or an adapter is a stub | No |
| `Rewind` | Reality is behind the persisted state | No |
| `Wait` | Not ready yet — recheck running, tracker unchanged | No |
| `Retry` | Something failed that might not fail next time | Yes |

There is no `Fail`. Steps never fail a job; the retry budget parks it for review
and only an operator abandons it. If you are reaching for a way to terminate a
job from a step, you want `Review`.

## Rules for `apply`

`RepairStore::apply` is a compare-and-swap plus an audit insert in one
transaction. Any implementation must:

- Update only if the row is still in the transition's `from` state.
- Return `Applied::AlreadyInTargetState` — not an error, and with **no** second
  audit row — when the row is already at `to`.
- Return `StoreError::Conflict` for any other state.
- Reset `attempts` to 0 and clear `next_attempt_at`.
- Set `review_from_state` exactly when moving to `awaiting_review`, and clear it
  otherwise.
- Leave the lease alone. Only `release` clears it, so a worker can drive a job
  through several steps while holding one lease.

## Rules for transitions

`validate_transition` is the entire table. Do not add a second place where a
transition is judged legal.

- `Progress` advances exactly one step along `PROGRESSION`.
- `Review` may only come from an actionable state and only targets
  `awaiting_review`.
- `OperatorRetry` may only resume `review_from_state`. It must never be able to
  skip a step — that is why the absence of `review_from_state` is an error
  rather than a default.
- `Reconciliation` only moves backwards, and only between lifecycle states.
- `Failure` cannot touch a terminal job.
- `OperatorRestart` is the only way out of `failed`, and it goes to the
  beginning.

## Idempotency checklist for a new step

Before merging a step that touches an external system, answer all of these:

1. If the process dies immediately after the side effect, what does the replay
   do? (It must be harmless.)
2. If the external system already did this, does the adapter report success?
3. Does the step read anything it needs from the job rather than from memory
   carried between ticks?
4. Is there a test in `tests/crash_recovery.rs` for the crash point?

## Retries, waits, and the budget

`attempts` counts consecutive failures at the current state and resets on every
transition. `Wait` deliberately does not count, so a torrent that takes an hour
to recheck does not exhaust the budget. When `attempts` reaches
`policy.max_attempts`, the job parks as `RetryBudgetExhausted` — it never fails
silently and never retries forever.

## Leases

The lease is the only thing preventing two workers from acting on one job, and
its expiry is the only thing recovering a job from a worker that died. At
startup, `clear_stale_leases(owner)` drops expired leases *and* leases held by
this instance's owner, on the assumption that a process with our owner id is the
one that just died. That assumption breaks if you ever run two instances with
the same `worker.owner`; give them different ones.

## Policy

`policy.rs` is pure functions over plain data. Keep it that way — no I/O, no
`async`, no reaching into the store. A safety decision that cannot be unit tested
in three lines is in the wrong place.

`assess_data` is the one rule configuration cannot weaken. If you add a policy
knob, make sure it can only ever make the answer *more* conservative.
