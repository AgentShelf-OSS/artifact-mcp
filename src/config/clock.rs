//! Injected wall clocks.

#[cfg(any(test, feature = "test-util"))]
use std::sync::Mutex;
use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use time::OffsetDateTime;

use crate::model::Timestamp;

/// Injected wall clock. Production reads the OS clock; tests pin an instant.
pub trait Clock: Send + Sync + fmt::Debug {
    /// Milliseconds since the Unix epoch.
    fn now_unix_millis(&self) -> i64;

    /// Seconds since the Unix epoch, floored towards negative infinity.
    fn now_unix_seconds(&self) -> i64 {
        self.now_unix_millis().div_euclid(1000)
    }

    /// The SQLite `datetime('now')` rendering (`YYYY-MM-DD HH:MM:SS`, UTC) that every
    /// persisted [`Timestamp`] uses.
    fn now_timestamp(&self) -> Timestamp {
        Timestamp(format_sqlite_datetime(self.now_unix_seconds()))
    }
}

/// Render `seconds` the way SQLite's `datetime('now')` does: UTC, second precision.
#[must_use]
pub fn format_sqlite_datetime(seconds: i64) -> String {
    let moment = OffsetDateTime::from_unix_timestamp(seconds)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        moment.year(),
        u8::from(moment.month()),
        moment.day(),
        moment.hour(),
        moment.minute(),
        moment.second()
    )
}

/// Production clock reading the host wall clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix_millis(&self) -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or_else(
            |error| {
                i64::try_from(error.duration().as_millis())
                    .map_or(i64::MIN, |millis| millis.saturating_neg())
            },
            |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        )
    }
}

/// Deterministic clock pinned to a fixed instant, advanceable on demand.
#[derive(Debug)]
#[cfg(any(test, feature = "test-util"))]
pub struct FixedClock {
    millis: Mutex<i64>,
}

#[cfg(any(test, feature = "test-util"))]
impl FixedClock {
    /// A clock pinned to `millis` since the Unix epoch.
    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self {
            millis: Mutex::new(millis),
        }
    }

    /// A clock pinned to `seconds` since the Unix epoch.
    #[must_use]
    pub const fn from_seconds(seconds: i64) -> Self {
        Self::from_millis(seconds * 1000)
    }

    /// Move the clock forward (or backward, with a negative delta).
    pub fn advance_millis(&self, delta: i64) {
        let mut guard = self
            .millis
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *guard = guard.saturating_add(delta);
    }
}

#[cfg(any(test, feature = "test-util"))]
impl Default for FixedClock {
    fn default() -> Self {
        // 2026-01-01T00:00:00Z — a stable, readable instant for golden output.
        Self::from_seconds(1_767_225_600)
    }
}

#[cfg(any(test, feature = "test-util"))]
impl Clock for FixedClock {
    fn now_unix_millis(&self) -> i64 {
        *self
            .millis
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}
