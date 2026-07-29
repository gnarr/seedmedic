//! `RuntimeHandle::reload` — see `docs/todos/0016-a-swappable-runtime.md`.
//!
//! Exercises the real `bootstrap::open`/`bootstrap::build` wiring (not the
//! `support::Harness` shortcut, which never touches `RuntimeHandle`), because
//! the property under test is the reload itself: build-before-stop, no
//! `/health` dip, `Persistent` surviving, and the refusals that keep a reload
//! from relocating, orphaning, or aliasing a live job's data.

mod support;

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use chrono::{DateTime, Utc};
use seedmedic::{
    bootstrap,
    config::Config,
    repair::{
        self, Discovered, JobId, JobPatch, PlannedFile, RepairDeps, RepairJob, RepairState,
        RepairStore, RepairWorker, StoreError, Transition, TransitionRecord, TransitionUpdate,
        WorkerConfig,
    },
    runtime::{ReloadError, RuntimeHandle},
    tracker::{HitAndRun, TrackerId, TrackerTorrentId},
};
use tempfile::TempDir;
use tokio::sync::watch;
use tower::ServiceExt;

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    String::from_utf8(bytes.to_vec()).expect("utf8 body")
}

async fn status_code(router: axum::Router, path: &str) -> StatusCode {
    router
        .oneshot(Request::get(path).body(Body::empty()).expect("request"))
        .await
        .expect("response")
        .status()
}

fn hit_and_run(torrent_id: &str) -> HitAndRun {
    HitAndRun {
        tracker: TrackerId::new("alpha"),
        torrent_id: TrackerTorrentId::new(torrent_id),
        torrent_name: format!("Demo {torrent_id}"),
        info_hash: None,
        size_bytes: 100,
        deadline: None,
        observed_at: Utc::now(),
    }
}

/// What one test's `config.toml` looks like. A struct rather than string
/// templating so each test can change exactly the one setting it cares about.
#[derive(Clone)]
struct ConfigOpts {
    database_path: PathBuf,
    staging_root: PathBuf,
    library_roots: Vec<PathBuf>,
    tracker_ids: Vec<String>,
    worker_owner: String,
    bind_address: String,
    /// Long by default so the real background worker `RuntimeHandle::start`
    /// spawns does not race a test's manual `store.apply` calls: only its
    /// one guaranteed-immediate first tick fires (see `TestEnv::start`),
    /// and that lands before any job exists to claim. Tests that need to
    /// observe real, repeated ticking set this short instead.
    poll_interval_seconds: u64,
    auth_token: Option<String>,
}

impl ConfigOpts {
    fn defaults(env: &TestEnv) -> Self {
        Self {
            database_path: env.db_path.clone(),
            staging_root: env.staging_path.clone(),
            library_roots: vec![env.library_path.clone()],
            tracker_ids: vec!["alpha".to_owned()],
            worker_owner: "primary".to_owned(),
            bind_address: "127.0.0.1:0".to_owned(),
            poll_interval_seconds: 3600,
            auth_token: None,
        }
    }

    fn render(&self) -> String {
        let roots = self
            .library_roots
            .iter()
            .map(|root| format!("\"{}\"", root.display()))
            .collect::<Vec<_>>()
            .join(", ");
        let trackers = self
            .tracker_ids
            .iter()
            .map(|id| format!("[[trackers]]\nid = \"{id}\"\nkind = \"fake\"\n"))
            .collect::<Vec<_>>()
            .join("\n");

        let auth_token = self
            .auth_token
            .as_ref()
            .map(|token| format!("auth_token = \"{token}\"\n"))
            .unwrap_or_default();

        format!(
            r#"
[server]
bind_address = "{bind}"
{auth_token}
[database]
path = "{db}"

[staging]
root = "{staging}"

[library]
roots = [{roots}]

[worker]
owner = "{owner}"
poll_interval_seconds = {poll_interval}
discovery_interval_seconds = 3600

{trackers}

[download_client]
kind = "fake"
"#,
            bind = self.bind_address,
            db = self.database_path.display(),
            staging = self.staging_root.display(),
            owner = self.worker_owner,
            poll_interval = self.poll_interval_seconds,
        )
    }
}

/// A working directory with a database path, a staging root, and a library
/// root already created, plus the `config.toml` reload reads from.
struct TestEnv {
    dir: TempDir,
    config_path: PathBuf,
    db_path: PathBuf,
    staging_path: PathBuf,
    library_path: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let staging_path = dir.path().join("staging");
        let library_path = dir.path().join("library");
        std::fs::create_dir_all(&staging_path).expect("staging dir");
        std::fs::create_dir_all(&library_path).expect("library dir");

        Self {
            config_path: dir.path().join("config.toml"),
            db_path: dir.path().join("seedmedic.db"),
            staging_path,
            library_path,
            dir,
        }
    }

    fn write_config(&self, toml_text: &str) {
        std::fs::write(&self.config_path, toml_text).expect("write config");
    }

    /// Write the default config and start a `RuntimeHandle` over it — the
    /// same sequence `main.rs` runs: open, build, reconcile, spawn.
    async fn start(&self) -> Arc<RuntimeHandle> {
        self.start_with(ConfigOpts::defaults(self)).await
    }

    /// Like [`Self::start`], but with a caller-chosen `config.toml`.
    async fn start_with(&self, opts: ConfigOpts) -> Arc<RuntimeHandle> {
        self.write_config(&opts.render());
        let config = Config::load_from(&self.config_path).expect("valid config");
        let persistent = bootstrap::open(&config)
            .await
            .expect("open persistent state");
        let handle = RuntimeHandle::start(&config, persistent, self.config_path.clone())
            .await
            .expect("start");

        // `tokio::time::interval`'s first tick fires immediately regardless
        // of `poll_interval`. Let it land — finding nothing, since no test
        // job exists yet — before returning, so a long `poll_interval`
        // genuinely means "the background worker will not touch what this
        // test is about to set up."
        tokio::time::sleep(Duration::from_millis(50)).await;
        handle
    }

    /// Like [`Self::start`], but immediately stops the worker `start` just
    /// spawned — for tests that manipulate job state directly via
    /// `store.apply`/`store.claim` and must not race a live worker claiming
    /// and driving the very rows they are setting up. `reload()` still works
    /// afterward exactly as documented: it tolerates there being no old
    /// worker to stop, and spawns a fresh one at the end regardless.
    async fn start_alone(&self) -> Arc<RuntimeHandle> {
        self.start_alone_with(ConfigOpts::defaults(self)).await
    }

    /// Like [`Self::start_alone`], but with a caller-chosen `config.toml`.
    async fn start_alone_with(&self, opts: ConfigOpts) -> Arc<RuntimeHandle> {
        let handle = self.start_with(opts).await;
        handle.stop_worker().await;
        handle
    }
}

/// Drive a job forward with `Progress` transitions until it reaches `target`,
/// patching in whatever a real step would have recorded along the way — just
/// enough for reload's refusal checks and reconciliation to see a job in the
/// state a test needs, without going through the real tracker/matching/staging
/// pipeline.
async fn advance_to(store: &Arc<dyn RepairStore>, id: JobId, target: RepairState) -> RepairJob {
    let mut job = store.job(id).await.expect("lookup").expect("job exists");
    while job.state != target {
        let transition = job.advance().expect("can advance");
        let patch = match transition.to() {
            RepairState::TorrentFetched => JobPatch {
                info_hash: Some(seedmedic::torrent::InfoHash::from_bytes([7; 20])),
                ..JobPatch::default()
            },
            RepairState::Matched => JobPatch {
                staging_dir: Some(job.default_staging_dir()),
                ..JobPatch::default()
            },
            _ => JobPatch::default(),
        };
        store
            .apply(id, transition, TransitionUpdate::default().patch(patch))
            .await
            .expect("apply");
        job = store.job(id).await.expect("lookup").expect("job exists");
    }
    job
}

/// Attempt a reload expected to be refused, and check the two invariants every
/// refusal must uphold: the config file is untouched, and the runtime in
/// service is still the exact instance from before the attempt.
async fn assert_refused(
    handle: &RuntimeHandle,
    config_path: &Path,
    old_runtime: &Arc<bootstrap::Runtime>,
) -> String {
    let before = std::fs::read(config_path).expect("read config");
    let error = handle
        .reload()
        .await
        .expect_err("this reload must be refused");
    let message = match error {
        ReloadError::Refused(message) => message,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let after = std::fs::read(config_path).expect("read config");
    assert_eq!(
        before, after,
        "a refused reload must not touch the config file"
    );
    assert!(
        Arc::ptr_eq(old_runtime, &handle.current()),
        "a refused reload must not swap the runtime"
    );
    message
}

#[tokio::test]
async fn a_successful_reload_swaps_the_runtime_and_a_failed_build_does_not() {
    let env = TestEnv::new();
    let handle = env.start().await;
    let old_runtime = handle.current();

    let mut opts = ConfigOpts::defaults(&env);
    opts.tracker_ids.push("beta".to_owned());
    env.write_config(&opts.render());

    let applied = handle
        .reload()
        .await
        .expect("a benign change must reload cleanly");
    assert!(applied.restart_needed.is_empty(), "{applied:?}");
    assert!(
        !Arc::ptr_eq(&old_runtime, &handle.current()),
        "a successful reload must install a new runtime"
    );

    // Force a *build* failure specifically (not a Config::load_from one):
    // a staging.root that exists as a regular file passes the cheap checks
    // problems_on_disk runs, but StagingRoot::new's create_dir_all fails on
    // it, since a directory cannot be created where a file already is.
    let not_a_directory = env.dir.path().join("staged-here-is-a-file");
    std::fs::write(&not_a_directory, b"not a directory").expect("write file");
    let mut broken = ConfigOpts::defaults(&env);
    broken.tracker_ids.push("beta".to_owned());
    broken.staging_root = not_a_directory;
    env.write_config(&broken.render());

    let before_runtime = handle.current();
    let error = handle
        .reload()
        .await
        .expect_err("an unusable staging root must fail the build");
    assert!(matches!(error, ReloadError::Build(_)), "{error:?}");
    assert!(
        Arc::ptr_eq(&before_runtime, &handle.current()),
        "a failed build must leave the previous runtime installed"
    );
}

#[tokio::test]
async fn a_failed_reload_leaves_the_worker_ticking() {
    let env = TestEnv::new();
    let mut opts = ConfigOpts::defaults(&env);
    opts.poll_interval_seconds = 1;
    let handle = env.start_with(opts).await;

    tokio::time::sleep(Duration::from_millis(200)).await;
    let tick_before = handle
        .current()
        .deps
        .worker_health
        .last_tick()
        .expect("the worker has ticked at least once by now");

    let not_a_directory = env.dir.path().join("staged-here-is-a-file");
    std::fs::write(&not_a_directory, b"not a directory").expect("write file");
    let mut broken = ConfigOpts::defaults(&env);
    broken.poll_interval_seconds = 1;
    broken.staging_root = not_a_directory;
    env.write_config(&broken.render());

    handle.reload().await.expect_err("must fail");

    tokio::time::sleep(Duration::from_millis(1300)).await;
    let tick_after = handle
        .current()
        .deps
        .worker_health
        .last_tick()
        .expect("still ticking");
    assert!(
        tick_after > tick_before,
        "the old worker must keep ticking after a failed reload: before={tick_before:?} \
         after={tick_after:?}"
    );
}

#[tokio::test]
async fn health_does_not_dip_immediately_after_a_reload() {
    let env = TestEnv::new();
    let handle = env.start().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let router = seedmedic::web::router(handle.clone(), "127.0.0.1:0".parse().expect("addr"));
    assert_eq!(
        status_code(router.clone(), "/health").await,
        StatusCode::OK,
        "must be healthy before the reload"
    );

    let mut opts = ConfigOpts::defaults(&env);
    opts.tracker_ids.push("beta".to_owned());
    env.write_config(&opts.render());
    handle.reload().await.expect("reload succeeds");

    // No sleep: this must not depend on a tick happening between the reload
    // finishing and this request — that is exactly what a rebuilt
    // `WorkerHealth` would break.
    assert_eq!(
        status_code(router, "/health").await,
        StatusCode::OK,
        "a settings save must never make /health dip"
    );
}

#[tokio::test]
async fn two_concurrent_reloads_serialise() {
    let env = TestEnv::new();
    let handle = env.start().await;

    let mut opts = ConfigOpts::defaults(&env);
    opts.tracker_ids.push("beta".to_owned());
    env.write_config(&opts.render());

    let (first, second) = tokio::join!(handle.reload(), handle.reload());
    assert!(first.is_ok(), "{first:?}");
    assert!(second.is_ok(), "{second:?}");

    // If two worker tasks were ever alive at once racing the same leases,
    // this would be where it shows up: the worker must simply keep working.
    tokio::time::sleep(Duration::from_millis(1300)).await;
    assert!(handle.current().deps.worker_health.last_tick().is_some());
}

#[tokio::test]
async fn a_reload_keeps_history_for_a_surviving_tracker_and_forgets_a_removed_one() {
    let env = TestEnv::new();
    let handle = env.start().await;

    let mut with_beta = ConfigOpts::defaults(&env);
    with_beta.tracker_ids.push("beta".to_owned());
    env.write_config(&with_beta.render());
    handle.reload().await.expect("reload succeeds");

    let runtime = handle.current();
    let alpha = TrackerId::new("alpha");
    let beta = TrackerId::new("beta");
    runtime
        .deps
        .diagnostics
        .record_tracker_success(&alpha, Utc::now());
    runtime
        .deps
        .diagnostics
        .record_tracker_success(&beta, Utc::now());

    env.write_config(&ConfigOpts::defaults(&env).render());
    handle.reload().await.expect("reload succeeds");

    assert!(
        handle
            .current()
            .deps
            .diagnostics
            .tracker_health(&alpha)
            .last_success
            .is_some(),
        "a surviving tracker must keep its poll history across a reload"
    );

    let router = seedmedic::web::router(handle.clone(), "127.0.0.1:0".parse().expect("addr"));
    let response = router
        .oneshot(
            Request::get("/status")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let body = body_text(response).await;
    assert!(body.contains("alpha"), "{body}");
    assert!(
        !body.contains("beta"),
        "a removed tracker must not still be listed: {body}"
    );
}

#[tokio::test]
async fn a_reload_reconciles_a_job_past_injected() {
    let env = TestEnv::new();
    let handle = env.start_alone().await;
    let runtime = handle.current();

    let discovered = runtime
        .deps
        .store
        .record_discovery(&hit_and_run("job-a"))
        .await
        .expect("discovery");
    let job = advance_to(&runtime.deps.store, discovered.id, RepairState::Injected).await;
    assert_eq!(job.state, RepairState::Injected);
    assert!(job.info_hash.is_some());

    // No config change needed: `bootstrap::build` always wires a fresh
    // `FakeTorrentClient`, so the next generation's client has never heard of
    // this info-hash — modelling "repointed at a different qBittorrent."
    handle.reload().await.expect("reload succeeds");

    let after = handle
        .current()
        .deps
        .store
        .job(job.id)
        .await
        .expect("lookup")
        .expect("job exists");
    assert_eq!(
        after.state,
        RepairState::Staged,
        "a reload must reconcile a job the new client has never heard of, not just re-wire \
         adapters"
    );
}

#[tokio::test]
async fn refuses_a_staging_root_change_while_a_job_has_data_staged() {
    let env = TestEnv::new();
    let handle = env.start_alone().await;
    let runtime = handle.current();

    let discovered = runtime
        .deps
        .store
        .record_discovery(&hit_and_run("job-a"))
        .await
        .expect("discovery");
    advance_to(&runtime.deps.store, discovered.id, RepairState::Matched).await;

    let new_staging = env.dir.path().join("other-staging");
    std::fs::create_dir_all(&new_staging).expect("mkdir");
    let mut opts = ConfigOpts::defaults(&env);
    opts.staging_root = new_staging;
    env.write_config(&opts.render());

    let message = assert_refused(&handle, &env.config_path, &runtime).await;
    assert!(message.contains("staging.root"), "{message}");
}

#[tokio::test]
async fn refuses_removing_a_tracker_with_unfinished_jobs() {
    let env = TestEnv::new();
    let handle = env.start_alone().await;
    let runtime = handle.current();

    runtime
        .deps
        .store
        .record_discovery(&hit_and_run("job-a"))
        .await
        .expect("discovery");

    let mut opts = ConfigOpts::defaults(&env);
    opts.tracker_ids = vec!["beta".to_owned()];
    env.write_config(&opts.render());

    let message = assert_refused(&handle, &env.config_path, &runtime).await;
    assert!(message.contains("alpha"), "{message}");
}

#[tokio::test]
async fn refuses_narrowing_library_roots_while_unfinished_jobs_exist() {
    let env = TestEnv::new();
    let handle = env.start_alone().await;
    let runtime = handle.current();

    runtime
        .deps
        .store
        .record_discovery(&hit_and_run("job-a"))
        .await
        .expect("discovery");

    let mut opts = ConfigOpts::defaults(&env);
    opts.library_roots = Vec::new();
    env.write_config(&opts.render());

    let message = assert_refused(&handle, &env.config_path, &runtime).await;
    assert!(message.contains("library.roots"), "{message}");
}

#[tokio::test]
async fn refuses_changing_worker_owner_while_a_job_is_leased() {
    let env = TestEnv::new();
    let handle = env.start_alone().await;
    let runtime = handle.current();

    let discovered = runtime
        .deps
        .store
        .record_discovery(&hit_and_run("job-a"))
        .await
        .expect("discovery");
    runtime
        .deps
        .store
        .claim("primary", Duration::from_secs(300), 10)
        .await
        .expect("claim");
    assert!(
        runtime
            .deps
            .store
            .job(discovered.id)
            .await
            .expect("lookup")
            .expect("job exists")
            .state
            .is_actionable()
    );

    let mut opts = ConfigOpts::defaults(&env);
    opts.worker_owner = "secondary".to_owned();
    env.write_config(&opts.render());

    let message = assert_refused(&handle, &env.config_path, &runtime).await;
    assert!(message.contains("worker.owner"), "{message}");
}

#[tokio::test]
async fn database_path_and_bind_address_changes_are_reported_but_never_applied() {
    let env = TestEnv::new();
    let handle = env.start_alone().await;
    let runtime = handle.current();

    let discovered = runtime
        .deps
        .store
        .record_discovery(&hit_and_run("job-a"))
        .await
        .expect("discovery");

    let mut opts = ConfigOpts::defaults(&env);
    opts.database_path = env.dir.path().join("a-different.db");
    opts.bind_address = "127.0.0.1:9999".to_owned();
    env.write_config(&opts.render());

    let applied = handle
        .reload()
        .await
        .expect("these two keys being un-applicable is not itself a refusal");
    assert!(
        applied.restart_needed.contains(&"database.path"),
        "{applied:?}"
    );
    assert!(
        applied.restart_needed.contains(&"server.bind_address"),
        "{applied:?}"
    );

    // Proof it truly was not applied: the job recorded through the *old*
    // database connection is still there through the "new" generation,
    // because `Persistent` — and the connection it holds — never changed.
    let still_there = handle
        .current()
        .deps
        .store
        .job(discovered.id)
        .await
        .expect("lookup");
    assert!(still_there.is_some());
}

#[tokio::test]
async fn a_session_is_live_until_destroyed() {
    let env = TestEnv::new();
    let handle = env.start().await;

    let id = handle.create_session();
    assert!(handle.has_session(&id));

    handle.destroy_session(&id);
    assert!(!handle.has_session(&id));
}

#[tokio::test]
async fn destroying_an_unknown_session_is_a_no_op() {
    let env = TestEnv::new();
    let handle = env.start().await;

    handle.destroy_session("never-issued");
    assert!(!handle.has_session("never-issued"));
}

/// See docs/todos/0018-browser-usable-authentication.md's invariant that a
/// session must not survive the secret it was trusted under changing.
#[tokio::test]
async fn changing_the_auth_token_invalidates_existing_sessions() {
    let env = TestEnv::new();
    let mut opts = ConfigOpts::defaults(&env);
    opts.auth_token = Some("old-token".to_owned());
    let handle = env.start_with(opts).await;

    let session = handle.create_session();
    assert!(handle.has_session(&session));

    let mut changed = ConfigOpts::defaults(&env);
    changed.auth_token = Some("new-token".to_owned());
    env.write_config(&changed.render());

    let applied = handle.reload().await.expect("reload succeeds");
    assert!(applied.auth_token_changed, "{applied:?}");
    assert!(
        !handle.has_session(&session),
        "a session minted under the old token must not survive its rotation"
    );
}

/// The other side of the invariant above: a reload that leaves the token
/// untouched must not sign anyone out.
#[tokio::test]
async fn a_reload_that_does_not_change_the_token_keeps_sessions_alive() {
    let env = TestEnv::new();
    let mut opts = ConfigOpts::defaults(&env);
    opts.auth_token = Some("same-token".to_owned());
    let handle = env.start_with(opts.clone()).await;

    let session = handle.create_session();

    let mut unrelated = opts.clone();
    unrelated.tracker_ids.push("beta".to_owned());
    env.write_config(&unrelated.render());

    let applied = handle.reload().await.expect("reload succeeds");
    assert!(!applied.auth_token_changed, "{applied:?}");
    assert!(handle.has_session(&session));
}

/// Setting a token where there was none, and clearing one that was set, both
/// count as a change — the `None`/`Some` boundary, not just value drift.
#[tokio::test]
async fn setting_or_clearing_the_token_also_invalidates_sessions() {
    let env = TestEnv::new();
    let handle = env.start().await; // no auth_token configured

    let session = handle.create_session();

    let mut opts = ConfigOpts::defaults(&env);
    opts.auth_token = Some("freshly-set".to_owned());
    env.write_config(&opts.render());

    let applied = handle.reload().await.expect("reload succeeds");
    assert!(applied.auth_token_changed, "{applied:?}");
    assert!(!handle.has_session(&session));
}

/// A [`RepairStore`] decorator that flips a `watch::Sender` to `true` the
/// moment its first `release` call returns — i.e. the instant one job's drive
/// has fully finished — so a shutdown signal can be raised deterministically
/// between two claimed jobs without any real-time race.
struct StopAfterFirstRelease {
    inner: Arc<dyn RepairStore>,
    shutdown: watch::Sender<bool>,
    released: AtomicUsize,
}

#[async_trait]
impl RepairStore for StopAfterFirstRelease {
    async fn record_discovery(&self, hit_and_run: &HitAndRun) -> Result<Discovered, StoreError> {
        self.inner.record_discovery(hit_and_run).await
    }

    async fn job(&self, id: JobId) -> Result<Option<RepairJob>, StoreError> {
        self.inner.job(id).await
    }

    async fn jobs(&self, limit: i64) -> Result<Vec<RepairJob>, StoreError> {
        self.inner.jobs(limit).await
    }

    async fn unfinished(&self) -> Result<Vec<RepairJob>, StoreError> {
        self.inner.unfinished().await
    }

    async fn parked(&self) -> Result<Vec<RepairJob>, StoreError> {
        self.inner.parked().await
    }

    async fn torrent_file(&self, id: JobId) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.torrent_file(id).await
    }

    async fn planned_files(&self, id: JobId) -> Result<Vec<PlannedFile>, StoreError> {
        self.inner.planned_files(id).await
    }

    async fn history(&self, id: JobId) -> Result<Vec<TransitionRecord>, StoreError> {
        self.inner.history(id).await
    }

    async fn set_review_resume_point(
        &self,
        id: JobId,
        state: RepairState,
    ) -> Result<(), StoreError> {
        self.inner.set_review_resume_point(id, state).await
    }

    async fn apply(
        &self,
        id: JobId,
        transition: Transition,
        update: TransitionUpdate,
    ) -> Result<repair::Applied, StoreError> {
        self.inner.apply(id, transition, update).await
    }

    async fn record_progress(&self, id: JobId, patch: JobPatch) -> Result<(), StoreError> {
        self.inner.record_progress(id, patch).await
    }

    async fn claim(
        &self,
        owner: &str,
        lease: Duration,
        limit: i64,
    ) -> Result<Vec<RepairJob>, StoreError> {
        self.inner.claim(owner, lease, limit).await
    }

    async fn release(
        &self,
        id: JobId,
        retry_at: Option<DateTime<Utc>>,
        count_attempt: bool,
    ) -> Result<(), StoreError> {
        let result = self.inner.release(id, retry_at, count_attempt).await;
        if self.released.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = self.shutdown.send(true);
        }
        result
    }

    async fn renew_lease(
        &self,
        id: JobId,
        owner: &str,
        lease: Duration,
    ) -> Result<bool, StoreError> {
        self.inner.renew_lease(id, owner, lease).await
    }

    async fn clear_stale_leases(&self, owner: &str) -> Result<u64, StoreError> {
        self.inner.clear_stale_leases(owner).await
    }

    async fn ping(&self) -> Result<(), StoreError> {
        self.inner.ping().await
    }

    async fn has_active_lease(&self) -> Result<bool, StoreError> {
        self.inner.has_active_lease().await
    }
}

/// A stop must land after at most one step, not after a whole batch —
/// `RepairWorker::run`'s shutdown check happens between claimed jobs and
/// between a job's own step iterations (see `src/repair/worker.rs`).
///
/// Two due jobs the fake tracker has never heard of (so each one's first step
/// is a quick, harmless retry), a `batch_size` of 4, and a store that raises
/// the shutdown signal itself the instant the first job's drive fully
/// finishes: if the fix were absent, the second job would still be driven in
/// the same tick; with it, the tick sees the signal and stops before touching
/// the second job at all.
#[tokio::test]
async fn a_stop_lands_after_one_step_not_a_whole_batch() {
    let env = TestEnv::new();
    // `start_alone` stops the handle's own worker so it cannot race this
    // test's separately-built one for a claim on job_a/job_b — but that
    // worker is briefly alive first, and with a configured tracker its own
    // discovery poll (which, like its work poll, fires once immediately no
    // matter how long `discovery_interval` is) would conjure up demo jobs of
    // its own in that window. Zero trackers means nothing for it to discover.
    let mut opts = ConfigOpts::defaults(&env);
    opts.tracker_ids = Vec::new();
    let handle = env.start_alone_with(opts).await;
    let runtime = handle.current();

    let job_a = runtime
        .deps
        .store
        .record_discovery(&hit_and_run("job-a"))
        .await
        .expect("discovery")
        .id;
    let job_b = runtime
        .deps
        .store
        .record_discovery(&hit_and_run("job-b"))
        .await
        .expect("discovery")
        .id;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let wrapped: Arc<dyn RepairStore> = Arc::new(StopAfterFirstRelease {
        inner: runtime.deps.store.clone(),
        shutdown: shutdown_tx,
        released: AtomicUsize::new(0),
    });

    let deps = Arc::new(RepairDeps {
        store: wrapped,
        trackers: runtime.deps.trackers.clone(),
        inspector: runtime.deps.inspector.clone(),
        candidate_sources: runtime.deps.candidate_sources.clone(),
        staging: runtime.deps.staging.clone(),
        client: runtime.deps.client.clone(),
        clock: runtime.deps.clock.clone(),
        policy: runtime.deps.policy,
        category: runtime.deps.category.clone(),
        worker_health: runtime.deps.worker_health.clone(),
        diagnostics: runtime.deps.diagnostics.clone(),
        client_is_stub: runtime.deps.client_is_stub,
        #[cfg(feature = "metrics")]
        metrics: runtime.deps.metrics.clone(),
        notifier: runtime.deps.notifier.clone(),
        tracker_unreachable_threshold: runtime.deps.tracker_unreachable_threshold,
    });
    let worker = RepairWorker::new(
        deps,
        WorkerConfig {
            owner: "batch-test".to_owned(),
            lease: Duration::from_secs(60),
            batch_size: 4,
            poll_interval: Duration::from_millis(50),
            discovery_interval: Duration::from_secs(3600),
        },
    );

    let task = tokio::spawn(worker.run(shutdown_rx));
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("worker.run must stop promptly, not wait for the whole batch")
        .expect("worker task must not panic");

    let a = runtime
        .deps
        .store
        .job(job_a)
        .await
        .expect("lookup")
        .expect("job exists");
    let b = runtime
        .deps
        .store
        .job(job_b)
        .await
        .expect("lookup")
        .expect("job exists");
    // Not `attempts`: with no tracker configured, the outcome is an
    // immediate `Review` park (the tracker being unconfigured is found
    // before any retryable error is), and every transition — parking
    // included — resets `attempts` to 0. `state` moving off `discovered` is
    // what proves a job was actually driven.
    let touched = usize::from(a.state != RepairState::Discovered)
        + usize::from(b.state != RepairState::Discovered);
    assert_eq!(
        touched, 1,
        "exactly one of the two claimed jobs may be touched before the stop lands: a={a:?} b={b:?}"
    );
}
