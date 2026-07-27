# 0013 — End-to-end and fault-injection test harness

**Status:** Not started
**Depends on:** 0008, 0009
**Blocks:** nothing

## Problem

The current tests are good at the paths somebody thought of. What they do not
cover:

- **Crashes at arbitrary points.** `tests/crash_recovery.rs` breaks reality at
  three hand-picked moments. A repair has a dozen places where the process could
  die between a side effect and its transition, and only a few are exercised.
- **Real adapters.** Every test runs against fakes for the tracker and the
  client. Once 0002 and 0007 land, the wiremock tests prove each adapter in
  isolation, but nothing proves the whole workflow against a real qBittorrent.
- **Concurrency.** `batch_size` defaults to 4, and no test ever runs two jobs at
  once, let alone two workers.
- **Scale.** Nothing runs with a hundred jobs, a thousand-file torrent, or a
  library with a hundred thousand files.
- **Time.** `TestClock` is advanced in fixed 30-second steps. Long waits,
  lease expiry mid-step, and backoff over hours are untested.

## Architectural context

`tests/AGENTS.md` describes the harness: real store, real staging, real
filesystem discovery, fakes only for the network. That principle holds — this
document extends the harness rather than replacing it.

Every side effect in the system is idempotent and every transition is a
compare-and-swap. Those are the properties this document tries to *break*.

## Expected behaviour

- A test can inject a failure at any named point in a repair and assert that
  recovery is clean and no side effect is duplicated.
- The whole workflow runs against a real qBittorrent in an opt-in test.
- Concurrent jobs and concurrent workers behave.
- Performance characteristics are known rather than assumed.

## Implementation steps

1. **A fault-injection layer.** Add a decorator adapter — `FailAt<T>` wrapping
   any port — configured with "fail the Nth call to method M with error E". Then
   enumerate the crash points and generate a test per point:

   ```
   for point in every_side_effect_in_a_repair() {
       run to point, kill, restart, run to completion,
       assert completed and every call count is exactly 1
   }
   ```

   This is the highest-value item here. It turns "we thought about crashes" into
   "we checked every one".

2. **A `Crash` that is really a crash.** Today "crash" means abandoning a
   `Harness`. Closer to reality: build a second `RepairDeps` over the *same*
   SQLite file and the same staging directory, with fresh in-memory fakes, and
   run reconciliation. That models a process restart, including the fakes losing
   their state — which is what caught the missing rewind during the bootstrap.

3. **Real qBittorrent.** An opt-in integration test — `#[ignore]`, or gated on
   `SEEDMEDIC_QBITTORRENT_URL` — that runs the full workflow against a live
   instance. Provide a `docker-compose.test.yml`. Do not make CI depend on it;
   do run it before a release.

4. **Concurrency.** Two workers with different owners against one store, twenty
   jobs, asserting: every job completes exactly once, no job is claimed by both,
   no duplicated side effects. Then the same with one worker and
   `batch_size = 8`.

5. **Scale.** A torrent with 2000 files and a library with 50,000 candidates.
   Assert on completion and on bounds — matching should be roughly linear, the
   filesystem walk should happen once per job, staging should not hold every
   file open at once.

6. **Time.** With `TestClock` under test control, assert: backoff actually
   delays a retry, a lease expires while a step is running (needs 0001's
   renewal), and a multi-day seed wait polls a bounded number of times.

7. **Property tests, maybe.** A generated sequence of transitions and failures
   against the state machine, asserting invariants: no job reaches `seeding`
   without passing `verified`; no job is `completed` without a tracker
   clearance; the audit trail is always a valid path through the state graph.
   `proptest` is a dependency worth weighing against the bugs it would find.

## Invariants and safety constraints

- Tests never touch a path outside a temporary directory. A test that could
  delete somebody's media is worse than no test.
- The real-qBittorrent test uses a dedicated container and its own staging
  directory, and is explicitly opt-in.
- Fault injection is test-only. `FailAt` must not be reachable from the
  production binary — gate it behind `#[cfg(test)]` or a non-default feature.
- Assertions are on durable state, not logs.

## Likely files

- `tests/support/mod.rs` (restart helper, `FailAt`)
- `tests/fault_injection.rs` (new)
- `tests/concurrency.rs` (new)
- `tests/scale.rs` (new, possibly `#[ignore]`)
- `tests/live_qbittorrent.rs` (new, `#[ignore]`)
- `docker-compose.test.yml`
- `.github/workflows/ci.yml`

## Required tests

The deliverable is tests, so the acceptance criteria carry the detail. At
minimum, one generated case per side effect in the repair lifecycle, and the
concurrency and time cases above.

## Acceptance criteria

- Every external side effect has a crash-before-and-after case, and all pass.
- Two workers over twenty jobs complete all twenty exactly once, with no
  duplicated adds.
- The full workflow passes against a real qBittorrent.
- A 2000-file torrent completes, with a recorded runtime so a regression is
  visible.
- CI stays fast — the slow tests are `#[ignore]` and run on demand.

## Out of scope

- Testing against a real private tracker. Nobody should point a test suite at
  one; the wiremock tests in 0002 cover the adapter.
- Chaos testing the database. SQLite's durability is not ours to verify.
- Load testing the web UI.

## Open questions

- How to enumerate crash points without hand-listing them? Instrumenting the
  worker to name its side effects would make the list derivable rather than
  maintained.
- Is `proptest` worth the dependency? The state machine is small enough that the
  table-driven tests may already cover it exhaustively — count the paths and
  decide.
- Should the concurrency test run in CI? It is inherently timing-sensitive, and a
  flaky test that guards a real invariant is a genuine dilemma.
- What is a realistic upper bound for a media torrent's file count? It sets the
  scale target.
