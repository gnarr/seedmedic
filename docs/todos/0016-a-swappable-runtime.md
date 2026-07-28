# 0016 — A swappable runtime

**Status:** Not started
**Depends on:** 0015
**Blocks:** 0017, 0019

## Problem

Configuration is consumed exactly once. `main` loads it, hands it to
`bootstrap::build`, and `build` decomposes it into an `App` holding an
`Arc<RepairDeps>`, a `WorkerConfig`, and four derived values. Nothing afterwards
holds a `Config`. `RepairDeps` says so in its own doc comment: "Assembled once at
startup. Steps borrow it; nothing mutates it."

That is a good property, and this document keeps it. What it does not support is
changing a setting without restarting the process — which the settings UI (0017)
needs, because a first-run flow that ends at "now go find a terminal and
`docker restart`" is not a first-run flow.

`docs/todos/0011` resolved against building this ("Config reloading: not worth
it — restarting the process is cheap"). That reasoning assumed the operator was
already in a terminal editing a TOML file, in which case a restart genuinely is
free. It stops holding the moment configuration happens in a browser. The
resolution in 0011 is amended by this document rather than quietly contradicted.

## Architectural context

The reload is not a new mechanism. It is the existing startup sequence, run
again, against durable state that was designed for exactly this:

- Every side effect is idempotent and every transition is a compare-and-swap, so
  re-running a step is harmless.
- `reconcile_on_startup` walks each unfinished job *backwards* to the last state
  reality still supports, and never forwards.
- The worker holds no state of its own.

So "stop the worker, rewire the adapters, reconcile, start a new worker" is
indistinguishable from a restart, from the state machine's point of view. The
work is in getting the ordering and the ownership right.

Two things must **not** be rebuilt, and getting this wrong is the main hazard:

- `WorkerHealth` — rebuild it and `last_tick()` is `None`, so `/health` returns
  503 after every settings save until the next tick. A settings change must not
  make a container look unhealthy.
- `Diagnostics` — the tracker error history an operator is looking at when they
  change a setting is precisely the thing they must not lose by changing it.

And the store must not be rebuilt either, for a stronger reason: `database.path`
changing mid-flight orphans every in-flight job, and reopening runs migrations
against a database the old worker may still be writing to, in a database that is
not in WAL mode.

## Expected behaviour

- A reload replaces every adapter derived from configuration, in one step, with
  no window in which some are old and some are new.
- A reload that cannot be brought up changes nothing. The previous runtime keeps
  serving and keeps working, and the error reaches the caller.
- Two concurrent reloads serialise. There is never more than one worker.
- `/health` does not dip after a reload.
- Tracker diagnostics and metrics counters survive a reload.
- `server.bind_address` and `database.path` are reported as needing a process
  restart, and are not applied. Nothing else does.
- A reload reconciles, because reality may have moved: repointing
  `download_client.base_url` at a different qBittorrent must rewind every job
  past `injected`.

## Implementation steps

1. **Split `bootstrap::build` in two.**

   ```rust
   /// Opened once per process. Everything here outlives every reload.
   pub struct Persistent {
       pub store: Arc<dyn RepairStore>,
       pub clock: Arc<dyn Clock>,
       pub worker_health: Arc<WorkerHealth>,
       pub diagnostics: Arc<Diagnostics>,
       #[cfg(feature = "metrics")]
       pub metrics: Arc<crate::metrics::Metrics>,
   }

   pub async fn open(config: &Config) -> Result<Persistent>;

   /// Wire one generation. Synchronous: nothing here does network or database
   /// I/O, so a reload cannot hang.
   pub fn build(config: &Config, persistent: &Persistent) -> Result<(Runtime, WorkerConfig)>;
   ```

   `build` becoming synchronous is a genuine simplification, not cosmetic:
   `database::connect` was its only `await`, and everything else — the HTTP
   client, the trackers, the client, the candidate sources, `StagingRoot::new` —
   is already sync. No timeout question, no cancellation question.

2. **`Runtime` carries everything one configuration produces**, not just the
   deps. This is the fix for a whole class of bug: `AppState` currently snapshots
   `auth_token`, `health_threshold`, `config_summary` and `metrics_enabled` at
   router construction, so swapping only `Arc<RepairDeps>` would leave an
   operator who just set `server.auth_token` looking at a page that says "Saved"
   while the UI stays unauthenticated.

   ```rust
   /// Everything one configuration produces. Replaced wholesale; never mutated,
   /// so a request that started against generation N finishes against
   /// generation N even if N+1 lands mid-request.
   pub struct Runtime {
       pub deps: Arc<RepairDeps>,
       pub health_threshold: Duration,
       pub auth_token: Option<Arc<str>>,
       pub config_summary: Arc<str>,
       pub metrics_enabled: bool,
   }
   ```

3. **`src/runtime.rs`**, a new cross-cutting support module alongside
   `bootstrap.rs`:

   ```rust
   pub struct RuntimeHandle {
       current: std::sync::RwLock<Arc<Runtime>>,
       /// Held for the whole of a reload, so two saves serialise instead of one
       /// interleaving its stop with the other's respawn. Holds the worker so
       /// there is exactly one place that owns it.
       reload: tokio::sync::Mutex<Option<WorkerTask>>,
       persistent: bootstrap::Persistent,
       config_path: PathBuf,
   }

   struct WorkerTask {
       shutdown: tokio::sync::watch::Sender<bool>,
       handle: tokio::task::JoinHandle<()>,
   }

   impl RuntimeHandle {
       /// One read lock, one `Arc` clone. Deliberately never hands out the
       /// guard, so there is no way to hold the lock across an `await`.
       pub fn current(&self) -> Arc<Runtime>;
   }
   ```

   `std::sync::RwLock` rather than `arc-swap`, `tokio::sync::RwLock`, or a
   `watch` channel: reads happen a handful of times per request and once per
   tick, so lock-free reads buy nothing measurable and `arc-swap` would be a
   dependency added for an unmeasured problem; `tokio::sync::RwLock` would colour
   `current()` async for no reason. The one hazard — holding a guard across an
   `await` — is removed by the API shape above rather than by discipline.

4. **The reload, and its ordering, which is the entire safety argument.**

   ```rust
   pub async fn reload(&self) -> Result<Applied, ReloadError> {
       let mut worker = self.reload.lock().await;

       // 1. BUILD FIRST. Both of these can fail, and both return here with the
       //    old runtime installed and the old worker still ticking.
       let config = Config::load_from(&self.config_path)?;
       let (runtime, worker_config) =
           bootstrap::build(&config, &self.persistent).map_err(ReloadError::Build)?;
       let runtime = Arc::new(runtime);

       // 2. Only now is it safe to stop the old worker. Await it; never abort.
       if let Some(old) = worker.take() { old.stop().await; }

       // 3. Same situation as a restart: new adapters over durable state.
       reconcile_on_startup(&runtime.deps, &worker_config.owner).await;

       // 4. Swap, then spawn.
       *self.current.write().expect("runtime lock poisoned") = runtime.clone();
       *worker = Some(WorkerTask::spawn(runtime, worker_config));
       Ok(applied)
   }
   ```

   Build-before-stop is what makes a failed reload a pure no-op. The operator's
   `POST` *is* the reload, so the error lands on the page they are looking at,
   naming what is wrong — which is why this is inline rather than in a supervisor
   task. A supervisor would need a "reload in progress" state, somewhere to stash
   the last error, and a page that lies for a moment.

5. **Reconcile only after the old worker has fully stopped.**
   `clear_stale_leases` clears leases `WHERE lease_expires_at <= ? OR lease_owner
   = ?`, so running it while the old worker is alive unleases that worker's own
   in-flight jobs — and `apply` compare-and-swaps on `state` only, not on the
   lease, so the old worker would keep recording transitions while a new worker
   could claim and re-run the same step. That is concurrent replay, not the
   sequential replay `tests/fault_injection.rs` models, and two `materialize`
   calls racing in the same directory is not something to introduce casually into
   a system whose selling point is that killing it is safe.

   Therefore: `stop()` awaits the `JoinHandle`, unbounded. **Never `abort()`**,
   and never a timeout-then-abort — two workers sharing a `worker.owner` is the
   exact hazard the lease design exists to prevent.

6. **Make the stop signal reachable inside a tick.** `RepairWorker::run`'s
   `select!` has `self.tick().await` in a branch body, so shutdown cannot preempt
   it: a stop currently waits for a whole batch of up to `batch_size` jobs, each
   driven up to `PROGRESSION.len() * 2` steps. Pass the `watch::Receiver` into
   `tick` and check it between claimed jobs and between `drive_inner` iterations.
   A stop then lands after at most one step.

   One step is still unbounded — a 40 GB copy in `stage` — but it is the smallest
   unit the architecture admits, and it is exactly the granularity crash recovery
   already assumes. Log the elapsed time when a stop took more than a second or
   two, so a slow save is honest rather than mysterious.

7. **`Diagnostics::reseed`**, about fifteen lines: update which trackers are
   stubs and drop entries for trackers no longer configured, keeping the poll
   history of those that remain. Without it, `/status` either forgets everything
   on a save or keeps claiming a removed tracker exists.

8. **`AppState`** becomes `{ runtime: Arc<RuntimeHandle>, bind_address:
   SocketAddr }`. Every handler gains `let runtime = state.runtime.current();`
   and `state.deps` becomes `runtime.deps`. The pure render helpers that take
   `&AppState` today take `&Runtime` instead, which makes "one request, one
   generation" visible in the signatures. `require_auth_token` reads
   `state.runtime.current().auth_token`, so a token set through the UI takes
   effect on the very next request.

   `bind_address` is what the process is actually listening on, fixed for its
   lifetime, so a `server.bind_address` change can be *reported* as needing a
   restart rather than silently ignored.

9. **Test seam.** Add `RuntimeHandle::fixed(Runtime) -> Arc<Self>`, documented as
   "nothing to reload, no worker; for tests". `support::router` stays a thin
   wrapper over it, so every existing web test exercises the new state type
   unchanged.

10. **`main.rs`** becomes: load config → `bootstrap::open` → `RuntimeHandle::start`
    (reconciles and spawns) → bind → serve → `handle.stop_worker().await`. The
    `watch` channel and `JoinHandle` currently owned by `main` move into
    `RuntimeHandle`.

11. **Refuse the changes that cannot be applied safely**, before writing
    anything. These are refusals, not warnings:

    | Change | Rule |
    |---|---|
    | `staging.root`, while any unfinished job has a `staging_dir` | **Refuse.** `staging_dir` is an `Option<SafeRelativePath>` resolved against the *current* root, so changing the root silently relocates every live job: reconciliation finds the plan absent under the new root and re-stages, orphaning the old directory forever with nothing that will ever delete it, while the client keeps seeding the old bytes. "Abandon and discard" would then delete the new directory while the client seeds the old one — the precise aliasing danger the review actions are written to avoid. |
    | Removing or renaming a tracker `id` with unfinished jobs | **Refuse**, naming the count. `UNIQUE (tracker_id, tracker_torrent_id)` means a rename does not orphan the old job so much as create a *second* job for the same torrent under the new id, while the old one parks unable to find its tracker. Two jobs, one info-hash, two staging directories, and an `add_torrent` that is contractually a success for a hash that already exists. |
    | Narrowing `library.roots` while unfinished jobs exist | **Refuse.** Removing a root and pointing `staging.root` inside it is self-consistent and passes every existing check, and SeedMedic then writes inside the media tree. |
    | `worker.owner` | **Refuse** while any job is leased. `clear_stale_leases` keys on the owner, so a change leaves the old owner's jobs locked until their leases expire, and would let one process steal a live peer's leases. |
    | `database.path`, `server.bind_address` | Written, reported as `RestartNeeded { keys }`, not applied. |

12. **Validate without side effects.** `StagingRoot::new` calls `create_dir_all`,
    so it must not be used to check a candidate path — that would create a
    directory for every typo, and it violates the rule that validation may read
    the filesystem but never write to it. Check with `check_overlap` plus the
    writability probe, and construct the real `StagingRoot` only on commit.

13. **Amend `docs/todos/0011`** in place: the resolution against config reloading
    is now superseded, with the reason (configuration moved into the browser).
    Add a "Why configuration is reloadable" section to `docs/architecture.md`
    next to "Why a durable state machine", covering build-before-stop and
    one-generation-per-worker. Update `AGENTS.md`'s layout listing and its
    "validated once in `Config::validate`" claim.

## Invariants and safety constraints

- **Build before stop.** Nothing is stopped until the replacement exists.
- **The old worker is awaited, never aborted**, and `reconcile_on_startup` runs
  only after it has exited. Two workers with the same owner must never be alive
  at once.
- **One generation per worker task.** A worker keeps the `Arc<Runtime>` it was
  spawned with for its whole life, so "which configuration was this step running
  under" is answerable from the audit trail. Rejected alternative, recorded so it
  is not re-litigated: having the worker re-read the current runtime each tick is
  a smaller diff but destroys `RepairDeps`'s stated immutability and makes that
  question unanswerable.
- **`Persistent` never changes.** The store, clock, worker health, diagnostics
  and metrics counters outlive every reload. Counters that reset on every save
  are not counters.
- **Reconciliation still only moves jobs backwards.** A reload does not get to
  advance anything.
- **A reload never widens what a repair may touch.** The refusals in step 11 are
  refusals.
- Only one SQLite pool is ever open on the database.

## Likely files

- `src/runtime.rs` (new)
- `src/bootstrap.rs`, `src/main.rs`
- `src/repair/worker.rs` (shutdown inside `tick`)
- `src/diagnostics.rs` (`reseed`)
- `src/web/mod.rs` and every page module (`AppState`, `&Runtime`)
- `tests/support/mod.rs`
- `docs/todos/0011-configuration-and-secrets.md`, `docs/architecture.md`,
  `AGENTS.md`

## Required tests

Integration, `tests/reload.rs`:

- `current()` returns the new runtime after a successful reload, and the **old**
  one after a failed build.
- A failed reload leaves the worker ticking — assert `worker_health.last_tick()`
  advances *after* the failure, not merely that it is non-`None`.
- `/health` is 200 immediately after a reload, with no tick in between. This is
  the `Persistent` regression test.
- Two concurrent reloads serialise, and exactly one worker is alive at the end.
- `Diagnostics` keeps history for a still-configured tracker and drops a removed
  one.
- **A reload reconciles**: drive a job to `injected`, remove the torrent from the
  fake client, reload, assert the rewind. This is the test that proves a reload
  is more than a re-wire.
- Each refusal in step 11, asserting the config file is unchanged and the old
  runtime still serves.
- `database.path` and `server.bind_address` changes report `RestartNeeded` and are
  not applied.
- A stop completes without waiting for a whole batch — drive with a
  `batch_size` above one and assert the stop lands after one step.

## Acceptance criteria

- `RuntimeHandle::reload()` swaps every config-derived adapter with no restart.
- A reload that cannot be brought up is invisible to everything except its
  caller.
- `/health` never dips because of a settings change.
- No reload can relocate, orphan, or alias the data of a live job.

## Out of scope

- Any UI. The reload is exercised by tests calling `reload()` directly; 0017 adds
  the caller.
- Watching the config file for external edits. Reload is triggered by a save.
- Rebinding a live listener.
- Multiple concurrent workers. Still exactly one per process.

## Open questions

- Should a slow reload return immediately with an "applying…" page that polls?

  **Resolved:** not yet. With the shutdown check inside `tick`, the common case
  is sub-millisecond because the worker is asleep in `interval.tick()`. The worst
  case is one step, which can be a large copy. Build the polling page when
  somebody reports a slow save; until then it is a second state and a page that
  can lie, bought with no evidence. Do log a slow stop.

- Should the reload be driven by a supervisor task instead of inline in the
  request?

  **Resolved:** inline. The operator's request is the reload, so the outcome is
  the response — which is the whole reason the error message is useful. A
  supervisor needs a progress state, an error slot, and a moment where the page
  is wrong.

- Should `reconcile_on_startup` be renamed now that it also runs on reload?

  **Resolved:** no. A rename churns six test call sites and buys nothing; add a
  line to its doc comment saying it also runs on reload, and why that is the same
  situation.

- Is `arc-swap` worth a dependency?

  **Resolved:** no. Reads are a handful per request. `std::sync::RwLock` with a
  `current()` that clones the `Arc` and drops the guard has the same safety
  property and no new crate.
