use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Injected time source shared across timestamps, durability diagnostics, and sans-I/O scheduling.
pub trait Clock: Send + Sync {
    /// Wall-clock milliseconds since UNIX_EPOCH.
    fn now_ms(&self) -> i64;

    /// Monotonic milliseconds for latency and watch-gap pacing.
    fn monotonic_ms(&self) -> u64;
}

/// Production wall + monotonic clock backed by the OS.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self { origin: Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
    }

    fn monotonic_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}
