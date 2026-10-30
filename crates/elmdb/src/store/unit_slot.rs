//! Whether a unit holds the environment's one write transaction, and who may claim it. A unit reserves the slot before
//! it takes the writer; a plain write claims the writer while holding the slot's lock, so a unit can't reserve between
//! the write's check and its claim (Loom and Shuttle explore this: the model below, `docs/architecture/testing.md`,
//! Concurrency models).

use crate::error::{Error, ErrorCode, Result};
use crate::sync::{Condvar, Mutex};

pub(crate) struct UnitSlot {
    open: Mutex<bool>,
    ended: Condvar,
}

impl UnitSlot {
    pub(crate) fn new() -> Self {
        Self { open: Mutex::new(false), ended: Condvar::new() }
    }

    /// Reserves the slot for a unit, refusing a second
    pub(crate) fn reserve(&self) -> Result<()> {
        let mut open = self.open.lock().expect("the unit's lock is never poisoned");
        if *open {
            return Err(Error::new(ErrorCode::UnitOpen, "A unit is already open on this database"));
        }
        *open = true;
        Ok(())
    }

    /// Runs `take`, which takes the writer, with the slot's lock held, refusing while a unit is open
    pub(crate) fn claim_writer<T>(&self, take: impl FnOnce() -> T) -> Result<T> {
        let open = self.open.lock().expect("the unit's lock is never poisoned");
        if *open {
            return Err(Error::new(ErrorCode::UnitOpen, "A unit is open: write through it or end it first"));
        }
        let taken = take();
        drop(open);
        Ok(taken)
    }

    /// The unit ended, committed or not: whoever waits for the writer hears of it
    pub(crate) fn release(&self) {
        *self.open.lock().expect("the unit's lock is never poisoned") = false;
        self.ended.notify_all();
    }

    pub(crate) fn is_open(&self) -> bool {
        *self.open.lock().expect("the unit's lock is never poisoned")
    }

    /// Blocks while a unit is open, until it ends or `stop` answers true; a caller that makes `stop` true wakes it
    /// with `wake`
    pub(crate) fn wait_while_open(&self, stop: impl Fn() -> bool) {
        let mut open = self.open.lock().expect("the unit's lock is never poisoned");
        while *open && !stop() {
            open = self.ended.wait(open).expect("the unit's lock is never poisoned");
        }
    }

    /// Wakes every `wait_while_open`, to check its `stop` again
    pub(crate) fn wake(&self) {
        let _open = self.open.lock().expect("the unit's lock is never poisoned");
        self.ended.notify_all();
    }
}

/// The slot under a model's threads (`crate::sync::model`). LMDB's writer lock is the model's mutex here: the model is
/// the slot's decisions, not LMDB's or the operating system's locks
#[cfg(all(test, any(loom, shuttle)))]
mod model {
    use super::UnitSlot;
    use crate::sync::Mutex;
    use crate::sync::model::{Arc, AtomicBool, Ordering, explore, thread};

    /// A plain write and a unit race for the writer. A write that goes on to take the writer must find no unit
    /// reserved: one that was would have the write wait on the unit's writer, which it holds across calls
    #[test]
    fn a_write_never_waits_for_the_writer_while_a_unit_is_reserved() {
        explore(|| {
            let slot = Arc::new(UnitSlot::new());
            let writer = Arc::new(Mutex::new(()));
            let reserved = Arc::new(AtomicBool::new(false));

            let write = {
                let (slot, writer, reserved) = (slot.clone(), writer.clone(), reserved.clone());
                thread::spawn(move || {
                    let claimed = slot.claim_writer(|| {
                        assert!(!reserved.load(Ordering::SeqCst), "a write began taking the writer of a reserved unit");
                        writer.lock().unwrap()
                    });
                    drop(claimed);
                })
            };

            slot.reserve().unwrap();
            reserved.store(true, Ordering::SeqCst);
            let held = writer.lock().unwrap();
            reserved.store(false, Ordering::SeqCst);
            drop(held);
            slot.release();
            write.join().unwrap();
            assert!(!slot.is_open());
        });
    }

    /// Two units: exactly one reserves
    #[test]
    fn of_two_units_one_reserves() {
        explore(|| {
            let slot = Arc::new(UnitSlot::new());
            let other = {
                let slot = slot.clone();
                thread::spawn(move || slot.reserve().is_ok())
            };
            let mine = slot.reserve().is_ok();
            let theirs = other.join().unwrap();
            assert!(mine ^ theirs, "both or neither reserved: {mine} {theirs}");
        });
    }

    /// A waiter returns once the unit ends, and once woken after its stop is set, whichever interleaving
    #[test]
    fn a_waiter_returns_when_the_unit_ends_or_it_is_stopped() {
        explore(|| {
            let slot = Arc::new(UnitSlot::new());
            slot.reserve().unwrap();
            let waiter = {
                let slot = slot.clone();
                thread::spawn(move || slot.wait_while_open(|| false))
            };
            slot.release();
            waiter.join().unwrap();
        });
        explore(|| {
            let slot = Arc::new(UnitSlot::new());
            slot.reserve().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let waiter = {
                let (slot, stop) = (slot.clone(), stop.clone());
                thread::spawn(move || slot.wait_while_open(|| stop.load(Ordering::SeqCst)))
            };
            stop.store(true, Ordering::SeqCst);
            slot.wake();
            waiter.join().unwrap();
            assert!(slot.is_open(), "a stopped wait leaves the unit open");
        });
    }
}
