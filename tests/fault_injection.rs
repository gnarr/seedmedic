//! What happens when the process dies in the gap between a step's external
//! side effect and the transition that records it.
//!
//! `crash_recovery.rs` covers a handful of hand-picked moments. This file
//! covers all of them: every state a repair passes through on its way to
//! `Completed` is one crash point, generated from `RepairState::PROGRESSION`
//! rather than hand-listed, so a new step added to the lifecycle gets a crash
//! test automatically instead of by remembering to add one.
//!
//! `support::fail_at::FailAt` makes the underlying `apply` call fail exactly
//! once, at the point that would have recorded the crashed-at transition. The
//! next tick replays the step from scratch — which is exactly what a real
//! restart would also do, since nothing durable told it otherwise.

mod support;

use std::sync::Arc;

use seedmedic::repair::{RepairState, RepairStore};
use support::{Harness, fail_at::FailAt};

#[tokio::test]
async fn every_transition_survives_a_crash_between_its_side_effect_and_its_record() {
    for target in RepairState::PROGRESSION.into_iter().skip(1) {
        let fail_call = target.rank().expect("progression states rank");

        let harness = Harness::new().await;
        harness.discover().await;
        // The crash points under test are all before completion; clearing the
        // hit-and-run now means the tracker's answer is never in question by
        // the time the repair reaches the confirmation step.
        harness.tracker.clear_hit_and_run(&harness.torrent_id);

        let store = FailAt::wrapping(harness.store.clone() as Arc<dyn RepairStore>, fail_call);
        let worker = harness.worker_with_store(store);

        let job = harness
            .run_until_with(&worker, 80, |job| job.state == RepairState::Completed)
            .await;

        assert_eq!(
            job.state,
            RepairState::Completed,
            "a crash recording the {target} transition must not stop the repair from finishing"
        );

        // The mutating side effects are the ones a fake makes idempotent by
        // tracking "did this genuinely happen" rather than "was this called";
        // a crash that forces a step to replay must not move any of these off
        // one, however many times the step itself re-ran.
        assert_eq!(
            harness.client.add_count(),
            1,
            "crash before {target}: the torrent must have been added exactly once"
        );
        assert_eq!(
            harness.client.recheck_count(),
            1,
            "crash before {target}: the recheck must have been started exactly once"
        );
        assert_eq!(
            harness.client.resume_count(),
            1,
            "crash before {target}: the torrent must have been resumed exactly once"
        );

        for (name, expected_len) in [("e01.mkv", 1000usize), ("e02.mkv", 2000usize)] {
            let path = harness.staging_root.join("job-1/Demo.Show.S01").join(name);
            let bytes = std::fs::read(&path).unwrap_or_else(|error| {
                panic!("crash before {target}: {path:?} must exist and be readable: {error}")
            });
            assert_eq!(
                bytes.len(),
                expected_len,
                "crash before {target}: {path:?} must not have been corrupted or duplicated by a replayed materialize"
            );
        }
    }
}
