//! Deterministic close-and-drain handle lifecycle (`DbLifecycle<T>`) for `Database`.
//!
//! Holding the inner handle inside `RwLock<Option<Arc<T>>>` with a `SeqCst` handshake + `Condvar` drain
//! guarantees `close()` / `close_at_exit()` waits for every in-flight `LifecycleGuard` to drop its `Arc<T>`
//! BEFORE running `on_drained_close` (which syncs and drops `LmdbStore` / `mdb_env_close`), while concurrent
//! `close()` callers block on `close_cv` and receive the same completion outcome.

use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use elmdb_storage::locks::sync::{AtomicBool, AtomicUsize, Condvar, Mutex, Ordering, RwLock};
use elmdb_wire::{ElmdbError, ErrorCode, Result};

#[derive(Default)]
struct CloseSync {
    done: bool,
    outcome: Option<(ErrorCode, String)>,
}

pub(crate) struct DbLifecycle<T> {
    pub(crate) handle: RwLock<Option<Arc<T>>>,
    pub(crate) closed: AtomicBool,
    active_ops: AtomicUsize,
    drain_mu: Mutex<()>,
    drain_cv: Condvar,
    close_mu: Mutex<CloseSync>,
    close_cv: Condvar,
}

pub(crate) struct LifecycleGuard<'a, T> {
    lifecycle: &'a DbLifecycle<T>,
    handle: Option<Arc<T>>,
}

impl<T> LifecycleGuard<'_, T> {
    pub(crate) fn arc(&self) -> &Arc<T> {
        self.handle.as_ref().expect("LifecycleGuard handle live until Drop")
    }
}

impl<T> Deref for LifecycleGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.handle.as_ref().expect("LifecycleGuard handle live until Drop")
    }
}

impl<T> Drop for LifecycleGuard<'_, T> {
    fn drop(&mut self) {
        // Drop our local `Arc<T>` strictly BEFORE decrementing `active_ops` so that when
        // `close()` observes `active_ops == 0`, zero other threads can still hold an `Arc` and
        // `mdb_env_close()` is guaranteed to complete before `close()` returns!
        drop(self.handle.take());
        let prev = self.lifecycle.active_ops.fetch_sub(1, Ordering::SeqCst);
        // mutant-skip: replace && with \|\| in <impl Drop for LifecycleGuard
        // Fast-path lock avoidance: waking `drain_cv` spuriously when `prev != 1` or `!closed` is
        // observationally identical because `close()` loops re-checking `active_ops == 0`.
        if prev == 1 && self.lifecycle.closed.load(Ordering::SeqCst) {
            let _lock = self.lifecycle.drain_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            self.lifecycle.drain_cv.notify_all();
        }
    }
}

impl<T> DbLifecycle<T> {
    pub(crate) fn new(handle: T) -> Self {
        Self {
            handle: RwLock::new(Some(Arc::new(handle))),
            closed: AtomicBool::new(false),
            active_ops: AtomicUsize::new(0),
            drain_mu: Mutex::new(()),
            drain_cv: Condvar::new(),
            close_mu: Mutex::new(CloseSync::default()),
            close_cv: Condvar::new(),
        }
    }

    #[inline]
    pub(crate) fn enter_op(&self) -> Result<LifecycleGuard<'_, T>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ElmdbError::new(ErrorCode::Closed, "database is closed"));
        }
        // Hold `handle.read()` across the `active_ops` increment and `closed` re-check so
        // `close()` (which takes `handle.write()` after setting `closed = true`) can
        // never race past an entering operation on weakly-ordered CPUs.
        let guard = self.handle.read().unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.closed.load(Ordering::SeqCst) {
            return Err(ElmdbError::new(ErrorCode::Closed, "database is closed"));
        }
        let Some(handle) = guard.as_ref().map(Arc::clone) else {
            return Err(ElmdbError::new(ErrorCode::Closed, "database is closed"));
        };
        self.active_ops.fetch_add(1, Ordering::SeqCst);
        drop(guard);
        Ok(LifecycleGuard { lifecycle: self, handle: Some(handle) })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn close(
        &self,
        timeout: Option<Duration>,
        propagate_sync_err: bool,
        read_only: bool,
        skip_drain: bool,
        before_take: impl FnOnce(),
        on_drained_close: impl FnOnce(Arc<T>) -> Result<()>,
    ) -> Result<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            let mut st = self.close_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            #[cfg(not(any(loom, shuttle)))]
            let deadline = timeout.map(|d| std::time::Instant::now() + d);
            #[cfg(any(loom, shuttle))]
            let mut remaining_yields: usize = match timeout {
                None => 0,
                Some(d) if d.is_zero() => 0,
                Some(_) => 2,
            };
            while !st.done {
                match timeout {
                    Some(_) => {
                        #[cfg(not(any(loom, shuttle)))]
                        {
                            let dl = deadline.expect("deadline when timeout is Some");
                            let now = std::time::Instant::now();
                            if now >= dl {
                                return if !propagate_sync_err {
                                    Ok(())
                                } else if read_only {
                                    Err(ElmdbError::new(
                                        ErrorCode::DatabaseLocked,
                                        "database close timed out waiting for concurrent close",
                                    ))
                                } else {
                                    Err(ElmdbError::new(
                                        ErrorCode::DurabilityFailed,
                                        "database close timed out waiting for concurrent close",
                                    ))
                                };
                            }
                            let (next_st, _) = self
                                .close_cv
                                .wait_timeout(st, dl.saturating_duration_since(now))
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            st = next_st;
                        }
                        #[cfg(any(loom, shuttle))]
                        {
                            if remaining_yields == 0 {
                                return if !propagate_sync_err {
                                    Ok(())
                                } else if read_only {
                                    Err(ElmdbError::new(
                                        ErrorCode::DatabaseLocked,
                                        "database close timed out waiting for concurrent close",
                                    ))
                                } else {
                                    Err(ElmdbError::new(
                                        ErrorCode::DurabilityFailed,
                                        "database close timed out waiting for concurrent close",
                                    ))
                                };
                            }
                            remaining_yields -= 1;
                            drop(st);
                            elmdb_storage::locks::model::thread::yield_now();
                            st = self.close_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                    }
                    None => {
                        st = self.close_cv.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                }
            }
            if propagate_sync_err && let Some((code, msg)) = &st.outcome {
                return Err(ElmdbError::new(*code, msg.clone()));
            }
            return Ok(());
        }

        struct MarkCloseDone<'a, T> {
            lifecycle: &'a DbLifecycle<T>,
            outcome: Option<(ErrorCode, String)>,
        }
        impl<T> Drop for MarkCloseDone<'_, T> {
            fn drop(&mut self) {
                let mut st = self.lifecycle.close_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                st.outcome = self.outcome.take();
                st.done = true;
                self.lifecycle.close_cv.notify_all();
            }
        }
        // Default to a failure outcome so unwinding on panic never reports `Ok(())` to waiters.
        let mut mark_done = MarkCloseDone {
            lifecycle: self,
            outcome: Some((ErrorCode::DurabilityFailed, "database close panicked before completing".to_owned())),
        };

        before_take();

        // Take the handle out under `handle.write()`, which acts as a full barrier against
        // any concurrent `enter_op` acquiring `handle.read()` + incrementing `active_ops`.
        let taken = self.handle.write().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        elmdb_storage::run_point!(CLOSE_INNER_BEFORE_DRAIN);

        // Wait for already-entered operations to drain cleanly via `drain_cv` (unless this very
        // thread is tearing down inside its own aborted active transaction).
        let drained = if skip_drain {
            false
        } else {
            let mut lock = self.drain_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            #[cfg(not(any(loom, shuttle)))]
            let deadline = timeout.map(|d| std::time::Instant::now() + d);
            #[cfg(any(loom, shuttle))]
            let mut remaining_yields: usize = match timeout {
                None => 0,
                Some(d) if d.is_zero() => 0,
                Some(_) => 2,
            };
            loop {
                if self.active_ops.load(Ordering::SeqCst) == 0 {
                    break true;
                }
                match timeout {
                    Some(_) => {
                        #[cfg(not(any(loom, shuttle)))]
                        {
                            let dl = deadline.expect("deadline when timeout is Some");
                            let now = std::time::Instant::now();
                            if now >= dl {
                                break false;
                            }
                            let (next_lock, _) = self
                                .drain_cv
                                .wait_timeout(lock, dl.saturating_duration_since(now))
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            lock = next_lock;
                        }
                        #[cfg(any(loom, shuttle))]
                        {
                            if remaining_yields == 0 {
                                break false;
                            }
                            remaining_yields -= 1;
                            drop(lock);
                            elmdb_storage::locks::model::thread::yield_now();
                            lock = self.drain_mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                    }
                    None => {
                        lock = self.drain_cv.wait(lock).unwrap_or_else(std::sync::PoisonError::into_inner);
                    }
                }
            }
        };

        let outcome = if !drained {
            drop(taken);
            if read_only {
                Err(ElmdbError::new(
                    ErrorCode::DatabaseLocked,
                    "database close timed out waiting for active operations to drain",
                ))
            } else {
                Err(ElmdbError::new(
                    ErrorCode::DurabilityFailed,
                    "database close timed out waiting for active operations to drain; durability sync skipped",
                ))
            }
        } else if let Some(handle) = taken {
            on_drained_close(handle)
        } else {
            Ok(())
        };

        mark_done.outcome = outcome.as_ref().err().map(|err| (err.code, err.message.clone()));
        drop(mark_done);
        if propagate_sync_err { outcome } else { Ok(()) }
    }
}

#[cfg(all(test, any(loom, shuttle)))]
#[path = "db_lifecycle_model.rs"]
mod model;
