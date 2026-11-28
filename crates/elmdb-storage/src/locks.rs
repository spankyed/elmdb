//! Strictly ordered 5-domain non-recursive lock hierarchy and concurrency-modelable sync primitives:
//!
//! ```text
//! L1: SchemaEpochLock (RwLock — read-mostly; NEVER held during index drops or builds)
//! L2: MapGate         (RwLock — only write-locked on Windows growth or >16 GiB Unix overflow)
//! L3: WriterLock      (Mutex  — serializes LMDB write transactions + DurabilityGate)
//! L4: DbiPublishGate  (RwLock — serializes rare LMDB `me_numdbs`/`me_dbflags` publication vs `begin_read`/`copy_compacted`)
//! L5: MetaLock        (Mutex/RwLock — short O(us) critical sections; NEVER held across I/O or queries)
//! ```
//!
//! In debug/test builds (and always under `--cfg loom` / `--cfg shuttle`), every acquisition asserts
//! strict ascending order (`domain > max_held_domain`) on the current thread so lock-order inversions
//! and accidental recursive acquisitions panic immediately.
//!
//! All concurrency-modelable primitives (`sync::*`) live here so `cargo xtask check-sync` enforces a
//! single choke point across the workspace. Under default/release builds, `parking_lot` and `std::sync`
//! are used directly with zero overhead; under `--cfg loom` or `--cfg shuttle`, Loom/Shuttle replace
//! the underlying primitives and thread-locals.

use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(not(any(loom, shuttle)))]
use std::sync::Arc;

#[cfg(not(any(loom, shuttle)))]
use parking_lot::{
    ArcMutexGuard, ArcRwLockReadGuard, Mutex, MutexGuard, RawMutex, RawRwLock, RwLock, RwLockReadGuard,
    RwLockWriteGuard,
};

/// Switchable synchronization primitives (`std`/`parking_lot` in normal builds, Loom under `--cfg loom`,
/// Shuttle under `--cfg shuttle`).
pub mod sync {
    #[cfg(not(any(loom, shuttle)))]
    pub use parking_lot::{Mutex as PlainMutex, MutexGuard as PlainMutexGuard};
    #[cfg(not(any(loom, shuttle)))]
    pub use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    #[cfg(not(any(loom, shuttle)))]
    pub use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
    #[cfg(not(any(loom, shuttle)))]
    pub use std::thread_local;

    #[cfg(loom)]
    pub use super::model::{PlainMutex, PlainMutexGuard};
    #[cfg(loom)]
    pub use loom::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    #[cfg(loom)]
    pub use loom::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
    #[cfg(loom)]
    pub use loom::thread_local;

    #[cfg(shuttle)]
    pub use super::model::{PlainMutex, PlainMutexGuard};
    #[cfg(shuttle)]
    pub use shuttle::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
    #[cfg(shuttle)]
    pub use shuttle::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
    #[cfg(shuttle)]
    pub use shuttle::thread_local;
}

#[cfg(any(loom, shuttle))]
#[path = "locks_model.rs"]
pub mod model;

#[cfg(any(loom, shuttle))]
use model::{ModelMutex, ModelMutexGuard, ModelRwLock, ModelRwReadGuard, ModelRwWriteGuard};

/// Identifies an independent lock-hierarchy instance (typically one `Database` / `KvStore`).
///
/// Lock ordering (`L1 < L2 < L3 < L4 < L5`) is enforced strictly within each `LockScopeId`,
/// allowing valid cross-database nested calls (e.g. `db1.transact(|| db2.get(...))`) in debug
/// builds while still panicking immediately on any same-database inversion or recursion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct LockScopeId(u64);

impl LockScopeId {
    pub const DEFAULT: Self = Self(0);

    pub fn fresh() -> Self {
        static NEXT_SCOPE_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_SCOPE_ID.fetch_add(1, Ordering::Relaxed))
    }
}

/// Lock hierarchy rank. Acquiring a lock with rank `<=` any rank currently held
/// on the calling thread in the same `LockScopeId` is a lock-order violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum LockDomain {
    SchemaEpoch = 1,
    MapGate = 2,
    Writer = 3,
    DbiPublish = 4,
    Meta = 5,
}

impl LockDomain {
    #[cfg(any(debug_assertions, loom, shuttle))]
    const fn bit(self) -> u8 {
        1u8 << (self as u8)
    }
}

#[cfg(any(debug_assertions, loom, shuttle))]
mod tracker {
    use std::cell::RefCell;

    use super::{LockDomain, LockScopeId};

    #[cfg(not(any(loom, shuttle)))]
    std::thread_local! {
        // Per-scope bitmasks currently held by this thread (typically 0..2 entries).
        static HELD_SCOPES: RefCell<Vec<(LockScopeId, u8)>> = const { RefCell::new(Vec::new()) };
    }

    #[cfg(loom)]
    loom::thread_local! {
        static HELD_SCOPES: RefCell<Vec<(LockScopeId, u8)>> = RefCell::new(Vec::new());
    }

    #[cfg(shuttle)]
    shuttle::thread_local! {
        static HELD_SCOPES: RefCell<Vec<(LockScopeId, u8)>> = RefCell::new(Vec::new());
    }

    pub fn acquire(scope: LockScopeId, domain: LockDomain) {
        HELD_SCOPES.with(|cell| {
            let mut scopes = cell.borrow_mut();
            let bit = domain.bit();
            if let Some((_, mask)) = scopes.iter_mut().find(|(id, _)| *id == scope) {
                // Reject any lock of equal or higher rank already held in this scope (no recursion, no inversion).
                let higher_or_equal = *mask & !(bit - 1);
                assert!(
                    higher_or_equal == 0,
                    "lock order violation in {scope:?}: attempting to acquire {domain:?} while holding mask {:#06b}",
                    *mask
                );
                *mask |= bit;
            } else {
                scopes.push((scope, bit));
            }
        });
    }

    pub fn release(scope: LockScopeId, domain: LockDomain) {
        HELD_SCOPES.with(|cell| {
            let mut scopes = cell.borrow_mut();
            let bit = domain.bit();
            let idx = scopes.iter().position(|(id, _)| *id == scope);
            debug_assert!(idx.is_some(), "releasing unheld lock domain {domain:?} in {scope:?}");
            if let Some(i) = idx {
                let mask = &mut scopes[i].1;
                debug_assert!(*mask & bit != 0, "releasing unheld lock domain {domain:?} in {scope:?}");
                *mask &= !bit;
                if *mask == 0 {
                    scopes.swap_remove(i);
                }
            }
        });
    }
}

#[cfg(not(any(debug_assertions, loom, shuttle)))]
mod tracker {
    use super::{LockDomain, LockScopeId};

    #[inline(always)]
    pub fn acquire(_scope: LockScopeId, _domain: LockDomain) {}

    #[inline(always)]
    pub fn release(_scope: LockScopeId, _domain: LockDomain) {}
}

/// `!Send` token ensuring domain guards are always released on the acquiring thread,
/// even if `parking_lot/send_guard` is ever enabled in the workspace.
struct DomainToken {
    scope: LockScopeId,
    domain: LockDomain,
    _not_send: PhantomData<*const ()>,
}

impl DomainToken {
    fn enter(scope: LockScopeId, domain: LockDomain) -> Self {
        tracker::acquire(scope, domain);
        Self { scope, domain, _not_send: PhantomData }
    }
}

impl Drop for DomainToken {
    fn drop(&mut self) {
        tracker::release(self.scope, self.domain);
    }
}

/// Read guard for a domain-checked `RwLock`.
pub struct OrderedReadGuard<'a, T> {
    #[cfg(not(any(loom, shuttle)))]
    guard: RwLockReadGuard<'a, T>,
    #[cfg(any(loom, shuttle))]
    guard: ModelRwReadGuard<T>,
    #[cfg(any(loom, shuttle))]
    _marker: PhantomData<&'a T>,
    _token: DomainToken,
}

impl<T> Deref for OrderedReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

/// Write guard for a domain-checked `RwLock`.
pub struct OrderedWriteGuard<'a, T> {
    #[cfg(not(any(loom, shuttle)))]
    guard: RwLockWriteGuard<'a, T>,
    #[cfg(any(loom, shuttle))]
    guard: ModelRwWriteGuard<T>,
    #[cfg(any(loom, shuttle))]
    _marker: PhantomData<&'a mut T>,
    _token: DomainToken,
}

impl<T> Deref for OrderedWriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<T> DerefMut for OrderedWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

/// Guard for a domain-checked `Mutex`.
pub struct OrderedMutexGuard<'a, T> {
    #[cfg(not(any(loom, shuttle)))]
    guard: MutexGuard<'a, T>,
    #[cfg(any(loom, shuttle))]
    guard: ModelMutexGuard<T>,
    #[cfg(any(loom, shuttle))]
    _marker: PhantomData<&'a mut T>,
    _token: DomainToken,
}

impl<T> Deref for OrderedMutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<T> DerefMut for OrderedMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

/// L1: Read-mostly lock guarding schema epoch transitions.
#[derive(Debug, Default)]
pub struct SchemaEpochLock<T> {
    #[cfg(not(any(loom, shuttle)))]
    lock: RwLock<T>,
    #[cfg(any(loom, shuttle))]
    lock: ModelRwLock<T>,
    scope: LockScopeId,
}

impl<T> SchemaEpochLock<T> {
    pub fn new(value: T) -> Self {
        Self::new_in_scope(LockScopeId::DEFAULT, value)
    }

    pub fn new_in_scope(scope: LockScopeId, value: T) -> Self {
        #[cfg(not(any(loom, shuttle)))]
        let lock = RwLock::new(value);
        #[cfg(any(loom, shuttle))]
        let lock = ModelRwLock::new(value);
        Self { lock, scope }
    }

    pub fn read(&self) -> OrderedReadGuard<'_, T> {
        let token = DomainToken::enter(self.scope, LockDomain::SchemaEpoch);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedReadGuard { guard: self.lock.read(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedReadGuard { guard: self.lock.read_owned(), _marker: PhantomData, _token: token }
    }

    pub fn write(&self) -> OrderedWriteGuard<'_, T> {
        let token = DomainToken::enter(self.scope, LockDomain::SchemaEpoch);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedWriteGuard { guard: self.lock.write(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedWriteGuard { guard: self.lock.write_owned(), _marker: PhantomData, _token: token }
    }
}

/// Owned `'static` (but `!Send`) read guard for a domain-checked `RwLock`.
pub struct OrderedArcReadGuard<T> {
    #[cfg(not(any(loom, shuttle)))]
    guard: ArcRwLockReadGuard<RawRwLock, T>,
    #[cfg(any(loom, shuttle))]
    guard: ModelRwReadGuard<T>,
    _token: DomainToken,
}

impl<T> Deref for OrderedArcReadGuard<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

/// L2: Non-recursive gate guarding LMDB map growth.
#[derive(Debug, Default)]
pub struct MapGate<T> {
    #[cfg(not(any(loom, shuttle)))]
    lock: Arc<RwLock<T>>,
    #[cfg(any(loom, shuttle))]
    lock: ModelRwLock<T>,
    scope: LockScopeId,
}

impl<T> MapGate<T> {
    pub fn new(value: T) -> Self {
        Self::new_in_scope(LockScopeId::DEFAULT, value)
    }

    pub fn new_in_scope(scope: LockScopeId, value: T) -> Self {
        #[cfg(not(any(loom, shuttle)))]
        let lock = Arc::new(RwLock::new(value));
        #[cfg(any(loom, shuttle))]
        let lock = ModelRwLock::new(value);
        Self { lock, scope }
    }

    pub fn read(&self) -> OrderedReadGuard<'_, T> {
        let token = DomainToken::enter(self.scope, LockDomain::MapGate);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedReadGuard { guard: self.lock.read(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedReadGuard { guard: self.lock.read_owned(), _marker: PhantomData, _token: token }
    }

    pub fn read_arc(&self) -> OrderedArcReadGuard<T> {
        let token = DomainToken::enter(self.scope, LockDomain::MapGate);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedArcReadGuard { guard: self.lock.read_arc(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedArcReadGuard { guard: self.lock.read_owned(), _token: token }
    }

    pub fn write(&self) -> OrderedWriteGuard<'_, T> {
        let token = DomainToken::enter(self.scope, LockDomain::MapGate);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedWriteGuard { guard: self.lock.write(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedWriteGuard { guard: self.lock.write_owned(), _marker: PhantomData, _token: token }
    }
}

/// Owned `'static` (but `!Send`) guard for a domain-checked `Mutex`.
pub struct OrderedArcMutexGuard<T> {
    #[cfg(not(any(loom, shuttle)))]
    guard: ArcMutexGuard<RawMutex, T>,
    #[cfg(any(loom, shuttle))]
    guard: ModelMutexGuard<T>,
    _token: DomainToken,
}

impl<T> Deref for OrderedArcMutexGuard<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<T> DerefMut for OrderedArcMutexGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

/// L3: Exclusive writer serialization lock (LMDB write txn + `DurabilityGate`).
#[derive(Debug, Default)]
pub struct WriterLock<T> {
    #[cfg(not(any(loom, shuttle)))]
    lock: Arc<Mutex<T>>,
    #[cfg(any(loom, shuttle))]
    lock: ModelMutex<T>,
    scope: LockScopeId,
}

impl<T> WriterLock<T> {
    pub fn new(value: T) -> Self {
        Self::new_in_scope(LockScopeId::DEFAULT, value)
    }

    pub fn new_in_scope(scope: LockScopeId, value: T) -> Self {
        #[cfg(not(any(loom, shuttle)))]
        let lock = Arc::new(Mutex::new(value));
        #[cfg(any(loom, shuttle))]
        let lock = ModelMutex::new(value);
        Self { lock, scope }
    }

    pub fn lock(&self) -> OrderedMutexGuard<'_, T> {
        let token = DomainToken::enter(self.scope, LockDomain::Writer);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedMutexGuard { guard: self.lock.lock(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedMutexGuard { guard: self.lock.lock_owned(), _marker: PhantomData, _token: token }
    }

    pub fn lock_arc(&self) -> OrderedArcMutexGuard<T> {
        let token = DomainToken::enter(self.scope, LockDomain::Writer);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedArcMutexGuard { guard: self.lock.lock_arc(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedArcMutexGuard { guard: self.lock.lock_owned(), _token: token }
    }
}

/// Exclusive write guard for `DbiPublishGate`: holds `copy_lock.write()` then `pub_lock.write()`
/// so a writer waiting on a long `copy_compacted` backup never blocks `LmdbRoTxn::begin()`!
pub struct DbiPublishWriteGuard<'a> {
    #[cfg(not(any(loom, shuttle)))]
    _pub_guard: RwLockWriteGuard<'a, ()>,
    #[cfg(not(any(loom, shuttle)))]
    _copy_guard: RwLockWriteGuard<'a, ()>,
    #[cfg(any(loom, shuttle))]
    _pub_guard: ModelRwWriteGuard<()>,
    #[cfg(any(loom, shuttle))]
    _copy_guard: ModelRwWriteGuard<()>,
    #[cfg(any(loom, shuttle))]
    _marker: PhantomData<&'a ()>,
    _token: DomainToken,
}

/// L4: Dual-lane barrier serializing rare LMDB `env->me_numdbs`/`env->me_dbflags` publication
/// (`commit_raw` when opening new sub-DBs and `ensure_subdbs_open`) against BOTH short-lived
/// `mdb_txn_begin(RDONLY)` (`read_txn`) and long-running `mdb_env_copy2` (`read_copy`).
///
/// Writers acquire `copy_lock.write()` BEFORE `pub_lock.write()`, so a writer waiting on a
/// streaming backup never parks on `pub_lock` and therefore **never stalls concurrent readers**,
/// while staying separate from L5 `MetaRwLock` (`DbiCache`) so routine writes and `lookup_flags`
/// never contend with backups either.
#[derive(Debug, Default)]
pub struct DbiPublishGate {
    #[cfg(not(any(loom, shuttle)))]
    copy_lock: RwLock<()>,
    #[cfg(not(any(loom, shuttle)))]
    pub_lock: RwLock<()>,
    #[cfg(any(loom, shuttle))]
    copy_lock: ModelRwLock<()>,
    #[cfg(any(loom, shuttle))]
    pub_lock: ModelRwLock<()>,
    scope: LockScopeId,
}

impl DbiPublishGate {
    pub fn new() -> Self {
        Self::new_in_scope(LockScopeId::DEFAULT)
    }

    pub fn new_in_scope(scope: LockScopeId) -> Self {
        #[cfg(not(any(loom, shuttle)))]
        return Self { copy_lock: RwLock::new(()), pub_lock: RwLock::new(()), scope };
        #[cfg(any(loom, shuttle))]
        Self { copy_lock: ModelRwLock::new(()), pub_lock: ModelRwLock::new(()), scope }
    }

    /// Short-lived reader barrier for `mdb_txn_begin(..., MDB_RDONLY)`.
    pub fn read_txn(&self) -> OrderedReadGuard<'_, ()> {
        let token = DomainToken::enter(self.scope, LockDomain::DbiPublish);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedReadGuard { guard: self.pub_lock.read(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedReadGuard { guard: self.pub_lock.read_owned(), _marker: PhantomData, _token: token }
    }

    /// Long-lived reader barrier for `mdb_env_copy2(..., MDB_CP_COMPACT)`.
    pub fn read_copy(&self) -> OrderedReadGuard<'_, ()> {
        let token = DomainToken::enter(self.scope, LockDomain::DbiPublish);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedReadGuard { guard: self.copy_lock.read(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedReadGuard { guard: self.copy_lock.read_owned(), _marker: PhantomData, _token: token }
    }

    /// Exclusive writer barrier for `mdb_dbis_update` (`commit_raw` with new sub-DBs / `ensure_subdbs_open`).
    pub fn write(&self) -> DbiPublishWriteGuard<'_> {
        let token = DomainToken::enter(self.scope, LockDomain::DbiPublish);
        #[cfg(not(any(loom, shuttle)))]
        {
            let copy_guard = self.copy_lock.write();
            let pub_guard = self.pub_lock.write();
            DbiPublishWriteGuard { _pub_guard: pub_guard, _copy_guard: copy_guard, _token: token }
        }
        #[cfg(any(loom, shuttle))]
        {
            let copy_guard = self.copy_lock.write_owned();
            let pub_guard = self.pub_lock.write_owned();
            DbiPublishWriteGuard { _pub_guard: pub_guard, _copy_guard: copy_guard, _marker: PhantomData, _token: token }
        }
    }
}

/// L5: Short-lived metadata / invalidation / scheduler mutex.
#[derive(Debug, Default)]
pub struct MetaLock<T> {
    #[cfg(not(any(loom, shuttle)))]
    lock: Mutex<T>,
    #[cfg(any(loom, shuttle))]
    lock: ModelMutex<T>,
    scope: LockScopeId,
}

impl<T> MetaLock<T> {
    pub fn new(value: T) -> Self {
        Self::new_in_scope(LockScopeId::DEFAULT, value)
    }

    pub fn new_in_scope(scope: LockScopeId, value: T) -> Self {
        #[cfg(not(any(loom, shuttle)))]
        let lock = Mutex::new(value);
        #[cfg(any(loom, shuttle))]
        let lock = ModelMutex::new(value);
        Self { lock, scope }
    }

    pub fn lock(&self) -> OrderedMutexGuard<'_, T> {
        let token = DomainToken::enter(self.scope, LockDomain::Meta);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedMutexGuard { guard: self.lock.lock(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedMutexGuard { guard: self.lock.lock_owned(), _marker: PhantomData, _token: token }
    }

    /// Replaces the inner value under `&mut self` (works in both normal and model builds).
    pub fn take_unlocked(&mut self) -> T
    where
        T: Default,
    {
        #[cfg(not(any(loom, shuttle)))]
        return std::mem::take(self.lock.get_mut());
        #[cfg(any(loom, shuttle))]
        return std::mem::take(&mut *self.lock.lock_owned());
    }
}

/// L5: Short-lived read-mostly metadata `RwLock` (e.g. LMDB `MDB_dbi` table).
#[derive(Debug, Default)]
pub struct MetaRwLock<T> {
    #[cfg(not(any(loom, shuttle)))]
    lock: RwLock<T>,
    #[cfg(any(loom, shuttle))]
    lock: ModelRwLock<T>,
    scope: LockScopeId,
}

impl<T> MetaRwLock<T> {
    pub fn new(value: T) -> Self {
        Self::new_in_scope(LockScopeId::DEFAULT, value)
    }

    pub fn new_in_scope(scope: LockScopeId, value: T) -> Self {
        #[cfg(not(any(loom, shuttle)))]
        let lock = RwLock::new(value);
        #[cfg(any(loom, shuttle))]
        let lock = ModelRwLock::new(value);
        Self { lock, scope }
    }

    pub fn read(&self) -> OrderedReadGuard<'_, T> {
        let token = DomainToken::enter(self.scope, LockDomain::Meta);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedReadGuard { guard: self.lock.read(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedReadGuard { guard: self.lock.read_owned(), _marker: PhantomData, _token: token }
    }

    pub fn write(&self) -> OrderedWriteGuard<'_, T> {
        let token = DomainToken::enter(self.scope, LockDomain::Meta);
        #[cfg(not(any(loom, shuttle)))]
        return OrderedWriteGuard { guard: self.lock.write(), _token: token };
        #[cfg(any(loom, shuttle))]
        OrderedWriteGuard { guard: self.lock.write_owned(), _marker: PhantomData, _token: token }
    }
}

#[cfg(all(test, not(any(loom, shuttle))))]
mod tests {
    use super::*;

    #[test]
    fn strictly_ascending_order_succeeds() {
        let l1 = SchemaEpochLock::new(1);
        let l2 = MapGate::new(2);
        let l3 = WriterLock::new(3);
        let l4 = MetaLock::new(4);

        let g1 = l1.read();
        let g2 = l2.read();
        let g3 = l3.lock();
        let g4 = l4.lock();
        assert_eq!(*g1 + *g2 + *g3 + *g4, 10);
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn inverted_lock_order_panics_in_debug() {
        let l3 = WriterLock::new(3);
        let l2 = MapGate::new(2);
        let _g3 = l3.lock();
        let _g2 = l2.read();
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn recursive_same_domain_panics_in_debug() {
        let l4a = MetaLock::new(1);
        let l4b = MetaLock::new(2);
        let _g4a = l4a.lock();
        let _g4b = l4b.lock();
    }

    #[test]
    fn distinct_scopes_allow_cross_database_nested_locks() {
        let scope_a = LockScopeId::fresh();
        let scope_b = LockScopeId::fresh();
        assert_ne!(scope_a, scope_b);

        let a_map = MapGate::new_in_scope(scope_a, ());
        let a_writer = WriterLock::new_in_scope(scope_a, ());
        let b_schema = SchemaEpochLock::new_in_scope(scope_b, ());
        let b_map = MapGate::new_in_scope(scope_b, ());
        let b_pub = DbiPublishGate::new_in_scope(scope_b);
        let b_meta = MetaRwLock::new_in_scope(scope_b, 42u32);

        // Holding L2+L3 on database A while performing full L1 -> L2 -> L4 -> L5 read on database B must succeed.
        let _ga2 = a_map.read();
        let _ga3 = a_writer.lock();
        let _gb1 = b_schema.read();
        let _gb2 = b_map.read();
        let _gb4 = b_pub.read_txn();
        let gb5 = b_meta.read();
        assert_eq!(*gb5, 42);
    }

    #[test]
    #[should_panic(expected = "lock order violation")]
    fn same_fresh_scope_inversion_still_panics_in_debug() {
        let scope = LockScopeId::fresh();
        let l3 = WriterLock::new_in_scope(scope, ());
        let l2 = MapGate::new_in_scope(scope, ());
        let _g3 = l3.lock();
        let _g2 = l2.read();
    }

    #[test]
    fn meta_lock_take_unlocked_extracts_value() {
        let mut lock = MetaLock::new(Some(42u32));
        assert_eq!(lock.take_unlocked(), Some(42));
        assert_eq!(lock.take_unlocked(), None);
    }

    #[test]
    fn dbi_publish_gate_new_in_scope_isolates_from_default_scope() {
        let default_pub = DbiPublishGate::new();
        let fresh_pub = DbiPublishGate::new_in_scope(LockScopeId::fresh());
        // Holding L4 on DEFAULT scope while acquiring L4 on a fresh scope must succeed (proving `new_in_scope`
        // actually stores the caller's scope rather than `LockScopeId::DEFAULT`).
        let _g_default = default_pub.read_txn();
        let _g_fresh = fresh_pub.read_txn();
    }
}
