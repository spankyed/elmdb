//! The synchronization primitives a concurrency model replaces: `std`'s and parking_lot's, Loom's when built with
//! `--cfg loom`, and Shuttle's when built with `--cfg shuttle` (`docs/architecture/testing.md`, Concurrency models).
//! Code that uses these is what a model explores; everything else keeps its own, so a model build runs only the models.

#[cfg(not(any(loom, shuttle)))]
pub(crate) use std::sync::{Condvar, Mutex};

#[cfg(loom)]
pub(crate) use loom::sync::{Condvar, Mutex};

#[cfg(shuttle)]
pub(crate) use shuttle::sync::{Condvar, Mutex};

/// A mutex whose lock is never poisoned: parking_lot's, which Loom doesn't model
#[cfg(not(shuttle))]
pub(crate) use parking_lot::Mutex as PlainMutex;

/// A mutex whose lock is never poisoned, over Shuttle's
#[cfg(shuttle)]
#[derive(Default)]
pub struct PlainMutex<T>(shuttle::sync::Mutex<T>);

#[cfg(shuttle)]
impl<T> PlainMutex<T> {
    pub(crate) fn lock(&self) -> shuttle::sync::MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn try_lock(&self) -> Option<shuttle::sync::MutexGuard<'_, T>> {
        self.0.try_lock().ok()
    }
}

/// Thread-local storage, per Shuttle task in a Shuttle build, whose tasks share one thread
#[cfg(not(shuttle))]
pub(crate) use std::thread_local;

#[cfg(shuttle)]
pub(crate) use shuttle::thread_local;

/// What a model runs on, Loom's threads or Shuttle's, and how it explores them: Loom every interleaving within
/// `LOOM_MAX_PREEMPTIONS`, Shuttle `ELMDB_SHUTTLE_ITERATIONS` random schedules (100,000 by default), printing a failing
/// one's schedule for `shuttle::replay`
#[cfg(all(test, loom))]
pub(crate) mod model {
    pub(crate) use loom::sync::Arc;
    pub(crate) use loom::sync::atomic::{AtomicBool, Ordering};
    pub(crate) use loom::thread;

    pub(crate) fn explore(model: impl Fn() + Send + Sync + 'static) {
        loom::model(model);
    }
}

#[cfg(all(test, shuttle))]
pub(crate) mod model {
    pub(crate) use shuttle::sync::Arc;
    pub(crate) use shuttle::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    pub(crate) use shuttle::thread;

    pub(crate) fn explore(model: impl Fn() + Send + Sync + 'static) {
        let iterations = std::env::var("ELMDB_SHUTTLE_ITERATIONS").map_or(100_000, |n| n.parse().expect("a count"));
        shuttle::check_random(model, iterations);
    }
}

/// A condition variable over `PlainMutex`, whose wait hands the guard back
#[cfg(not(shuttle))]
#[derive(Default)]
pub struct PlainCondvar(parking_lot::Condvar);

#[cfg(not(shuttle))]
impl PlainCondvar {
    pub(crate) fn wait<'a, T>(&self, mut guard: parking_lot::MutexGuard<'a, T>) -> parking_lot::MutexGuard<'a, T> {
        self.0.wait(&mut guard);
        guard
    }

    pub(crate) fn notify_all(&self) {
        self.0.notify_all();
    }
}

/// A condition variable over `PlainMutex`, over Shuttle's
#[cfg(shuttle)]
pub struct PlainCondvar(shuttle::sync::Condvar);

#[cfg(shuttle)]
impl Default for PlainCondvar {
    fn default() -> Self {
        Self(shuttle::sync::Condvar::new())
    }
}

#[cfg(shuttle)]
impl PlainCondvar {
    pub(crate) fn wait<'a, T>(&self, guard: shuttle::sync::MutexGuard<'a, T>) -> shuttle::sync::MutexGuard<'a, T> {
        self.0.wait(guard).unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn notify_all(&self) {
        self.0.notify_all();
    }
}
