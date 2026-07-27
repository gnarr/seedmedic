# AGENTS.md — `tests`

Supplements the root `AGENTS.md`.

## What belongs here

Integration tests drive the **whole workflow** through the worker. Anything that
can be proven about a single function belongs in a `#[cfg(test)]` module next to
that function instead — those run faster and fail more precisely.

| File | Covers |
|---|---|
| `repair_lifecycle.rs` | Discovery through seeding and completion, the file plan, the audit trail, the incomplete-data gate |
| `idempotency.rs` | Rediscovery, replayed transitions, stale transitions, repeated side effects |
| `crash_recovery.rs` | Dead-worker leases, vanished staging, vanished torrent, reconciliation direction |
| `seeding_monitoring.rs` | Client health checks, tracker-status escalation, deadlines, and progress recording while a job sits in `Seeding` |

## Real where it can be

`support::Harness` is real everywhere except the network:

- **Real** SQLite store (in-memory, real migrations) — the store *is* the thing
  under test for durability; there is no in-memory fake of it and there must not
  be one.
- **Real** `LocalStaging` over temporary directories, with a real library
  directory containing real files of the right sizes.
- **Real** `FilesystemCandidateSource` doing a real directory walk.
- **Fake** tracker and download client, because they are HTTP.

If you find yourself wanting a fake for something that is not a network service,
that is a signal the design is wrong, not that a fake is needed.

## Fakes must behave

`FakeTracker` and `FakeTorrentClient` are not stubs. They model state: a
hit-and-run stays `Active` until explicitly cleared, a recheck takes a
configurable number of polls, resuming incomplete data yields `Downloading`
rather than `Seeding`. They count calls, which is how the tests prove a replay
did not duplicate a side effect.

When you extend a port, extend the fake to model the new behaviour rather than
returning `Ok(())`. A fake that always succeeds tests nothing.

## The clock

`TestClock` starts at a fixed instant and only moves when a test moves it.
`Harness::run_until` advances it between ticks, which is why waits and backoff
are exercised rather than skipped. Never `tokio::time::sleep` in a test — if a
test needs time to pass, advance the clock.

## Writing a new case

- Name it as the property it proves:
  `a_torrent_that_disappears_mid_flight_rewinds_without_a_restart`, not
  `test_rewind`.
- Assert on the *durable* record — job state, `planned_files`, `history` — not
  on log output.
- For a new external side effect, add a crash point: run to a state, break
  reality, and assert both that the repair recovers and that the call count did
  not go up.
- `assert!` messages state the rule, so a failure reads as a violated invariant
  rather than a mismatched value.

## Bounds

`run_until` takes a tick budget and panics with the job's state and review reason
when it runs out. Keep the budget tight enough that an infinite loop fails fast,
and prefer fixing a stuck job over raising the number.
