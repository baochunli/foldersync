//! Time source abstraction for deterministic timestamps in tests.

use std::sync::Mutex;

use chrono::{DateTime, Local, SecondsFormat, Timelike, Utc};
use sync_core::WallTime;

/// Clock abstraction (§5) supplying UTC timestamps and local wall-clock times.
pub trait Clock: Send + Sync {
    /// Current UTC timestamp.
    fn now_utc(&self) -> DateTime<Utc>;

    /// Formats the current UTC timestamp as RFC 3339 (`YYYY-MM-DDTHH:MM:SSZ`).
    fn now_rfc3339(&self) -> String {
        self.now_utc().to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    /// Local wall-clock `HH:MM` derived from a UTC timestamp for status text.
    fn wall_time(&self, ts: DateTime<Utc>) -> WallTime {
        let local = ts.with_timezone(&Local);
        WallTime::new(local.hour() as u8, local.minute() as u8)
            .expect("chrono hour/minute are always valid")
    }
}

/// Real wall-clock implementation backed by [`Utc::now`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_utc(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Deterministic clock for unit tests.
#[derive(Debug)]
pub struct FixedClock {
    now: Mutex<DateTime<Utc>>,
}

impl FixedClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    pub fn set(&self, now: DateTime<Utc>) {
        *self.now.lock().unwrap() = now;
    }
}

impl Clock for FixedClock {
    fn now_utc(&self) -> DateTime<Utc> {
        *self.now.lock().unwrap()
    }

    fn wall_time(&self, ts: DateTime<Utc>) -> WallTime {
        WallTime::new(ts.hour() as u8, ts.minute() as u8)
            .expect("chrono hour/minute are always valid")
    }
}
