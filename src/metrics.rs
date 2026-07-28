//! In-process counters for `/metrics`. Present only when built with the
//! `metrics` feature, and only reachable when `metrics.enabled` is also set
//! in config — see `docs/todos/0012-observability.md`.
//!
//! Plain counters and a running average rather than a real histogram, and
//! JSON rather than a Prometheus exporter: both avoid a dependency the doc's
//! own open questions left unresolved, and this is explicitly a
//! nice-to-have, never something the repair workflow depends on.

use std::{collections::HashMap, sync::Mutex, time::Duration};

use serde::Serialize;

#[derive(Default)]
pub struct Metrics {
    transitions: Mutex<HashMap<(String, String), u64>>,
    step_durations: Mutex<HashMap<String, StepDurations>>,
    tracker_polls: Mutex<HashMap<(String, &'static str), u64>>,
}

#[derive(Default, Clone, Copy)]
struct StepDurations {
    count: u64,
    total: Duration,
}

impl Metrics {
    pub fn record_transition(&self, from: &str, to: &str) {
        *self
            .transitions
            .lock()
            .expect("metrics poisoned")
            .entry((from.to_owned(), to.to_owned()))
            .or_insert(0) += 1;
    }

    pub fn record_step_duration(&self, state: &str, elapsed: Duration) {
        let mut durations = self.step_durations.lock().expect("metrics poisoned");
        let entry = durations.entry(state.to_owned()).or_default();
        entry.count += 1;
        entry.total += elapsed;
    }

    /// `outcome` is a fixed set of literals at each call site (`"success"`,
    /// `"error"`), not user data, so `'static` is a real invariant, not just
    /// a convenient bound.
    pub fn record_tracker_poll(&self, tracker: &str, outcome: &'static str) {
        *self
            .tracker_polls
            .lock()
            .expect("metrics poisoned")
            .entry((tracker.to_owned(), outcome))
            .or_insert(0) += 1;
    }

    pub fn snapshot(&self) -> Snapshot {
        let transitions = self
            .transitions
            .lock()
            .expect("metrics poisoned")
            .iter()
            .map(|((from, to), count)| TransitionCount {
                from: from.clone(),
                to: to.clone(),
                count: *count,
            })
            .collect();

        let step_durations = self
            .step_durations
            .lock()
            .expect("metrics poisoned")
            .iter()
            .map(|(state, durations)| StepDurationSummary {
                state: state.clone(),
                count: durations.count,
                average_millis: average_millis(durations),
            })
            .collect();

        let tracker_polls = self
            .tracker_polls
            .lock()
            .expect("metrics poisoned")
            .iter()
            .map(|((tracker, outcome), count)| TrackerPollCount {
                tracker: tracker.clone(),
                outcome: (*outcome).to_owned(),
                count: *count,
            })
            .collect();

        Snapshot {
            transitions,
            step_durations,
            tracker_polls,
        }
    }
}

fn average_millis(durations: &StepDurations) -> u64 {
    if durations.count == 0 {
        0
    } else {
        (durations.total.as_millis() / u128::from(durations.count)) as u64
    }
}

#[derive(Serialize)]
pub struct Snapshot {
    pub transitions: Vec<TransitionCount>,
    pub step_durations: Vec<StepDurationSummary>,
    pub tracker_polls: Vec<TrackerPollCount>,
}

#[derive(Serialize)]
pub struct TransitionCount {
    pub from: String,
    pub to: String,
    pub count: u64,
}

#[derive(Serialize)]
pub struct StepDurationSummary {
    pub state: String,
    pub count: u64,
    pub average_millis: u64,
}

#[derive(Serialize)]
pub struct TrackerPollCount {
    pub tracker: String,
    pub outcome: String,
    pub count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_reports_a_transition() {
        let metrics = Metrics::default();
        metrics.record_transition("discovered", "torrent_fetched");
        metrics.record_transition("discovered", "torrent_fetched");

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.transitions.len(), 1);
        assert_eq!(snapshot.transitions[0].count, 2);
    }

    #[test]
    fn averages_step_durations() {
        let metrics = Metrics::default();
        metrics.record_step_duration("matched", Duration::from_millis(100));
        metrics.record_step_duration("matched", Duration::from_millis(300));

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.step_durations[0].count, 2);
        assert_eq!(snapshot.step_durations[0].average_millis, 200);
    }
}
