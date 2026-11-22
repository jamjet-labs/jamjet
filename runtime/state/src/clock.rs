//! This process's clock, for the times a store takes from the process
//! rather than from its database.
//!
//! The Postgres store decides leases, retries and claim order on the
//! database's clock, so engines whose clocks disagree still agree on them.
//! It reads this clock only to turn a time a caller gives into a delay.
//!
//! Under the `test-hooks` feature, `JAMJET_TEST_CLOCK_SKEW_SECS` moves this
//! clock by that many seconds, read once per process, so a test can run a
//! process whose clock is wrong. Without the feature the variable is not
//! read and its name is not in the build.

use chrono::{DateTime, Utc};

/// The time now by this process's clock: `Utc::now()`, moved by the test
/// skew under the `test-hooks` feature.
pub fn process_now() -> DateTime<Utc> {
    let now = Utc::now();
    #[cfg(feature = "test-hooks")]
    let now = now + skew::get();
    now
}

#[cfg(feature = "test-hooks")]
mod skew {
    use std::sync::OnceLock;

    const ENV: &str = "JAMJET_TEST_CLOCK_SKEW_SECS";

    /// The skew the variable sets, zero when it is unset. A value that is
    /// not a whole number of seconds panics, so a mistyped skew cannot pass
    /// for a clock that is right.
    pub(super) fn get() -> chrono::Duration {
        static SKEW: OnceLock<chrono::Duration> = OnceLock::new();
        *SKEW.get_or_init(|| match std::env::var(ENV) {
            Err(_) => chrono::Duration::zero(),
            Ok(v) => match v.trim().parse::<i64>() {
                Ok(secs) => chrono::Duration::seconds(secs),
                Err(_) => panic!("{ENV}: not a whole number of seconds: {v:?}"),
            },
        })
    }
}
