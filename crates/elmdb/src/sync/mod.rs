//! The synchronization primitives a concurrency model replaces: `std`'s and parking_lot's, Loom's when built with
//! `--cfg loom`, and Shuttle's when built with `--cfg shuttle` (`docs/architecture/testing.md`, Concurrency models).
//! Code that uses these is what a model explores; everything else keeps its own, so a model build runs only the models.

#[cfg(not(any(loom, shuttle)))]
pub(crate) use std::sync::{Condvar, Mutex, MutexGuard};

#[cfg(loom)]
pub(crate) use loom::sync::{Condvar, Mutex, MutexGuard};

#[cfg(shuttle)]
pub(crate) use shuttle::sync::{Condvar, Mutex, MutexGuard};

/// A mutex whose lock is never poisoned: parking_lot's, which Loom doesn't model
#[cfg(not(shuttle))]
pub(crate) use parking_lot::Mutex as PlainMutex;

// Shuttle's primitives and the models' threads, built only for a model, are files of their own, which cargo-mutants
// leaves out (`.cargo/mutants.toml`): no suite it runs builds them
#[cfg(shuttle)]
mod shuttle_primitives;
#[cfg(shuttle)]
pub use shuttle_primitives::{PlainCondvar, PlainMutex};

#[cfg(all(test, any(loom, shuttle)))]
pub(crate) mod model;

/// Thread-local storage, per Shuttle task in a Shuttle build, whose tasks share one thread
#[cfg(not(shuttle))]
pub(crate) use std::thread_local;

#[cfg(shuttle)]
pub(crate) use shuttle::thread_local;

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
