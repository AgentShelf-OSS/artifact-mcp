//! Bounded retry backoff for non-429 outcomes.

use std::time::Duration;

/// Bounded non-429 backoff, indexed by the already-incremented claim attempt.
pub const RETRY_BACKOFF: [Duration; 7] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(600),
    Duration::from_secs(1800),
    Duration::from_secs(3600),
];

pub(super) fn backoff(attempt: i64) -> Duration {
    RETRY_BACKOFF[usize::try_from(attempt.saturating_sub(1))
        .unwrap_or(usize::MAX)
        .min(RETRY_BACKOFF.len() - 1)]
}
