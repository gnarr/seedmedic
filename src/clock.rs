//! Time is a port. Leases, backoff and retry budgets all depend on "now", so a
//! test must be able to move it without sleeping.

use chrono::{DateTime, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[cfg(any(test, feature = "fakes"))]
pub use test_clock::TestClock;

#[cfg(any(test, feature = "fakes"))]
mod test_clock {
    use std::sync::Mutex;

    use chrono::{DateTime, Duration, Utc};

    use super::Clock;

    /// A clock that only moves when a test moves it.
    #[derive(Debug)]
    pub struct TestClock {
        now: Mutex<DateTime<Utc>>,
    }

    impl TestClock {
        pub fn new(now: DateTime<Utc>) -> Self {
            Self {
                now: Mutex::new(now),
            }
        }

        pub fn advance(&self, by: Duration) {
            let mut now = self.now.lock().expect("test clock poisoned");
            *now += by;
        }
    }

    impl Default for TestClock {
        fn default() -> Self {
            Self::new(
                DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                    .expect("valid fixed timestamp")
                    .with_timezone(&Utc),
            )
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> DateTime<Utc> {
            *self.now.lock().expect("test clock poisoned")
        }
    }
}
