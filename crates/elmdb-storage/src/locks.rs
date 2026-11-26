//! Strictly ordered 4-domain non-recursive lock hierarchy:
//!
//! ```text
//! L1: SchemaEpochLock (RwLock — read-mostly; NEVER held during index drops or builds)
//! L2: MapGate         (RwLock — only write-locked on Windows growth or >16 GiB Unix overflow)
//! L3: WriterLock      (Mutex  — serializes LMDB write transactions + DurabilityGate)
//! L4: MetaLock        (Mutex  — short O(us) critical sections; NEVER held across I/O or queries)
//! ```
//!
//! In debug/test builds, every acquisition asserts strict ascending order
//! (`domain > max_held_domain`) on the current thread so lock-order inversions
//! and accidental recursive acquisitions panic immediately.

use std::ops::{Deref, DerefMut};

use parking_lot::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Lock hierarchy rank. Acquiring a lock with rank `<=` any rank currently held
/// on the calling thread is a lock-order violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum LockDomain {
    SchemaEpoch = 1,
    MapGate = 2,
    Writer = 3,
    Meta = 4,
}

impl LockDomain {
    #[cfg(debug_assertions)]
    const fn bit(self) -> u8 {
        1u8 << (self as u8)
    }
}

#[cfg(debug_assertions)]
mod tracker {
    use std::cell::Cell;

    use super::LockDomain;

    thread_local! {
        static HELD_MASK: Cell<u8> = const { Cell::new(0) };
    }

    pub fn acquire(domain: LockDomain) {
        HELD_MASK.with(|cell| {
            let mask = cell.get();
            let bit = domain.bit();
            // Reject any lock of equal or higher rank already held (no recursion, no inversion).
            let higher_or_equal = mask & !(bit - 1);
            assert!(
                higher_or_equal == 0,
                "lock order violation: attempting to acquire {domain:?} while holding mask {mask:#06b}"
            );
            cell.set(mask | bit);
        });
    }

    pub fn release(domain: LockDomain) {
        HELD_MASK.with(|cell| {
            let mask = cell.get();
            debug_assert!(mask & domain.bit() != 0, "releasing unheld lock domain {domain:?}");
            cell.set(mask & !domain.bit());
        });
    }
}

#[cfg(not(debug_assertions))]
mod tracker {
    use super::LockDomain;

    #[inline(always)]
    pub fn acquire(_domain: LockDomain) {}

    #[inline(always)]
    pub fn release(_domain: LockDomain) {}
}

struct DomainToken(LockDomain);

impl DomainToken {
    fn enter(domain: LockDomain) -> Self {
        tracker::acquire(domain);
        Self(domain)
    }
}

impl Drop for DomainToken {
    fn drop(&mut self) {
        tracker::release(self.0);
    }
}

/// Read guard for a domain-checked `RwLock`.
pub struct OrderedReadGuard<'a, T> {
    guard: RwLockReadGuard<'a, T>,
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
    guard: RwLockWriteGuard<'a, T>,
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
    guard: MutexGuard<'a, T>,
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
pub struct SchemaEpochLock<T>(RwLock<T>);

impl<T> SchemaEpochLock<T> {
    pub const fn new(value: T) -> Self {
        Self(RwLock::new(value))
    }

    pub fn read(&self) -> OrderedReadGuard<'_, T> {
        let token = DomainToken::enter(LockDomain::SchemaEpoch);
        OrderedReadGuard { guard: self.0.read(), _token: token }
    }

    pub fn write(&self) -> OrderedWriteGuard<'_, T> {
        let token = DomainToken::enter(LockDomain::SchemaEpoch);
        OrderedWriteGuard { guard: self.0.write(), _token: token }
    }
}

/// L2: Non-recursive gate guarding LMDB map growth.
#[derive(Debug, Default)]
pub struct MapGate<T>(RwLock<T>);

impl<T> MapGate<T> {
    pub const fn new(value: T) -> Self {
        Self(RwLock::new(value))
    }

    pub fn read(&self) -> OrderedReadGuard<'_, T> {
        let token = DomainToken::enter(LockDomain::MapGate);
        OrderedReadGuard { guard: self.0.read(), _token: token }
    }

    pub fn write(&self) -> OrderedWriteGuard<'_, T> {
        let token = DomainToken::enter(LockDomain::MapGate);
        OrderedWriteGuard { guard: self.0.write(), _token: token }
    }
}

/// L3: Exclusive writer serialization lock (LMDB write txn + `DurabilityGate`).
#[derive(Debug, Default)]
pub struct WriterLock<T>(Mutex<T>);

impl<T> WriterLock<T> {
    pub const fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }

    pub fn lock(&self) -> OrderedMutexGuard<'_, T> {
        let token = DomainToken::enter(LockDomain::Writer);
        OrderedMutexGuard { guard: self.0.lock(), _token: token }
    }
}

/// L4: Short-lived metadata / invalidation / scheduler mutex.
#[derive(Debug, Default)]
pub struct MetaLock<T>(Mutex<T>);

impl<T> MetaLock<T> {
    pub const fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }

    pub fn lock(&self) -> OrderedMutexGuard<'_, T> {
        let token = DomainToken::enter(LockDomain::Meta);
        OrderedMutexGuard { guard: self.0.lock(), _token: token }
    }
}

#[cfg(test)]
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
}
