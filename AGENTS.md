# AGENTS.md — SeedMedic

Read this before changing anything. It exists so you do not have to rediscover
the architecture, and so you do not accidentally weaken a safety rule that looks
like an inconvenience.

There are localised `AGENTS.md` files under `src/repair/`, `src/tracker/`,
`src/staging/`, `src/web/`, `migrations/`, and `tests/`. They add detail; they
never contradict this one.

## What SeedMedic is

A self-hosted service that watches private trackers for hit-and-run warnings and
repairs them from media the user already has: fetch the `.torrent`, find the
matching files in the library, rebuild the torrent's exact layout in a staging
area, hand it to qBittorrent paused, force a recheck, and only then — if policy
allows — start seeding. The repair is finished when the *tracker* says the
hit-and-run is cleared, not when the client says it is seeding.

## Safety posture

SeedMedic operates on somebody's media library, which is irreplaceable and which
it does not own. The order of priorities is:

1. Never damage the library.
2. Never claim something happened that did not.
3. Repair the hit-and-run.

In that order. A repair that stops and asks a human is a success. A repair that
guesses is a failure even when it guesses right.

Concretely, these rules are load-bearing. Do not relax any of them without a
very specific reason and a test:

- **The media library is read-only.** Only `staging::adapters::local` writes to
  a filesystem, and only under a validated `StagingRoot` that is proven at
  startup not to overlap any library root.
- **Never resume an incomplete torrent whose data is hardlinked to the library.**
  The client would treat the library file as a partial download and write into
  it. `repair::policy::assess_data` refuses this unconditionally; no
  configuration value can turn it off. An *unknown* materialisation counts as
  hardlinked.
- **Exact file size is evidence, not proof.** Size agreement alone never exceeds
  `MatchConfidence::Ambiguous`. `Exact` is reserved for piece-verified matches.
- **Torrent paths are hostile input.** Nothing joins a torrent-supplied path onto
  a directory except through `torrent::SafeRelativePath`, and nothing touches the
  resulting path without `staging::safety::resolve_under`.
- **Default to paused.** `AutoResume` has no `Always`. The default is `Never`.
- **Destructive operations are narrow and explicit.** The only one is discarding
  a job's own staging directory, and it never passes `delete_files` to the
  download client.
- **Placeholders fail loudly.** An unimplemented adapter returns
  `NotImplemented { adapter, todo }` and the repair parks for review. It never
  returns an empty list, a default, or `Ok(())`.

## Architecture

A modular monolith with hexagonal boundaries inside each module.

**Why.** One deployable, one database, one process — because there is no
operational problem here that a queue or a second service would solve. Ports and
adapters, because every interesting thing SeedMedic does is a conversation with
an external system that is slow, flaky, or absent, and the workflow has to be
testable without any of them. Do not add process boundaries, message brokers, or
extra crates until something concrete demands one.

### Layout

The top level names capabilities, not layers:

```
src/
  tracker/    hit-and-run discovery, .torrent retrieval, clearance checks
  torrent/    metadata decoding and the path-safety rules
  library/    candidate discovery and deterministic matching
  staging/    materialising library content into the recovery area
  seeding/    the download client: add paused, recheck, resume
  repair/     the durable state machine that drives all of the above
  web/        operator UI (driving adapter)
  bootstrap.rs config.rs clock.rs database.rs not_implemented.rs runtime.rs
```

Inside a capability:

| File | Holds |
|---|---|
| `domain.rs` | Plain types and their invariants. No I/O, no `async`. |
| `ports.rs` | Traits the capability needs from outside, plus their error type. |
| `application/` | Use cases. Only where there is more than one worth naming. |
| `adapters/` | Implementations of the ports. Subordinate to everything above. |

`mod.rs` re-exports the capability's public surface. Keep `domain` and `ports`
private modules; export the types.

### Dependency direction

```
web  →  repair  →  {tracker, torrent, library, staging, seeding}  →  config, clock
```

- A capability may depend on another capability's **domain types and ports**
  (`repair` uses `tracker::HitAndRun`, `library` uses `torrent::TorrentFile`).
- A capability may **never** depend on another's `adapters`.
- No capability depends on `web` or `bootstrap`.
- `bootstrap.rs` is the only place that names a concrete adapter. If you find
  yourself writing `Arc::new(SomeConcreteAdapter)` anywhere else, stop.

## The repair state machine

The lifecycle is a straight line with two exits:

```
discovered → torrent_fetched → matched → staged → injected
           → rechecking → verified → seeding → completed
                    ↘ awaiting_review ↘ failed
```

A state means "everything up to here is durably done". The worker never holds
state of its own: it reads a job, decides one step from its state, and records
the result.

### Durability rules

Every transition goes through `RepairStore::apply`, which:

1. Compare-and-swaps on the `from` state, so two workers cannot both advance it.
2. Writes the audit row in the **same** database transaction.
3. Returns `Applied::AlreadyInTargetState` — not an error — when the job is
   already at `to`, and writes no second audit row.

That last point is the whole idempotency story. A step may crash between its
side effect and its transition; the replay lands on `AlreadyInTargetState`.

Because of that, **every external side effect a step performs must itself be
idempotent**. The port docs say so; honour it in every adapter. Adding a torrent
that is already present is success. Rechecking a torrent that is already being
rechecked is success.

### Adding a state transition

1. Add the state to `RepairState` and to `PROGRESSION` if it is on the happy
   path. The `CHECK` constraint in a new migration must match.
2. Extend the `match` in `validate_transition`. It is the entire transition
   table; there is nowhere else to look.
3. Add the step function under `repair/application/` and wire it into
   `application::step`, whose exhaustive match will refuse to compile until you
   do.
4. Update `actionable_states!()` in `repair/adapters/sqlite.rs` — a test enforces
   that it matches the enum.
5. Add transition-table tests in `repair/domain.rs` and, if the step touches an
   external system, a crash-recovery case in `tests/`.

### Reconciliation

Persisted state is what SeedMedic believed; reality may have moved. On startup,
`repair::reconcile::reconcile_on_startup` walks every unfinished job back to the
furthest state reality supports, and a step may return `StepOutcome::Rewind` to
do the same mid-flight.

**Reconciliation only ever moves a job backwards.** External state is never
grounds for advancing: that qBittorrent has a torrent with this info-hash does
not mean *we* put it there with the data we think we staged.

## Error handling

- Domain and port errors are `thiserror` enums, one per capability, named for
  the capability (`TrackerError`, `StagingError`, `ClientError`, `StoreError`).
- `anyhow` is for `main`, `bootstrap`, and adapters talking to the outside world
  where the caller only needs a message.
- Never `.unwrap()`. `.expect()` only where the invariant is stated in the
  message and provably holds (`"job directory names are generated, not
  supplied"`).
- Port error types carry an `is_transient()` where the distinction matters. A
  step turns a transient error into `StepOutcome::Retry` and everything else into
  `StepOutcome::Review`. Steps do not fail jobs; only an operator does.

## Logging and observability

- `tracing`, structured fields, no string interpolation of values into the
  message. `info!(job = %id, from = %state, "repair advanced")`.
- Log at a state change, not on every poll. If a message would appear on every
  tick, it is the wrong message — see the `created` flag on `Discovered`.
- `info` for lifecycle events, `warn` for things that will be retried, `error`
  for things that need a human's attention in the logs.
- Never log a `Secret`, an API key, or a password. `Secret`'s `Debug` redacts;
  do not work around it.
- The durable record is `repair_job_transitions`, not the log. Anything needed to
  explain a decision months later goes in the `detail` JSON.

## Testing

| Kind | Where | What |
|---|---|---|
| Invariants | `#[cfg(test)]` next to the code | Transition table, resume policy, path rejection, matching |
| Integration | `tests/` | The whole workflow over a real SQLite store and a real staging directory |

`tests/support/mod.rs` builds a harness that is real everywhere it can be: real
store, real local staging over temp directories, real filesystem candidate
discovery. Only the tracker and the download client are fakes, because they are
the network.

Every change to a safety rule needs a test that fails without it. Every new
external side effect needs a crash-recovery test proving the replay is harmless.

## Commands

```bash
rtk cargo fmt
rtk cargo clippy --all-targets --all-features -- -D warnings
rtk cargo test --locked
```

CI runs all three. Everything must be clean; there are no allowed warnings.

## How to add a tracker adapter

See `src/tracker/AGENTS.md` for the details. In short: implement
`TrackerClient` under `src/tracker/adapters/`, add a `TrackerKind` variant, wire
it in `bootstrap::build_trackers`, and do not add methods to the port for
capabilities no use case needs yet.

## Evolving the database

See `migrations/AGENTS.md`. Forward-only numbered files, additive where
possible, `CHECK` constraints kept in step with the enums, and no `sqlx::query!`
macros — runtime `sqlx::query()` only, so there is no offline metadata to
maintain.

## Configuration and secrets

- One TOML file, `deny_unknown_fields`, validated in `Config::validate` on
  every load — at startup, and again on every reload (see below), since a
  reload is that same load run a second time.
- Anything that would make SeedMedic unsafe or useless is rejected before it
  takes effect, not defended against at every call site.
- Secrets are `config::Secret`, which redacts itself in `Debug` and carries a
  `SecretSource` (`Environment`/`File`/`Inline`/`Unset`) so the settings UI can
  say where a value came from without ever exposing it.
- Full secrets handling (`*_file`, env overrides) is
  `docs/todos/0011-configuration-and-secrets.md`.
- Every setting is also viewable and editable at `/settings` — see
  `src/web/AGENTS.md` and `docs/todos/0017-the-settings-pages.md`. The file
  stays the source of truth and stays hand-editable; the UI writes it through
  `config::ConfigDocument`, which preserves comments and key order and never
  regenerates the file from scratch.

## The swappable runtime

Configuration can be reloaded without restarting the process — see
`docs/todos/0016-a-swappable-runtime.md` and "Why configuration is
reloadable" in `docs/architecture.md`. In short:

- `bootstrap::open` runs once per process and produces `Persistent` — the
  database connection, the clock, `WorkerHealth`, `Diagnostics` — which
  outlives every reload.
- `bootstrap::build` wires one generation (a `Runtime`) from a `Config` and a
  `Persistent`. It is synchronous and does no I/O, so a reload cannot hang.
- `src/runtime.rs`'s `RuntimeHandle` holds the current generation and serialises
  reloads: build the new generation, stop the old worker (awaited, never
  aborted), reconcile, spawn the new worker, swap. A reload that fails to
  build leaves the previous generation running, untouched.
- `staging.root`, a tracker id, and `library.roots` cannot be changed while a
  job depends on the old value; `worker.owner` cannot change while a job is
  leased. These are refused, not warned about. `database.path` and
  `server.bind_address` are accepted into the file but never applied without a
  restart, since `Persistent` and the bound listener both outlive every
  reload by construction.

## Negative code

Code is a liability. Before adding any, check whether the requirement can be met
by deleting something, narrowing the supported behaviour, or making an invariant
explicit instead.

Things that were deliberately *not* built, and should not be added without a
concrete need:

- A generic repository abstraction. `RepairStore` has the methods the workflow
  calls and no others.
- A blob store for `.torrent` files. They live in a column, so acquiring one is
  atomic with the transition that records it.
- A separate command/query/event/handler/DTO type per operation.
- A plugin system, a second crate, a message queue, event sourcing.
- Traits that exist only so a test can mock something. Every port here is
  implemented by at least one real adapter, or is a real external system that
  the bootstrap stubs.
- An abstraction that renames a dependency's API.

If you add an abstraction, be able to say which of "easier to understand, safer
to change, harder to misuse, cheaper to operate, cheaper to maintain, easier to
test, easier to delete" it buys.

## Working on TODOs

Substantial work lives in `docs/todos/`, numbered in dependency order. Each is a
cohesive change with its own acceptance criteria.

1. Pick the lowest-numbered document whose dependencies are done.
2. Read it fully, including "out of scope" and "open questions". Resolve the open
   questions before writing code, and record the answers in the document.
3. Implement it. Delete the `NotImplemented` stub it replaces and remove the
   `const TODO` pointing at the document.
4. Update the document's status line and this file if the architecture changed.

Code-level placeholders reference their document by path. Do not leave a bare
`TODO: implement this` — if the missing work needs architectural context, it
needs a document.

## Definition of done

- `rtk cargo fmt`, `clippy -D warnings`, and `test --locked` are all clean.
- New behaviour has tests; new safety rules have tests that fail without them.
- No new `.unwrap()`, no new bare `TODO`, no new warning suppressions without a
  `reason`.
- Public items that are not self-evident have doc comments saying *why*.
- If an adapter became real, its stub and its `NotImplemented` are gone.
- If the schema changed, there is a new migration and the `CHECK` constraints
  match the enums.
- The relevant `AGENTS.md` still tells the truth.
