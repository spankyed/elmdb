//! `PlainMutex` and `PlainCondvar` over Shuttle's primitives, for a build with `--cfg shuttle`

/// How many times `try_lock_until` tries the lock in a model before its deadline is taken to have passed
const TRIES_BEFORE_A_DEADLINE: usize = 16;

/// A mutex whose lock is never poisoned, over Shuttle's
#[derive(Debug, Default)]
pub struct PlainMutex<T>(shuttle::sync::Mutex<T>);

impl<T> PlainMutex<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(shuttle::sync::Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> shuttle::sync::MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The lock, tried until `deadline`. Shuttle's mutex has no timed wait, and the wall clock is no model's, whose
    /// schedule a replay must repeat: it is tried, the task yielding between, a fixed number of times, which stands for
    /// a wait that ends while the holder has yet to let go
    pub(crate) fn try_lock_until(&self, _deadline: std::time::Instant) -> Option<shuttle::sync::MutexGuard<'_, T>> {
        for _ in 0..TRIES_BEFORE_A_DEADLINE {
            if let Some(guard) = self.try_lock() {
                return Some(guard);
            }
            shuttle::thread::yield_now();
        }
        self.try_lock()
    }

    pub(crate) fn try_lock(&self) -> Option<shuttle::sync::MutexGuard<'_, T>> {
        match self.0.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
}

/// A condition variable over `PlainMutex`, over Shuttle's
pub struct PlainCondvar(shuttle::sync::Condvar);

impl Default for PlainCondvar {
    fn default() -> Self {
        Self(shuttle::sync::Condvar::new())
    }
}

impl PlainCondvar {
    pub(crate) fn wait<'a, T>(&self, guard: shuttle::sync::MutexGuard<'a, T>) -> shuttle::sync::MutexGuard<'a, T> {
        self.0.wait(guard).unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn notify_all(&self) {
        self.0.notify_all();
    }
}
