//! `RepairStore::renew_lease` is what lets a worker keep a lease across a step
//! that runs long, without letting a worker that already lost the lease take
//! it back by renewing.

mod support;

use std::{sync::Arc, time::Duration};

use chrono::Duration as ChronoDuration;
use seedmedic::{
    clock::Clock,
    repair::{RepairState, RepairStore, WorkerConfig, worker::RepairWorker},
};
use support::{Harness, OWNER};

#[tokio::test]
async fn renewing_extends_the_lease_so_a_second_claim_still_fails() {
    let harness = Harness::new().await;
    let job = harness.discover().await;

    harness
        .store
        .claim(OWNER, Duration::from_secs(60), 4)
        .await
        .expect("claim");

    // Past the original lease, but the renewal below moves the goalposts.
    harness.clock.advance(ChronoDuration::seconds(90));
    assert!(
        harness
            .store
            .renew_lease(job.id, OWNER, Duration::from_secs(60))
            .await
            .expect("renew")
    );

    assert!(
        harness
            .store
            .claim("another-worker", Duration::from_secs(60), 4)
            .await
            .expect("claim")
            .is_empty(),
        "a renewed lease must still block other claimants"
    );
}

#[tokio::test]
async fn renewal_by_a_non_owner_affects_nothing() {
    let harness = Harness::new().await;
    let job = harness.discover().await;

    harness
        .store
        .claim(OWNER, Duration::from_secs(300), 4)
        .await
        .expect("claim");

    let renewed = harness
        .store
        .renew_lease(job.id, "an-impostor", Duration::from_secs(300))
        .await
        .expect("renew");
    assert!(
        !renewed,
        "a worker that never held this lease must not be able to grant itself one"
    );

    // The real owner's lease is exactly as it was: still in force.
    assert!(
        harness
            .store
            .claim("an-impostor", Duration::from_secs(300), 4)
            .await
            .expect("claim")
            .is_empty(),
        "the impostor's failed renewal must not have given it the lease either"
    );
}

#[tokio::test]
async fn a_worker_that_renews_and_then_stops_still_releases_the_job_after_one_lease_period() {
    let harness = Harness::new().await;
    let job = harness.discover().await;

    harness
        .store
        .claim(OWNER, Duration::from_secs(60), 4)
        .await
        .expect("claim");
    assert!(
        harness
            .store
            .renew_lease(job.id, OWNER, Duration::from_secs(60))
            .await
            .expect("renew")
    );

    // The worker dies here without calling `release`. Nothing but expiry of
    // the *renewed* lease should ever free the job again.
    harness.clock.advance(ChronoDuration::seconds(59));
    assert!(
        harness
            .store
            .claim("another-worker", Duration::from_secs(60), 4)
            .await
            .expect("claim")
            .is_empty(),
        "the renewed lease has not expired yet"
    );

    harness.clock.advance(ChronoDuration::seconds(2));
    let reclaimed = harness
        .store
        .claim("another-worker", Duration::from_secs(60), 4)
        .await
        .expect("claim");
    assert_eq!(
        reclaimed.len(),
        1,
        "one lease period after the last renewal, the job must be claimable again"
    );
    assert_eq!(reclaimed[0].id, job.id);
}

/// The tests above prove `renew_lease` itself works. This one proves the
/// *worker* actually calls it while driving a job through several steps in a
/// single tick — "the step that just finished may have taken a while; renew
/// now so a long one never outlives its lease" (`RepairWorker::drive_inner`).
///
/// `FakeTorrentClient::slow_down` advances the test clock on every call the
/// download client makes, standing in for real wall-clock time passing while
/// SeedMedic waits on the network. A concurrently spawned prober tries to
/// steal the job with a competing claim every time the clock moves. If the
/// worker only renewed once per tick rather than once per completed step,
/// the original lease would already be behind the clock by the time the
/// prober gets to try — and the steal would succeed.
#[tokio::test]
async fn a_worker_renews_its_lease_between_steps_so_a_slow_drive_is_never_stolen() {
    let harness = Harness::new().await;
    harness.discover().await;

    let lease = Duration::from_secs(3);
    let per_call = ChronoDuration::seconds(1);
    let claimed_at = harness.clock.now();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    harness
        .client
        .slow_down(harness.clock.clone(), per_call, tx);

    let store = harness.store.clone() as Arc<dyn RepairStore>;
    let prober = tokio::spawn(async move {
        let mut stolen = false;
        while rx.recv().await.is_some() {
            let claimed = store
                .claim("impostor", Duration::from_secs(60), 4)
                .await
                .expect("impostor claim");
            stolen |= !claimed.is_empty();
        }
        stolen
    });

    let worker = RepairWorker::new(
        harness.deps.clone(),
        WorkerConfig {
            owner: OWNER.to_owned(),
            lease,
            batch_size: 4,
            poll_interval: Duration::from_secs(1),
            discovery_interval: Duration::from_secs(1),
        },
    );
    worker.tick().await;

    // Stop signalling so the prober's channel closes and it can return.
    harness.client.stop_slowing_down();
    let stolen = prober.await.expect("prober task panicked");

    assert!(
        harness.clock.now() - claimed_at > ChronoDuration::from_std(lease).expect("valid lease"),
        "the scenario must genuinely outlast the original lease, or renewal proves nothing"
    );
    assert!(
        !stolen,
        "an impostor's claim must never succeed while the original worker is still driving the job"
    );

    // And the drive made real, durable progress despite the slow client and
    // the short lease: renewal protected it rather than the job just sitting
    // there untouched.
    let job = harness.only_job().await;
    assert!(
        job.state.rank().expect("actionable state") > RepairState::Staged.rank().expect("ranked"),
        "the job must have advanced past staging despite the slow, lease-outlasting drive"
    );
}
