# 0013 — End-to-end and fault-injection test harness

**Status:** Done
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

1. **A fault-injection layer.** ✅ `tests/support/fail_at.rs`'s `FailAt` wraps
   `RepairStore` and fails one chosen `apply` call — the Nth transition a
   driven job makes — rather than wrapping every port individually. Since
   every side-effecting step ends in exactly one `apply`, this one decorator
   covers all of them uniformly. `tests/fault_injection.rs` generates one test
   per state in `RepairState::PROGRESSION`, asserting the repair still
   completes and that `add_count`/`recheck_count`/`resume_count` each stay at
   1 regardless of which transition's write was made to fail.

2. **A `Crash` that is really a crash.** ✅ `Harness::new_file_backed` and
   `Harness::restart` (`tests/support/mod.rs`) rebuild `RepairDeps` from a
   freshly opened connection to the same on-disk SQLite file, plus fresh
   `WorkerHealth`/`Diagnostics`. The tracker and download client fakes are
   *not* rebuilt — they model network services whose state does not depend on
   which of our processes is talking to them, so reusing them is the more
   faithful choice, not a shortcut. See
   `tests/crash_recovery.rs::a_repair_survives_a_genuine_close_and_reopen_of_the_database`.

3. **Real qBittorrent.** ✅ `tests/live_qbittorrent.rs`, `#[ignore]`d and
   additionally gated on `SEEDMEDIC_QBITTORRENT_URL` at runtime, plus
   `docker-compose.test.yml` for a disposable instance. Verified end to end
   against a real qBittorrent 5.2.3 container — which surfaced two real
   adapter bugs (login's status-code/body convention and the
   `QBT_SID_<port>` session cookie name; both fixed). Not run by CI.

4. **Concurrency.** ✅ `tests/concurrency.rs`: two workers with different
   owners, and separately one worker at `batch_size = 8`, over twenty
   independent jobs on a real SQLite file, on a multi-threaded runtime so
   claims genuinely race rather than merely interleave. Runs in the default
   suite (see the resolved open question below on why this is not flaky).

5. **Scale.** ✅ `tests/scale.rs`, `#[ignore]`d (writes 50,000 files to disk): a
   2000-file torrent against a 50,000-file library, with wall-clock runtime
   printed so a regression is visible, and a counting `CandidateSource` that
   asserts the library is walked once per job. Not a strict complexity
   assertion — see the resolved open question below.

6. **Time.** ✅ `tests/time_control.rs` covers backoff (a retriable failure
   delays the next attempt and does not retry early) and bounded multi-day
   polling (a five-day hit-and-run deadline costs a few hundred polls, not
   thousands). `tests/lease_renewal.rs` covers the lease-survives-a-slow-step
   case, via `FakeTorrentClient::slow_down`, which advances the clock and
   signals a channel on every client call so a test can probe the store from
   inside a step it does not otherwise control the timing of.

7. **Property tests: decided against.** See the resolved open question below.

## Invariants and safety constraints

- Tests never touch a path outside a temporary directory. A test that could
  delete somebody's media is worse than no test.
- The real-qBittorrent test uses a dedicated container and its own staging
  directory, and is explicitly opt-in.
- Fault injection is test-only. `FailAt` must not be reachable from the
  production binary — gate it behind `#[cfg(test)]` or a non-default feature.
- Assertions are on durable state, not logs.

## Likely files

- `tests/support/mod.rs` — restart helper (`Harness::new_file_backed`,
  `Harness::restart`), `worker_for`, `run_until_with`
- `tests/support/fail_at.rs` — `FailAt`
- `tests/fault_injection.rs`
- `tests/concurrency.rs`
- `tests/scale.rs` (`#[ignore]`)
- `tests/live_qbittorrent.rs` (`#[ignore]`)
- `tests/time_control.rs`
- `docker-compose.test.yml`
- `src/seeding/adapters/fake.rs` — `FakeTorrentClient::slow_down` /
  `stop_slowing_down`, for the lease-renewal-under-a-slow-step test
- `src/seeding/adapters/qbittorrent.rs` — two real bugs the live test found:
  the login status-code/body convention and the `QBT_SID_<port>` cookie name
- `.github/workflows/ci.yml` — unchanged; `cargo test` already skips
  `#[ignore]`d tests by default, which is all this needed

## Required tests

The deliverable is tests, so the acceptance criteria carry the detail. At
minimum, one generated case per side effect in the repair lifecycle, and the
concurrency and time cases above. All delivered — see the implementation
steps above for where each one lives.

## Acceptance criteria

- [x] Every external side effect has a crash-before-and-after case, and all
  pass. (`tests/fault_injection.rs`, generated from
  `RepairState::PROGRESSION`.)
- [x] Two workers over twenty jobs complete all twenty exactly once, with no
  duplicated adds. (`tests/concurrency.rs`.)
- [x] The full workflow passes against a real qBittorrent.
  (`tests/live_qbittorrent.rs`, verified against qBittorrent 5.2.3.)
- [x] A 2000-file torrent completes, with a recorded runtime so a regression
  is visible. (`tests/scale.rs`.)
- [x] CI stays fast — the slow tests are `#[ignore]` and run on demand.

## Out of scope

- Testing against a real private tracker. Nobody should point a test suite at
  one; the wiremock tests in 0002 cover the adapter.
- Chaos testing the database. SQLite's durability is not ours to verify.
- Load testing the web UI.

## Open questions

- ~~How to enumerate crash points without hand-listing them?~~ Resolved:
  `RepairState::PROGRESSION` already is that list — every state past
  `Discovered` is one step's crash point, so `fault_injection.rs` iterates it
  directly instead of hand-listing side effects separately. No new
  instrumentation needed.
- ~~Is `proptest` worth the dependency?~~ Resolved: no. Counting the paths, as
  suggested: `RepairState` has 11 variants and `TransitionReason` has 8, but
  `validate_transition` is not one combinatorial function over both — it is a
  `match` on `TransitionReason` where each arm is one or two independent
  structural checks (rank+1, a fixed target state, an `Option` field being
  set), none of which interact with each other. The two arms where a state
  loop could hide something (`Progress`, which must hold for every
  consecutive pair, and `Review`, which must hold for every actionable state)
  already have exhaustive loops in `src/repair/domain.rs`'s 13 unit tests; the
  rest are simple enough that one positive and one negative case each is the
  whole truth table. A generator would mostly re-derive those same loops at
  the cost of a new dependency and slower builds. The property invariants
  under "Property tests, maybe" ("no job reaches `seeding` without
  `verified`", audit-trail validity) are already structural consequences of
  `PROGRESSION` being a fixed sequence walked one step at a time — there is no
  code path that skips a step, so no generator is needed to demonstrate one
  can't be taken.
- ~~Should the concurrency test run in CI?~~ Resolved: yes.
  `tests/concurrency.rs` avoids the usual source of flakiness — real-time
  races — by never sleeping: both workers advance a shared clock together,
  between synchronized rounds, and the race that matters (two claims for the
  same job) is genuine tokio-task concurrency on a multi-threaded runtime,
  not something timed against a wall clock. Five consecutive local runs
  before landing it were all green.
- ~~What is a realistic upper bound for a media torrent's file count?~~
  Resolved pragmatically: 2000, twice the "thousand-file torrent" this
  document's Problem section named as the target, for margin. `NOISE_FILES`
  brings the library to 50,000 total candidates, per the Implementation
  steps' own number.
