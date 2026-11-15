//! `PlainMutex` and `PlainCondvar` over Shuttle's primitives, for a build with `--cfg shuttle`

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

    /// The lock, tried until `deadline`: Shuttle's mutex has no timed wait, so it is tried and the task yields
    pub(crate) fn try_lock_until(&self, deadline: std::time::Instant) -> Option<shuttle::sync::MutexGuard<'_, T>> {
        loop {
            if let Some(guard) = self.try_lock() {
                return Some(guard);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            shuttle::thread::yield_now();
        }
    }

    pub(crate) fn try_lock(&self) -> Option<shuttle::sync::MutexGuard<'_, T>> {
        self.0.try_lock().ok()
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
