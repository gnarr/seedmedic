//! The operator UI's live feed.
//!
//! Fan-out only, and deliberately so: nothing in the repair workflow may come
//! to depend on what is published here, exactly as with [`crate::diagnostics`].
//! A dropped event costs a stale panel until the next refetch. It must never
//! cost a wrong decision, and a `publish` must never change what the worker
//! does next — which is why [`EventBus::publish`] takes `&self`, cannot fail,
//! and returns nothing.
//!
//! ## Why this lives on `bootstrap::Persistent`
//!
//! A config reload replaces the whole [`crate::bootstrap::Runtime`]. A channel
//! that lived there would drop every subscriber on every settings save — and
//! the moment an operator most wants the dashboard to be live is immediately
//! after they changed a setting. `Persistent` outlives every reload by
//! construction, which is the same reason [`crate::repair::WorkerHealth`] and
//! [`crate::diagnostics::Diagnostics`] are there.
//!
//! ## Why the payloads are hints
//!
//! An event says *what changed*, not *what it changed to* — beyond the two or
//! three strings that let a chip update without a round trip. Three reasons,
//! all structural:
//!
//! 1. **There is no single right shape.** A job that moves needs the list-row
//!    shape on the dashboard and the detail shape (files, history, legal
//!    actions) on its own page. Publishing both means extra queries on every
//!    transition, for subscribers that may be looking at neither.
//! 2. **Authorisation has one gate.** The bus is process-wide; a subscriber is a
//!    request. A hint sends the refetch back through the ordinary authenticated
//!    `GET`, so there is exactly one place that decides who may read what.
//! 3. **A hint cannot leak.** An event carrying settings values could carry a
//!    secret. One carrying `{seq}` structurally cannot — the same reasoning that
//!    gives [`crate::config::SecretSource`] no value-carrying variant.
//!
//! See `docs/todos/0021-a-react-operator-ui.md`.

use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use crate::repair::{JobId, RepairState};

/// How many events the channel holds for a subscriber that has fallen behind.
///
/// Hints are tens of bytes, so this is a few kilobytes and several seconds of
/// slack at any realistic rate. A subscriber that exceeds it gets
/// [`broadcast::error::RecvError::Lagged`], which the SSE handler turns into a
/// `gap` event rather than a silent hole — see `docs/todos/0021`.
const CAPACITY: usize = 256;

/// What changed. Every variant carries the monotonic `seq` that lets a client
/// tell "I missed something" from "nothing happened".
#[derive(Clone, Debug)]
pub enum EventKind {
    /// One job moved between states.
    JobTransitioned {
        job: JobId,
        from: RepairState,
        to: RepairState,
        /// `TransitionReason::as_str`, or `"discovered"` for the row a new job
        /// opens with.
        reason: &'static str,
    },
    /// Several jobs changed at once — a bulk action, or a discovery run that
    /// created rows. One event, not N: a 200-job bulk abandon must not push 200
    /// events through a 256-slot channel and lag every subscriber off it.
    JobsChanged { jobs: Vec<JobId> },
    /// Telemetry moved without a state change: seeding progress, a recheck
    /// percentage, the tracker's unknown-answer streak. A separate, cheaper name
    /// from `JobTransitioned` precisely so a client can throttle it hard — this
    /// one fires for every seeding job on every poll.
    JobProgress { job: JobId },
    /// A worker tick, a discovery run, or a reconciliation finished. The three
    /// summaries that were previously returned, logged, and thrown away.
    Activity(Activity),
    /// A tracker became reachable or unreachable.
    TrackersChanged,
    /// A config reload landed.
    ConfigReloaded { restart_needed: Vec<&'static str> },
    /// `server.auth_token` changed, so every browser session was just
    /// invalidated. Without this, every other open tab silently 401s on its
    /// next action instead of showing the login screen.
    AuthTokenChanged,
}

/// The most recent outcome of each of the worker's three periodic jobs.
///
/// Kept here rather than on `Runtime` for the same reason as the channel: a
/// settings save must not blank the activity panel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Activity {
    pub kind: ActivityKind,
    pub at: Option<DateTime<Utc>>,
    pub claimed: usize,
    pub advanced: usize,
    pub parked: usize,
    pub retrying: usize,
    pub rewound: usize,
    pub jobs_created: usize,
    pub trackers_failed: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ActivityKind {
    #[default]
    Tick,
    Discovery,
    Reconcile,
}

/// An event with its sequence number.
#[derive(Clone, Debug)]
pub struct Event {
    pub seq: u64,
    pub kind: EventKind,
}

/// The three most recent activity summaries, one per kind.
#[derive(Clone, Copy, Debug, Default)]
pub struct LatestActivity {
    pub tick: Option<Activity>,
    pub discovery: Option<Activity>,
    pub reconcile: Option<Activity>,
}

pub struct EventBus {
    sender: broadcast::Sender<Event>,
    /// Monotonic. Restarts at 0 on process start, deliberately: a new process
    /// has no history to resume into, and a `Last-Event-ID` carried over from a
    /// previous one must never look resumable.
    seq: AtomicU64,
    latest: Mutex<LatestActivity>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(CAPACITY);
        Self {
            sender,
            seq: AtomicU64::new(0),
            latest: Mutex::new(LatestActivity::default()),
        }
    }

    /// Announce a change. Never fails, never blocks, never returns anything the
    /// caller has to handle.
    pub fn publish(&self, kind: EventKind) {
        if let EventKind::Activity(activity) = &kind {
            self.record_activity(*activity);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        // `send` fails only when there are no subscribers, which is the normal
        // state of a headless instance. Dropped deliberately, and deliberately
        // not logged: a publish must be invisible to the workflow, including in
        // its logs.
        let _ = self.sender.send(Event { seq, kind });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    /// The sequence number of the last event published.
    ///
    /// A subscriber reads this *after* subscribing, so nothing can slip between
    /// the two and be reported as neither delivered nor missed.
    pub fn seq(&self) -> u64 {
        self.seq.load(Ordering::Relaxed)
    }

    pub fn latest_activity(&self) -> LatestActivity {
        *self.latest.lock().expect("event bus activity poisoned")
    }

    fn record_activity(&self, activity: Activity) {
        let mut latest = self.latest.lock().expect("event bus activity poisoned");
        match activity.kind {
            ActivityKind::Tick => latest.tick = Some(activity),
            ActivityKind::Discovery => latest.discovery = Some(activity),
            ActivityKind::Reconcile => latest.reconcile = Some(activity),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishing_with_no_subscriber_is_a_no_op_and_still_advances_seq() {
        let bus = EventBus::new();
        assert_eq!(bus.seq(), 0);

        bus.publish(EventKind::TrackersChanged);

        // The send failed — nobody is listening — and that must be invisible.
        // The sequence still advanced, so a subscriber that arrives later can
        // tell it missed something rather than believing nothing happened.
        assert_eq!(bus.seq(), 1);
    }

    #[tokio::test]
    async fn a_subscriber_receives_what_is_published_after_it_subscribed() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        bus.publish(EventKind::JobTransitioned {
            job: JobId(7),
            from: RepairState::Injected,
            to: RepairState::Rechecking,
            reason: "progress",
        });

        let event = receiver.try_recv().expect("delivered");
        assert_eq!(event.seq, 1);
        match event.kind {
            EventKind::JobTransitioned { job, from, to, .. } => {
                assert_eq!(job, JobId(7));
                assert_eq!(from, RepairState::Injected);
                assert_eq!(to, RepairState::Rechecking);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// The backpressure contract the SSE handler depends on: a subscriber that
    /// falls more than `CAPACITY` behind is told it lagged, and by how much,
    /// rather than being silently handed a gap.
    #[tokio::test]
    async fn a_subscriber_that_falls_behind_is_told_it_lagged() {
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        for _ in 0..(CAPACITY + 10) {
            bus.publish(EventKind::TrackersChanged);
        }

        let error = receiver.try_recv().expect_err("must report the gap");
        assert!(
            matches!(error, broadcast::error::TryRecvError::Lagged(missed) if missed >= 10),
            "expected Lagged, got {error:?}"
        );
    }

    #[test]
    fn the_latest_activity_of_each_kind_is_kept_separately() {
        let bus = EventBus::new();

        bus.publish(EventKind::Activity(Activity {
            kind: ActivityKind::Tick,
            advanced: 3,
            ..Activity::default()
        }));
        bus.publish(EventKind::Activity(Activity {
            kind: ActivityKind::Discovery,
            jobs_created: 2,
            ..Activity::default()
        }));

        let latest = bus.latest_activity();
        assert_eq!(latest.tick.expect("a tick was recorded").advanced, 3);
        assert_eq!(
            latest
                .discovery
                .expect("a discovery was recorded")
                .jobs_created,
            2
        );
        assert!(
            latest.reconcile.is_none(),
            "a kind that never happened must stay absent rather than reading as zero"
        );
    }
}
