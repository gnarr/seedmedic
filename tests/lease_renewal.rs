//! `RepairStore::renew_lease` is what lets a worker keep a lease across a step
//! that runs long, without letting a worker that already lost the lease take
//! it back by renewing.

mod support;

use std::time::Duration;

use chrono::Duration as ChronoDuration;
use seedmedic::repair::RepairStore;
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
