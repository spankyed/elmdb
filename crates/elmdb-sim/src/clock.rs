use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use elmdb_storage::Clock;

/// Deterministic clock with manually controlled wall and monotonic time.
#[derive(Debug)]
pub struct SimClock {
    now_ms: AtomicI64,
    monotonic_ms: AtomicU64,
}

impl SimClock {
    pub fn new(initial_wall_ms: i64) -> Self {
        Self { now_ms: AtomicI64::new(initial_wall_ms), monotonic_ms: AtomicU64::new(0) }
    }

    pub fn advance_ms(&self, delta_ms: u64) {
        self.monotonic_ms.fetch_add(delta_ms, Ordering::Relaxed);
        self.now_ms.fetch_add(i64::try_from(delta_ms).unwrap_or(i64::MAX), Ordering::Relaxed);
    }

    /// Simulates wall-clock jump (NTP/suspend) without advancing monotonic clock.
    pub fn set_wall_ms(&self, wall_ms: i64) {
        self.now_ms.store(wall_ms, Ordering::Relaxed);
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new(1_700_000_000_000)
    }
}

impl Clock for SimClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::Relaxed)
    }

    fn monotonic_ms(&self) -> u64 {
        self.monotonic_ms.load(Ordering::Relaxed)
    }
}
