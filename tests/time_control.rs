//! Behaviour that only shows up once `TestClock` actually moves: retries
//! back off instead of hammering immediately, and a seeding wait that could
//! span days does not poll as though it were still counting in seconds.
//!
//! `lease_renewal.rs` covers the third time-dependent property this suite
//! cares about — a lease surviving a slow, multi-step drive — since it needs
//! the same `FakeTorrentClient::slow_down` machinery as the rest of that
//! file's scenarios.

mod support;

use chrono::Duration as ChronoDuration;
use seedmedic::{
    clock::Clock,
    repair::{RepairState, ReviewReason},
    seeding::ClientError,
};
use support::{Harness, clock_epoch, default_policy, torrent_metadata};

#[tokio::test]
async fn a_retriable_failure_delays_the_next_attempt_instead_of_retrying_immediately() {
    let harness = Harness::new().await;
    harness.discover().await;

    // The very first call `inject` makes fails: the drive gets from
    // `Discovered` to `Staged` in the same tick (nothing before injection
    // touches the download client), then the client call itself fails.
    harness
        .client
        .fail_next_call_with(ClientError::Transport("simulated network blip".into()));
    harness.tick().await;

    let job = harness.only_job().await;
    assert_eq!(
        job.state,
        RepairState::Staged,
        "a retriable failure must not move the job; it stays where the failed step started"
    );
    assert_eq!(job.attempts, 1);
    let due = job
        .next_attempt_at
        .expect("a retry must schedule its next attempt");
    assert!(
        due > harness.clock.now(),
        "the retry must be delayed, not due immediately"
    );

    // Not due yet: ticking again must not re-attempt the side effect.
    harness.tick().await;
    assert_eq!(
        harness.client.add_count(),
        0,
        "ticking before the backoff elapses must not retry the side effect yet"
    );
    assert_eq!(
        harness.job(job.id).await.state,
        RepairState::Staged,
        "the job must still be waiting out its backoff"
    );

    // Advance exactly to the due time: now the retried step goes through.
    harness.clock.advance(due - harness.clock.now());
    harness.tick().await;
    let job = harness.job(job.id).await;
    assert!(
        job.state.rank().expect("actionable") > RepairState::Staged.rank().expect("actionable"),
        "once due, the retried step must succeed and the repair must proceed"
    );
    assert_eq!(harness.client.add_count(), 1);
}

/// A hit-and-run deadline days away must not turn into one poll per second of
/// that window — `policy::tracker_poll_delay` scales the interval with how
/// much time is left, specifically so a multi-day wait costs a bounded number
/// of polls rather than one per unit of real time.
#[tokio::test]
async fn a_multi_day_seeding_wait_polls_a_bounded_number_of_times() {
    let poll_interval = std::time::Duration::from_secs(2 * 3600);
    let poll_min_interval = std::time::Duration::from_secs(60);
    let deadline = clock_epoch() + ChronoDuration::days(5);

    let policy = seedmedic::repair::SafetyPolicy {
        tracker_poll_interval: poll_interval,
        tracker_poll_min_interval: poll_min_interval,
        ..default_policy()
    };

    let harness = support::Harness::with_policy_metadata_and_deadline(
        policy,
        torrent_metadata(),
        &[("e01.mkv", vec![b'a'; 1000]), ("e02.mkv", vec![b'b'; 2000])],
        Some(deadline),
    )
    .await;
    harness.discover().await;

    // Never clear the hit-and-run: the tracker keeps saying `Active` until
    // the deadline passes, so every poll while `Seeding` genuinely waits.
    let mut job = harness
        .run_until(40, |job| job.state == RepairState::Seeding)
        .await;

    let mut polls = 0;
    for _ in 0..1000 {
        if job.state != RepairState::Seeding {
            break;
        }
        harness.tick().await;
        polls += 1;
        job = harness.only_job().await;
        if let Some(due) = job.next_attempt_at {
            let remaining = due - harness.clock.now();
            if remaining > ChronoDuration::zero() {
                harness.clock.advance(remaining);
            }
        }
    }

    assert_eq!(
        job.state,
        RepairState::AwaitingReview,
        "the deadline passing must park the job for review rather than loop forever"
    );
    assert_eq!(
        job.review_reason,
        Some(ReviewReason::HitAndRunDeadlinePassed)
    );

    // A naive design that always polled at the minimum interval would need
    // 5 days / 1 minute = 7200 polls to cover the same window. Scaling the
    // interval with the time remaining keeps the real count in the low
    // hundreds — the bound this test exists to prove.
    assert!(
        polls < 500,
        "a five-day wait must not require anywhere near one poll per minute of real time — got {polls} polls"
    );
}
