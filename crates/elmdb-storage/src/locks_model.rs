//! Loom & Shuttle concurrency-model backend and Model 1 (`locks::model`) for `elmdb-storage::locks`.
//!
//! Compiled **only** under `--cfg loom` or `--cfg shuttle`; default and release builds use
//! `parking_lot` directly in `locks.rs` with zero overhead.

use std::fmt;
use std::ops::{Deref, DerefMut};

use super::sync;

#[cfg(loom)]
pub use loom::sync::Arc;
#[cfg(loom)]
pub use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
#[cfg(loom)]
pub use loom::thread;

#[cfg(shuttle)]
pub use shuttle::sync::Arc;
#[cfg(shuttle)]
pub use shuttle::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(shuttle)]
pub use shuttle::thread;

/// Runs a concurrency model under Loom (`LOOM_MAX_PREEMPTIONS`) or Shuttle (`ELMDB_SHUTTLE_ITERATIONS`,
/// default 100,000 split evenly across uniform random, PCT depth 2, and PCT depth 3).
#[cfg(loom)]
pub fn explore(model: impl Fn() + Send + Sync + 'static) {
    loom::model(model);
}

/// Runs a concurrency model under Shuttle: 1/3 uniform random, 1/3 PCT depth 2, 1/3 PCT depth 3.
#[cfg(shuttle)]
pub fn explore(model: impl Fn() + Send + Sync + 'static) {
    let iterations = std::env::var("ELMDB_SHUTTLE_ITERATIONS").map_or(100_000, |n| n.parse().expect("a count"));
    let each = (iterations / 3).max(1);
    let model: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(model);
    let run = |m: &std::sync::Arc<dyn Fn() + Send + Sync>| {
        let m = std::sync::Arc::clone(m);
        move || m()
    };
    shuttle::check_random(run(&model), each);
    shuttle::check_pct(run(&model), each, 2);
    shuttle::check_pct(run(&model), each, 3);
}

/// Non-poisoning mutex wrapper over Loom/Shuttle `Mutex`, matching `parking_lot::Mutex` ergonomics
/// for simulator & internal locks (`elmdb-sim`, etc.).
#[derive(Debug, Default)]
pub struct PlainMutex<T>(sync::Mutex<T>);

pub type PlainMutexGuard<'a, T> = sync::MutexGuard<'a, T>;

impl<T> PlainMutex<T> {
    pub fn new(value: T) -> Self {
        Self(sync::Mutex::new(value))
    }

    pub fn lock(&self) -> PlainMutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct ModelMutexState<T> {
    locked: bool,
    value: Option<T>,
}

/// Safe owned-guard mutex backed by Loom/Shuttle `Mutex` + `Condvar`.
/// Supports both borrowed `lock()` and `'static` `lock_arc()` with `#![forbid(unsafe_code)]`.
pub(super) struct ModelMutex<T> {
    inner: std::sync::Arc<(sync::Mutex<ModelMutexState<T>>, sync::Condvar)>,
}

impl<T: fmt::Debug> fmt::Debug for ModelMutex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelMutex").finish_non_exhaustive()
    }
}

impl<T: Default> Default for ModelMutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> ModelMutex<T> {
    pub(super) fn new(value: T) -> Self {
        Self {
            inner: std::sync::Arc::new((
                sync::Mutex::new(ModelMutexState { locked: false, value: Some(value) }),
                sync::Condvar::new(),
            )),
        }
    }

    pub(super) fn lock_owned(&self) -> ModelMutexGuard<T> {
        let (mu, cv) = &*self.inner;
        let mut st = mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        while st.locked {
            st = cv.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        st.locked = true;
        let value = st.value.take().expect("ModelMutex value present when unlocked");
        drop(st);
        ModelMutexGuard { inner: std::sync::Arc::clone(&self.inner), value: Some(value) }
    }
}

pub(super) struct ModelMutexGuard<T> {
    inner: std::sync::Arc<(sync::Mutex<ModelMutexState<T>>, sync::Condvar)>,
    value: Option<T>,
}

impl<T> Deref for ModelMutexGuard<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.value.as_ref().expect("ModelMutexGuard live until Drop")
    }
}

impl<T> DerefMut for ModelMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value.as_mut().expect("ModelMutexGuard live until Drop")
    }
}

impl<T> Drop for ModelMutexGuard<T> {
    fn drop(&mut self) {
        let (mu, cv) = &*self.inner;
        let mut st = mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.value = self.value.take();
        st.locked = false;
        cv.notify_all();
    }
}

struct ModelRwState<T> {
    readers: usize,
    waiting_writers: usize,
    writer: bool,
    value: Option<std::sync::Arc<T>>,
}

/// Safe write-preferring owned-guard reader-writer lock backed by Loom/Shuttle `Mutex` + `Condvar`,
/// mirroring `parking_lot::RwLock`'s writer-preference so models catch reader-stall deadlocks
/// (e.g. `DbiPublishGate` lane ordering). Supports concurrent `read()` / `'static` `read_arc()`
/// and exclusive `write()` with `#![forbid(unsafe_code)]`.
pub(super) struct ModelRwLock<T> {
    inner: std::sync::Arc<(sync::Mutex<ModelRwState<T>>, sync::Condvar)>,
}

impl<T: fmt::Debug> fmt::Debug for ModelRwLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelRwLock").finish_non_exhaustive()
    }
}

impl<T: Default> Default for ModelRwLock<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> ModelRwLock<T> {
    pub(super) fn new(value: T) -> Self {
        Self {
            inner: std::sync::Arc::new((
                sync::Mutex::new(ModelRwState {
                    readers: 0,
                    waiting_writers: 0,
                    writer: false,
                    value: Some(std::sync::Arc::new(value)),
                }),
                sync::Condvar::new(),
            )),
        }
    }

    pub(super) fn read_owned(&self) -> ModelRwReadGuard<T> {
        let (mu, cv) = &*self.inner;
        let mut st = mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Mirror `parking_lot::RwLock`'s write-preferring semantics: new readers wait behind
        // a queued writer so models detect any lock-ordering bug that would stall readers.
        while st.writer || st.waiting_writers > 0 {
            st = cv.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        st.readers += 1;
        let value = std::sync::Arc::clone(st.value.as_ref().expect("ModelRwLock value present for readers"));
        drop(st);
        ModelRwReadGuard { inner: std::sync::Arc::clone(&self.inner), value: Some(value) }
    }

    pub(super) fn write_owned(&self) -> ModelRwWriteGuard<T> {
        let (mu, cv) = &*self.inner;
        let mut st = mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.waiting_writers += 1;
        while st.writer || st.readers > 0 {
            st = cv.wait(st).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        st.waiting_writers -= 1;
        st.writer = true;
        let arc = st.value.take().expect("ModelRwLock value present for writer");
        let value = std::sync::Arc::try_unwrap(arc).ok().expect("zero readers implies unique Arc owner");
        drop(st);
        ModelRwWriteGuard { inner: std::sync::Arc::clone(&self.inner), value: Some(value) }
    }
}

pub(super) struct ModelRwReadGuard<T> {
    inner: std::sync::Arc<(sync::Mutex<ModelRwState<T>>, sync::Condvar)>,
    value: Option<std::sync::Arc<T>>,
}

impl<T> Deref for ModelRwReadGuard<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.value.as_deref().expect("ModelRwReadGuard live until Drop")
    }
}

impl<T> Drop for ModelRwReadGuard<T> {
    fn drop(&mut self) {
        // Drop our Arc clone strictly BEFORE reacquiring the state mutex so a subsequent
        // writer observing `readers == 0` is guaranteed unique ownership via `Arc::try_unwrap`.
        drop(self.value.take());
        let (mu, cv) = &*self.inner;
        let mut st = mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.readers -= 1;
        if st.readers == 0 {
            cv.notify_all();
        }
    }
}

pub(super) struct ModelRwWriteGuard<T> {
    inner: std::sync::Arc<(sync::Mutex<ModelRwState<T>>, sync::Condvar)>,
    value: Option<T>,
}

impl<T> Deref for ModelRwWriteGuard<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        self.value.as_ref().expect("ModelRwWriteGuard live until Drop")
    }
}

impl<T> DerefMut for ModelRwWriteGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value.as_mut().expect("ModelRwWriteGuard live until Drop")
    }
}

impl<T> Drop for ModelRwWriteGuard<T> {
    fn drop(&mut self) {
        let (mu, cv) = &*self.inner;
        let mut st = mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        st.value = Some(std::sync::Arc::new(self.value.take().expect("ModelRwWriteGuard live until Drop")));
        st.writer = false;
        cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::locks::{DbiPublishGate, LockScopeId, MapGate, MetaLock, MetaRwLock, SchemaEpochLock, WriterLock};

    /// Model 1a: L1..L5 strict hierarchy under concurrent readers, writers (with schema epoch bump),
    /// and map resizers never deadlocks and maintains mutual exclusion across `MapGate::write()`.
    #[test]
    fn map_gate_resize_excludes_all_readers_and_writers() {
        explore(|| {
            let scope = LockScopeId::fresh();
            let schema = Arc::new(SchemaEpochLock::new_in_scope(scope, 0u64));
            let map = Arc::new(MapGate::new_in_scope(scope, 1u64));
            let writer = Arc::new(WriterLock::new_in_scope(scope, 0u64));
            let meta = Arc::new(MetaLock::new_in_scope(scope, 0u64));
            let active_readers = Arc::new(AtomicUsize::new(0));
            let active_writers = Arc::new(AtomicUsize::new(0));
            let resizing = Arc::new(AtomicBool::new(false));

            let t_reader = {
                let schema = Arc::clone(&schema);
                let map = Arc::clone(&map);
                let meta = Arc::clone(&meta);
                let active_readers = Arc::clone(&active_readers);
                let resizing = Arc::clone(&resizing);
                thread::spawn(move || {
                    let s = schema.read();
                    let r = map.read_arc();
                    active_readers.fetch_add(1, Ordering::SeqCst);
                    assert!(!resizing.load(Ordering::SeqCst), "reader entered during active MapGate::write");
                    let _m = meta.lock();
                    let seen_map = *r;
                    let seen_schema = *s;
                    assert!(seen_map >= 1);
                    assert!(seen_schema <= 1);
                    active_readers.fetch_sub(1, Ordering::SeqCst);
                })
            };

            let t_writer = {
                let schema = Arc::clone(&schema);
                let map = Arc::clone(&map);
                let writer = Arc::clone(&writer);
                let meta = Arc::clone(&meta);
                let active_writers = Arc::clone(&active_writers);
                let resizing = Arc::clone(&resizing);
                thread::spawn(move || {
                    let mut s = schema.write();
                    let r = map.read_arc();
                    active_writers.fetch_add(1, Ordering::SeqCst);
                    assert!(!resizing.load(Ordering::SeqCst), "writer entered during active MapGate::write");
                    let mut w = writer.lock_arc();
                    *s += 1;
                    *w += 1;
                    let mut m = meta.lock();
                    *m += *r;
                    active_writers.fetch_sub(1, Ordering::SeqCst);
                })
            };

            let t_resizer = {
                let map = Arc::clone(&map);
                let active_readers = Arc::clone(&active_readers);
                let active_writers = Arc::clone(&active_writers);
                let resizing = Arc::clone(&resizing);
                thread::spawn(move || {
                    let mut g = map.write();
                    resizing.store(true, Ordering::SeqCst);
                    assert_eq!(
                        active_readers.load(Ordering::SeqCst),
                        0,
                        "MapGate::write entered while readers were active"
                    );
                    assert_eq!(
                        active_writers.load(Ordering::SeqCst),
                        0,
                        "MapGate::write entered while writers were active"
                    );
                    *g += 1;
                    resizing.store(false, Ordering::SeqCst);
                })
            };

            t_reader.join().unwrap();
            t_writer.join().unwrap();
            t_resizer.join().unwrap();

            assert_eq!(*schema.read(), 1);
            assert_eq!(*map.read(), 2);
            assert_eq!(*writer.lock(), 1);
            assert!(*meta.lock() >= 1);
        });
    }

    /// Model 1b: `DbiPublishGate` dual-lane barrier serializes `write()` against BOTH
    /// `read_txn()` (short readers) and `read_copy()` (long backups).
    #[test]
    fn dbi_publish_gate_dual_lane_exclusion() {
        explore(|| {
            let scope = LockScopeId::fresh();
            let gate = Arc::new(DbiPublishGate::new_in_scope(scope));
            let dbis = Arc::new(MetaRwLock::new_in_scope(scope, 0u64));
            let copy_active = Arc::new(AtomicBool::new(false));
            let txn_active = Arc::new(AtomicBool::new(false));
            let pub_active = Arc::new(AtomicBool::new(false));

            let t_copy = {
                let gate = Arc::clone(&gate);
                let copy_active = Arc::clone(&copy_active);
                let pub_active = Arc::clone(&pub_active);
                thread::spawn(move || {
                    let _g = gate.read_copy();
                    copy_active.store(true, Ordering::SeqCst);
                    assert!(!pub_active.load(Ordering::SeqCst), "copy overlapped with publish");
                    thread::yield_now();
                    copy_active.store(false, Ordering::SeqCst);
                })
            };

            let t_reader = {
                let gate = Arc::clone(&gate);
                let dbis = Arc::clone(&dbis);
                let txn_active = Arc::clone(&txn_active);
                let pub_active = Arc::clone(&pub_active);
                thread::spawn(move || {
                    let _g = gate.read_txn();
                    txn_active.store(true, Ordering::SeqCst);
                    assert!(!pub_active.load(Ordering::SeqCst), "read_txn overlapped with publish");
                    let v = *dbis.read();
                    assert!(v <= 1);
                    txn_active.store(false, Ordering::SeqCst);
                })
            };

            let t_pub = {
                let gate = Arc::clone(&gate);
                let dbis = Arc::clone(&dbis);
                let copy_active = Arc::clone(&copy_active);
                let txn_active = Arc::clone(&txn_active);
                let pub_active = Arc::clone(&pub_active);
                thread::spawn(move || {
                    let _g = gate.write();
                    pub_active.store(true, Ordering::SeqCst);
                    assert!(!copy_active.load(Ordering::SeqCst), "publish overlapped with read_copy");
                    assert!(!txn_active.load(Ordering::SeqCst), "publish overlapped with read_txn");
                    *dbis.write() += 1;
                    pub_active.store(false, Ordering::SeqCst);
                })
            };

            t_copy.join().unwrap();
            t_reader.join().unwrap();
            t_pub.join().unwrap();

            assert_eq!(*dbis.read(), 1);
        });
    }

    /// Model 1c: With write-preferring `ModelRwLock`, a publisher blocked on a long `read_copy()`
    /// MUST NOT park on `pub_lock` and stall concurrent `read_txn()` readers!
    /// Here `t_copy` holds `read_copy()` until `t_reader` finishes `read_txn()` (synchronized via
    /// Condvar), while `t_pub` concurrently attempts `gate.write()`. If `write()` ever acquired
    /// `pub_lock` before `copy_lock` (or collapsed the two lanes), this deadlocks immediately!
    #[test]
    fn dbi_publish_gate_blocked_publisher_never_stalls_read_txn() {
        explore(|| {
            let scope = LockScopeId::fresh();
            let gate = Arc::new(DbiPublishGate::new_in_scope(scope));
            let reader_done = Arc::new((sync::Mutex::new(false), sync::Condvar::new()));

            // Hold `read_copy()` on the main thread before spawning publisher & reader so
            // `t_pub` is guaranteed to block waiting on `copy_lock` while `t_reader` runs!
            let copy_guard = gate.read_copy();

            let t_pub = {
                let gate = Arc::clone(&gate);
                thread::spawn(move || {
                    let _g = gate.write();
                })
            };

            let t_reader = {
                let gate = Arc::clone(&gate);
                let reader_done = Arc::clone(&reader_done);
                thread::spawn(move || {
                    let _r = gate.read_txn();
                    let (mu, cv) = &*reader_done;
                    let mut done = mu.lock().unwrap();
                    *done = true;
                    cv.notify_all();
                })
            };

            // Wait for `t_reader` to complete `read_txn()` BEFORE releasing `copy_guard`!
            {
                let (mu, cv) = &*reader_done;
                let mut done = mu.lock().unwrap();
                while !*done {
                    done = cv.wait(done).unwrap();
                }
            }
            drop(copy_guard);

            t_reader.join().unwrap();
            t_pub.join().unwrap();
        });
    }

    /// Model 1d: Cross-database nested locks with distinct `LockScopeId`s never trigger false
    /// lock-order panics even under interleaved concurrent threads sharing the same Loom/Shuttle host.
    #[test]
    fn distinct_scopes_per_simulated_thread_never_cross_contaminate() {
        explore(|| {
            let scope_a = LockScopeId::fresh();
            let scope_b = LockScopeId::fresh();
            let a_writer = Arc::new(WriterLock::new_in_scope(scope_a, 0u64));
            let b_schema = Arc::new(SchemaEpochLock::new_in_scope(scope_b, 0u64));
            let b_meta = Arc::new(MetaLock::new_in_scope(scope_b, 0u64));

            let t1 = {
                let a_writer = Arc::clone(&a_writer);
                let b_schema = Arc::clone(&b_schema);
                let b_meta = Arc::clone(&b_meta);
                thread::spawn(move || {
                    let mut wa = a_writer.lock();
                    let _sb = b_schema.read();
                    let mut mb = b_meta.lock();
                    *wa += 1;
                    *mb += 1;
                })
            };

            let t2 = {
                let b_schema = Arc::clone(&b_schema);
                let b_meta = Arc::clone(&b_meta);
                thread::spawn(move || {
                    let _sb = b_schema.read();
                    let mut mb = b_meta.lock();
                    *mb += 1;
                })
            };

            t1.join().unwrap();
            t2.join().unwrap();

            assert_eq!(*a_writer.lock(), 1);
            assert_eq!(*b_meta.lock(), 2);
        });
    }

    /// Model 1e: Holding a lock on a thread and attempting an inversion on that same simulated
    /// thread panics cleanly even when a sibling thread concurrently holds locks in the same scope.
    #[test]
    fn same_scope_inversion_panics_under_model_tracker() {
        explore(|| {
            let scope = LockScopeId::fresh();
            let l1 = Arc::new(SchemaEpochLock::new_in_scope(scope, ()));
            let l2 = Arc::new(MapGate::new_in_scope(scope, ()));
            let l3 = Arc::new(WriterLock::new_in_scope(scope, ()));

            let sibling = {
                let l1 = Arc::clone(&l1);
                let l2 = Arc::clone(&l2);
                thread::spawn(move || {
                    let _g1 = l1.read();
                    let _g2 = l2.read();
                })
            };

            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _g3 = l3.lock();
                let _g2 = l2.read();
            }));
            sibling.join().unwrap();

            let payload = caught.expect_err("lock inversion must panic");
            let msg = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("");
            assert!(msg.contains("lock order violation"), "unexpected panic message: {msg:?}");
        });
    }
}
