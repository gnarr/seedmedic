//! The swappable runtime: reload configuration without restarting the process.
//!
//! Not a new mechanism — the existing startup sequence, run again, against
//! durable state that was designed for exactly this: every side effect is
//! idempotent, every transition is a compare-and-swap, and
//! `reconcile_on_startup` only ever walks a job backwards. So "stop the
//! worker, rewire the adapters, reconcile, start a new worker" is
//! indistinguishable from a restart from the state machine's point of view.
//! See `docs/todos/0016-a-swappable-runtime.md`.
//!
//! Three things must never be rebuilt by a reload:
//! [`crate::repair::WorkerHealth`] (or `/health` dips after every settings
//! save), [`crate::diagnostics::Diagnostics`] (or an operator loses the tracker
//! error history they were looking at when they changed a setting), and
//! [`crate::events::EventBus`] (or every open event stream is dropped by the
//! very save it was watching). All three live in `bootstrap::Persistent`, which
//! [`RuntimeHandle`] holds for the life of the process and never touches again
//! after [`RuntimeHandle::start`].

use std::{
    collections::HashSet,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use thiserror::Error;
use tokio::sync::{Mutex, watch};
use tracing::{error, warn};

use crate::{
    bootstrap::{self, Persistent, Runtime},
    config::{Config, ConfigError, Secret},
    events::EventBus,
    repair::{RepairJob, WorkerConfig, reconcile::reconcile_on_startup, worker::RepairWorker},
};

#[derive(Debug, Error)]
pub enum ReloadError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// A change this reload would need to make cannot be applied safely —
    /// see step 11 of `docs/todos/0016-a-swappable-runtime.md`. The config
    /// file is left exactly as it was read; nothing here writes to it.
    #[error("{0}")]
    Refused(String),
    /// Building the new generation failed. The old runtime and its worker are
    /// untouched — build-before-stop is what makes this a pure no-op.
    #[error("{0:#}")]
    Build(anyhow::Error),
}

/// What a successful reload could and could not apply.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Applied {
    /// Keys the new file changed that this reload never applies —
    /// `database.path` and `server.bind_address` — because `Persistent` (the
    /// database connection) and the bound listener both outlive every reload
    /// by construction. Reported so a caller (0017's settings UI) can tell an
    /// operator a restart is still needed, rather than silently ignoring the
    /// change.
    pub restart_needed: Vec<&'static str>,
    /// Whether this reload changed `server.auth_token` — every browser
    /// session was just invalidated (see `RuntimeHandle::reload`), so the
    /// caller that changed it (0017's settings save) knows it must mint a
    /// fresh one for whoever just saved, or they are locked out of the page
    /// they saved from. See docs/todos/0018-browser-usable-authentication.md
    /// step 5.
    pub auth_token_changed: bool,
}

impl Applied {
    fn diff(old: &Config, new: &Config, auth_token_changed: bool) -> Self {
        let mut restart_needed = Vec::new();
        if new.database.path != old.database.path {
            restart_needed.push("database.path");
        }
        if new.server.bind_address != old.server.bind_address {
            restart_needed.push("server.bind_address");
        }
        Self {
            restart_needed,
            auth_token_changed,
        }
    }
}

/// Whether two optional tokens hold the same secret value. Comparing a
/// `Secret`'s plaintext is fine here — `RuntimeHandle` is not under
/// `src/web/`, and `bootstrap::build` already does the same to derive
/// `Runtime::auth_token` in the first place — but it exists nowhere a
/// browser response is built from.
fn auth_token_changed(old: &Option<Secret>, new: &Option<Secret>) -> bool {
    match (old, new) {
        (None, None) => false,
        (Some(old), Some(new)) => !old.verify(new.expose()),
        _ => true,
    }
}

/// One process's worker task: the `JoinHandle` and the sender that tells it to
/// stop, held together so there is exactly one place that owns them.
struct WorkerTask {
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

impl WorkerTask {
    fn spawn(runtime: &Arc<Runtime>, config: WorkerConfig) -> Self {
        let (shutdown, receiver) = watch::channel(false);
        let worker = RepairWorker::new(runtime.deps.clone(), config);
        let handle = tokio::spawn(worker.run(receiver));
        Self { shutdown, handle }
    }

    /// Signal the worker to stop and wait for it to actually exit. Never
    /// `abort()`, and never a timeout-then-abort: two workers sharing a lease
    /// owner is the exact hazard the lease design exists to prevent, and only
    /// an awaited exit guarantees this one is gone before reconciliation runs.
    async fn stop(self) {
        let _ = self.shutdown.send(true);
        let started = Instant::now();
        if let Err(error) = self.handle.await {
            error!(%error, "repair worker task panicked while stopping");
        }
        let elapsed = started.elapsed();
        if elapsed > Duration::from_secs(1) {
            warn!(
                ?elapsed,
                "stopping the repair worker took longer than expected"
            );
        }
    }
}

/// The live, swappable generation, plus the machinery to replace it.
pub struct RuntimeHandle {
    current: RwLock<Arc<Runtime>>,
    /// Held for the whole of a reload, so two saves serialise instead of one
    /// interleaving its stop with the other's respawn. Holds the worker task
    /// so there is exactly one place that owns it. `None` only between a
    /// worker being stopped and its replacement being spawned — which never
    /// spans an `await` a reader could observe, and `None` forever on a
    /// handle built with `fixed`, which has no worker at all.
    worker: Mutex<Option<WorkerTask>>,
    persistent: Persistent,
    config_path: PathBuf,
    /// Live browser session ids — see docs/todos/0018-browser-usable-authentication.md
    /// step 3. Lives here rather than on `Runtime` because it must survive a
    /// reload; cleared on process restart (nothing here is persisted) and
    /// whenever `server.auth_token` changes (`Self::reload`), never on any
    /// other configuration change.
    sessions: std::sync::Mutex<HashSet<String>>,
    /// Bumped by every successful reload.
    ///
    /// Exists for one caller: the event stream, which is the single documented
    /// exception to one-generation-per-request (see `src/web/AGENTS.md`). A
    /// long-lived stream re-reads `current()` per emit, and this is how it tells
    /// a client "the adapters under me were replaced — refetch rather than
    /// trusting what you hold".
    generation: std::sync::atomic::AtomicU64,
}

impl RuntimeHandle {
    /// Wire the first generation from an already-opened [`Persistent`],
    /// reconcile, and spawn its worker. Called once per process, with
    /// `persistent` fresh from [`bootstrap::open`]; every generation after
    /// this comes from [`Self::reload`], which never opens the database
    /// again.
    pub async fn start(
        config: &Config,
        persistent: Persistent,
        config_path: PathBuf,
    ) -> anyhow::Result<Arc<Self>> {
        let (runtime, worker_config) = bootstrap::build(config, &persistent, &config_path)?;
        let runtime = Arc::new(runtime);

        reconcile_on_startup(&runtime.deps, &worker_config.owner).await;
        let worker = WorkerTask::spawn(&runtime, worker_config);

        Ok(Arc::new(Self {
            current: RwLock::new(runtime),
            worker: Mutex::new(Some(worker)),
            persistent,
            config_path,
            sessions: std::sync::Mutex::new(HashSet::new()),
            generation: std::sync::atomic::AtomicU64::new(0),
        }))
    }

    /// Nothing to reload, no worker; for tests that only exercise the web
    /// layer over a `Runtime` they built by hand.
    pub fn fixed(runtime: Runtime) -> Arc<Self> {
        let persistent = Persistent {
            store: runtime.deps.store.clone(),
            clock: runtime.deps.clock.clone(),
            worker_health: runtime.deps.worker_health.clone(),
            diagnostics: runtime.deps.diagnostics.clone(),
            events: runtime.deps.events.clone(),
            #[cfg(feature = "metrics")]
            metrics: runtime.deps.metrics.clone(),
        };
        Arc::new(Self {
            current: RwLock::new(Arc::new(runtime)),
            worker: Mutex::new(None),
            persistent,
            config_path: PathBuf::new(),
            sessions: std::sync::Mutex::new(HashSet::new()),
            generation: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// One read lock, one `Arc` clone. Deliberately never hands out the
    /// guard, so there is no way to hold the lock across an `await`.
    pub fn current(&self) -> Arc<Runtime> {
        self.current.read().expect("runtime lock poisoned").clone()
    }

    /// Where `reload` reads from — what `/settings` opens as a
    /// [`crate::config::ConfigDocument`] to edit.
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// The live feed.
    ///
    /// Reached through the handle rather than through `current().deps` so a
    /// long-lived subscriber — an event stream — never has to hold a `Runtime`
    /// that a reload has since replaced. The bus lives on `Persistent`, so this
    /// is the same object across every generation.
    pub fn events(&self) -> Arc<EventBus> {
        self.persistent.events.clone()
    }

    /// How many times configuration has been reloaded. See the field's docs.
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Mint a fresh session, record it as live, and return its id — the
    /// value `/login` (or a settings save that just changed the token) puts
    /// in the cookie. A random id, not the token itself: see step 3 of
    /// docs/todos/0018-browser-usable-authentication.md for why.
    pub fn create_session(&self) -> String {
        let id = random_session_id();
        self.sessions
            .lock()
            .expect("session lock poisoned")
            .insert(id.clone());
        id
    }

    /// Whether `id` names a live session — the middleware's cookie check.
    pub fn has_session(&self, id: &str) -> bool {
        self.sessions
            .lock()
            .expect("session lock poisoned")
            .contains(id)
    }

    /// Forget one session — `POST /logout`. Removing an id that was never
    /// live (or already removed) is a no-op, not an error.
    pub fn destroy_session(&self, id: &str) {
        self.sessions
            .lock()
            .expect("session lock poisoned")
            .remove(id);
    }

    /// Replace every config-derived adapter with a fresh generation built
    /// from the file at `config_path`, in one step, with no window in which
    /// some adapters are old and some are new.
    ///
    /// Build-before-stop: both the config load and `bootstrap::build` can
    /// fail, and both return here with the old runtime installed and the old
    /// worker still ticking, so a reload that cannot be brought up changes
    /// nothing observable but its own error.
    pub async fn reload(&self) -> Result<Applied, ReloadError> {
        let mut worker = self.worker.lock().await;

        let new_config = Config::load_from(&self.config_path)?;
        let old_runtime = self.current();

        let unfinished = self.persistent.store.unfinished().await.map_err(|error| {
            ReloadError::Refused(format!(
                "could not check in-flight repairs before reloading: {error}"
            ))
        })?;
        check_refusals(
            &old_runtime.config,
            &new_config,
            &unfinished,
            &self.persistent,
        )
        .await?;

        let (runtime, worker_config) =
            bootstrap::build(&new_config, &self.persistent, &self.config_path)
                .map_err(ReloadError::Build)?;
        let runtime = Arc::new(runtime);

        // Only now is it safe to stop the old worker. Awaited above in
        // `WorkerTask::stop`; never aborted.
        if let Some(old) = worker.take() {
            old.stop().await;
        }

        // Same situation as a restart: new adapters over durable state, and
        // reality may have moved on since the old adapters last checked it.
        reconcile_on_startup(&runtime.deps, &worker_config.owner).await;

        let token_changed = auth_token_changed(&old_runtime.auth_token, &runtime.auth_token);

        *self.current.write().expect("runtime lock poisoned") = runtime.clone();
        *worker = Some(WorkerTask::spawn(&runtime, worker_config));

        // A leaked or shared session must not survive the secret it was
        // trusted under changing — see the invariants in
        // docs/todos/0018-browser-usable-authentication.md.
        if token_changed {
            self.sessions.lock().expect("session lock poisoned").clear();
        }

        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let applied = Applied::diff(&old_runtime.config, &new_config, token_changed);

        // Published from the bus on `Persistent`, which this reload did not
        // replace — so a client watching the save that caused it is still
        // connected to hear about it. That is the whole reason the bus is not on
        // `Runtime`; see `crate::events`.
        self.persistent
            .events
            .publish(crate::events::EventKind::ConfigReloaded {
                restart_needed: applied.restart_needed.clone(),
            });
        if token_changed {
            // Every *other* tab's cookie just died. Without this they discover
            // it by silently 401ing on their next action instead of showing the
            // login screen.
            self.persistent
                .events
                .publish(crate::events::EventKind::AuthTokenChanged);
        }

        Ok(applied)
    }

    /// Stop the worker for good, at process shutdown. Never followed by
    /// another reload — `main.rs` calls this once, after the web server has
    /// already stopped serving.
    pub async fn stop_worker(&self) {
        if let Some(worker) = self.worker.lock().await.take() {
            worker.stop().await;
        }
    }
}

/// 32 bytes from the OS, hex-encoded — a session id, not a secret derived
/// from anything guessable. `/dev/urandom` rather than a `rand` dependency:
/// this is the only place SeedMedic needs randomness, and reading a device
/// file is five lines against a crate and its transitive dependencies.
fn random_session_id() -> String {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .expect("reading /dev/urandom");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Refuse a change this reload cannot apply safely. Checked before anything
/// is built or written — these are refusals, not warnings; see step 11 of
/// `docs/todos/0016-a-swappable-runtime.md`.
async fn check_refusals(
    old: &Config,
    new: &Config,
    unfinished: &[RepairJob],
    persistent: &Persistent,
) -> Result<(), ReloadError> {
    // `staging_dir` is resolved against the *current* root; changing the root
    // silently relocates every job that already has one, orphaning the old
    // directory forever with nothing that will ever delete it. Exempt when
    // the *old* root was itself unset: `UnconfiguredStaging` cannot write
    // anywhere, so a job carrying a planned `staging_dir` from before
    // `staging.root` was ever set has nothing staged to orphan — and this is
    // exactly the settings page's fresh-install path (docs/todos/0017).
    if !old.staging.root.as_os_str().is_empty() && new.staging.root != old.staging.root {
        let staged = unfinished
            .iter()
            .filter(|job| job.staging_dir.is_some())
            .count();
        if staged > 0 {
            return Err(ReloadError::Refused(format!(
                "staging.root cannot change while {staged} unfinished repair(s) have data \
                 staged under the current root; finish or abandon them first"
            )));
        }
    }

    // `UNIQUE (tracker_id, tracker_torrent_id)` means removing or renaming a
    // tracker id does not orphan its jobs so much as create a second job for
    // the same torrent under the new id, while the old one parks unable to
    // find its tracker.
    let old_ids: HashSet<&str> = old.trackers.iter().map(|t| t.id.as_str()).collect();
    let new_ids: HashSet<&str> = new.trackers.iter().map(|t| t.id.as_str()).collect();
    for removed in old_ids.difference(&new_ids) {
        let affected = unfinished
            .iter()
            .filter(|job| job.tracker.as_str() == *removed)
            .count();
        if affected > 0 {
            return Err(ReloadError::Refused(format!(
                "tracker `{removed}` cannot be removed or renamed while it has {affected} \
                 unfinished repair(s)"
            )));
        }
    }

    // Removing a library root and pointing `staging.root` inside it is
    // self-consistent and passes every existing check, and SeedMedic would
    // then write inside the media tree.
    if !unfinished.is_empty() {
        let new_roots: HashSet<&Path> = new.library.roots.iter().map(PathBuf::as_path).collect();
        let narrowed = old
            .library
            .roots
            .iter()
            .any(|root| !new_roots.contains(root.as_path()));
        if narrowed {
            return Err(ReloadError::Refused(format!(
                "library.roots cannot be narrowed while {} unfinished repair(s) exist",
                unfinished.len()
            )));
        }
    }

    // `clear_stale_leases` keys on the owner; changing it out from under a
    // leased job would leave that job locked until its lease expires, and
    // would let a new process steal a live peer's leases.
    if new.worker.owner != old.worker.owner {
        let leased = persistent.store.has_active_lease().await.map_err(|error| {
            ReloadError::Refused(format!("could not check active leases: {error}"))
        })?;
        if leased {
            return Err(ReloadError::Refused(
                "worker.owner cannot change while a repair is leased".to_owned(),
            ));
        }
    }

    Ok(())
}
