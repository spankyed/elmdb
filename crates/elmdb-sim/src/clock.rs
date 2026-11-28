use elmdb_storage::Clock;
use elmdb_storage::locks::sync::{AtomicU64, Ordering};

/// Deterministic clock with manually controlled wall and monotonic time.
#[derive(Debug)]
pub struct SimClock {
    now_ms: AtomicU64,
    monotonic_ms: AtomicU64,
}

impl SimClock {
    pub fn new(initial_wall_ms: i64) -> Self {
        Self { now_ms: AtomicU64::new(initial_wall_ms.cast_unsigned()), monotonic_ms: AtomicU64::new(0) }
    }

    pub fn advance_ms(&self, delta_ms: u64) {
        self.monotonic_ms.fetch_add(delta_ms, Ordering::Relaxed);
        let delta = i64::try_from(delta_ms).unwrap_or(i64::MAX);
        self.now_ms.fetch_add(delta.cast_unsigned(), Ordering::Relaxed);
    }

    /// Simulates wall-clock jump (NTP/suspend) without advancing monotonic clock.
    pub fn set_wall_ms(&self, wall_ms: i64) {
        self.now_ms.store(wall_ms.cast_unsigned(), Ordering::Relaxed);
    }
}

impl Default for SimClock {
    fn default() -> Self {
        Self::new(1_700_000_000_000)
    }
}

impl Clock for SimClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::Relaxed).cast_signed()
    }

    fn monotonic_ms(&self) -> u64 {
        self.monotonic_ms.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sim_clock_tracks_wall_and_monotonic_time() {
        let clock = SimClock::new(-50);
        assert_eq!(clock.now_ms(), -50);
        assert_eq!(clock.monotonic_ms(), 0);
        clock.advance_ms(75);
        assert_eq!(clock.now_ms(), 25);
        assert_eq!(clock.monotonic_ms(), 75);
        clock.set_wall_ms(1_000);
        assert_eq!(clock.now_ms(), 1_000);
        assert_eq!(clock.monotonic_ms(), 75);
    }
}
