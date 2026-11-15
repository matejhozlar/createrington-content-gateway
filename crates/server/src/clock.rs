use std::time::{SystemTime, UNIX_EPOCH};

pub const MINUTE_MS: u64 = 60_000;
pub const HOUR_MS: u64 = 60 * MINUTE_MS;
pub const DAY_MS: u64 = 24 * HOUR_MS;

/// Milliseconds since the Unix epoch. Swapped for a fixed clock in tests.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            })
    }
}

#[cfg(test)]
pub(crate) struct TestClock(std::sync::atomic::AtomicU64);

#[cfg(test)]
impl TestClock {
    pub(crate) fn at(ms: u64) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self(std::sync::atomic::AtomicU64::new(ms)))
    }

    pub(crate) fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(test)]
impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}
