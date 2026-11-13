//! A panic in elmdb stopped at the binding. A panic that unwinds out of an export crosses into Node's C++, which
//! aborts the process; so every export runs its work here, and a panic becomes a JavaScript error whose code is
//! `PANICKED`, naming the export, where the panic was raised and what it said. napi-rs's own `catch_unwind` answers
//! its `GenericFailure`, which is no code of elmdb's.
//!
//! A panic is a bug in elmdb whatever raised it, and what it left in memory is not known. So the database handle it
//! was raised through refuses every call after it but `close` and what asks nothing of the engine, with the same
//! code, and a unit the handle had open is ended, keeping none of its writes. What was committed before is on disk
//! as it was: a transaction a panic unwound through is aborted. The handle closed and the database opened again is
//! a database as any other.
//!
//! What V8 collects is dropped here too: the last hold on a database closes it, and an open unit is aborted, in a
//! finalizer a panic would unwind out of into Node.
//!
//! A test asks for a panic in an export by starting Node with `ELMDB_TEST_PANIC` set to its name, as its errors
//! name it (`Database.readSync`): that export then panics the first time it is called
//! (`docs/architecture/testing.md`, The binding). Nothing else reads the variable, it is read once, and a value
//! that names no export asks for nothing.

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once, OnceLock};

use crate::{Code, to_js};

thread_local! {
    /// Where this thread's last panic was raised, which its payload doesn't say
    static RAISED_AT: RefCell<Option<String>> = const { RefCell::new(None) };
}

static HOOKED: Once = Once::new();

/// Keeps where each panic is raised, for the error that reports it, and hands the panic on to the hook there was
fn keep_locations() {
    HOOKED.call_once(|| {
        let before = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let at = info.location().map(ToString::to_string);
            // A thread being torn down has no thread-local to keep it in
            let _ = RAISED_AT.try_with(|raised| *raised.borrow_mut() = at);
            before(info);
        }));
    });
}

/// What a panic's payload says: the message of a `panic!`, an `expect` or an `unwrap`, which is a string of one kind
/// or the other
fn said(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|said| (*said).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic that says nothing".to_owned())
}

/// Runs `work`, answering a panic in it as elmdb's `PANICKED`, which says `what` panicked
pub fn caught<T>(what: &str, work: impl FnOnce() -> T) -> Result<T, elmdb::Error> {
    keep_locations();
    // What an earlier panic left, one caught before it reached here, is not this one's
    let _ = RAISED_AT.try_with(|raised| raised.borrow_mut().take());
    catch_unwind(AssertUnwindSafe(work)).map_err(|payload| {
        let said = said(&*payload);
        // A payload whose own drop panics is left undropped, which leaks it
        if let Err(dropping) = catch_unwind(AssertUnwindSafe(move || drop(payload))) {
            std::mem::forget(dropping);
        }
        let at = RAISED_AT.try_with(|raised| raised.borrow_mut().take()).ok().flatten();
        let at = at.map_or_else(String::new, |at| format!(" at {at}"));
        elmdb::Error::new(
            elmdb::ErrorCode::Panicked,
            format!("{what} panicked{at}: {said}. This is a bug in elmdb: report it with this message"),
        )
    })
}

/// Panics when a test asked for a panic in `export` and it hasn't panicked yet
fn asked_for(export: &'static str) {
    static ASKED: OnceLock<Option<(String, AtomicBool)>> = OnceLock::new();
    let asked = ASKED.get_or_init(|| {
        let named = std::env::var_os("ELMDB_TEST_PANIC")?.into_string().ok()?;
        Some((named, AtomicBool::new(false)))
    });
    if let Some((named, raised)) = asked
        && named == export
        && !raised.swap(true, Ordering::SeqCst)
    {
        panic!("a panic a test asked for");
    }
}

/// `caught`, for an export that answers JavaScript itself and is of no database handle: its own failure, or the
/// panic's
pub fn guarded<T>(export: &'static str, work: impl FnOnce() -> napi::Result<T, Code>) -> napi::Result<T, Code> {
    let ran = caught(export, || {
        asked_for(export);
        work()
    });
    match ran {
        Ok(answered) => answered,
        Err(panicked) => Err(to_js(panicked)),
    }
}

/// Whether a database handle has panicked, and what the panic was: shared by the handle, its units and its promises
#[derive(Clone, Default)]
pub struct Health(Arc<OnceLock<String>>);

impl Health {
    /// The refusal of a call made after a panic, or none while there has been none
    pub fn refusal(&self, export: &str) -> Option<elmdb::Error> {
        self.0.get().map(|first| {
            elmdb::Error::new(
                elmdb::ErrorCode::Panicked,
                format!(
                    "{export} is refused: this database panicked earlier, and what that left in memory is not \
                     known. Close it and open it again: what it committed is kept. The panic: {first}"
                ),
            )
        })
    }

    /// Runs an export's work off the thread that asked for it: refused after a panic, and a panic in it the handle's
    /// first
    pub fn caught<T>(&self, export: &'static str, work: impl FnOnce() -> T) -> Result<T, elmdb::Error> {
        if let Some(refused) = self.refusal(export) {
            return Err(refused);
        }
        self.always(export, work)
    }

    /// Runs an export's work whatever the handle has been through, as a close is: a panic in it is the handle's
    /// first, if it has had none
    pub fn always<T>(&self, export: &'static str, work: impl FnOnce() -> T) -> Result<T, elmdb::Error> {
        let ran = caught(export, || {
            asked_for(export);
            work()
        });
        ran.map_err(|panicked| {
            let _ = self.0.set(panicked.message.clone());
            elmdb::Error::new(
                panicked.code,
                format!(
                    "{}. The call may not have finished, and this database refuses calls until it is closed and \
                     opened again",
                    panicked.message
                ),
            )
        })
    }

    /// `caught`, for an export that answers JavaScript itself
    pub fn guarded<T>(
        &self,
        export: &'static str,
        work: impl FnOnce() -> napi::Result<T, Code>,
    ) -> napi::Result<T, Code> {
        match self.caught(export, work) {
            Ok(answered) => answered,
            Err(panicked) => Err(to_js(panicked)),
        }
    }

    /// `always`, for an export that answers JavaScript itself
    pub fn unrefused<T>(
        &self,
        export: &'static str,
        work: impl FnOnce() -> napi::Result<T, Code>,
    ) -> napi::Result<T, Code> {
        match self.always(export, work) {
            Ok(answered) => answered,
            Err(panicked) => Err(to_js(panicked)),
        }
    }

    pub fn panicked(&self) -> bool {
        self.0.get().is_some()
    }
}

/// A hold on a database that is let go where a panic is caught: the last one closes the database, which V8 has run
/// in a finalizer and napi as a promise settles, and a panic there would unwind into Node
pub struct Held(ManuallyDrop<Arc<elmdb::Database>>);

impl Held {
    pub fn new(db: Arc<elmdb::Database>) -> Self {
        Held(ManuallyDrop::new(db))
    }

    pub fn again(&self) -> Self {
        Held::new(Arc::clone(&self.0))
    }
}

impl Deref for Held {
    type Target = elmdb::Database;

    fn deref(&self) -> &elmdb::Database {
        &self.0
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: the hold is taken out once, here, and `self.0` is not read after: this is the wrapper's drop, and
        // nothing else takes it
        let db = unsafe { ManuallyDrop::take(&mut self.0) };
        let _ = caught("letting go of a database", move || drop(db));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_is_an_error_that_says_what_panicked_where_and_what_it_said() {
        assert_eq!(caught("a sum", || 1 + 1).unwrap(), 2);
        // A message of either kind of string, as `panic!` with and without arguments raises
        for raise in [|| panic!("plainly"), || panic!("{}ly", "plain")] as [fn(); 2] {
            let error = caught("Database.write", raise).unwrap_err();
            assert_eq!(error.code, elmdb::ErrorCode::Panicked);
            let (before, after) = error.message.split_once(": plainly. ").expect(&error.message);
            assert!(before.starts_with("Database.write panicked at ") && before.contains("panics.rs:"), "{before}");
            assert_eq!(after, "This is a bug in elmdb: report it with this message");
        }
        // A payload that is no string says nothing, and one raised with no hook run has no place
        let error = caught("a close", || std::panic::panic_any(7)).unwrap_err();
        assert!(error.message.contains(": a panic that says nothing. "), "{}", error.message);
        let error = caught("a close", || std::panic::resume_unwind(Box::new("again"))).unwrap_err();
        assert!(error.message.starts_with("a close panicked: again. "), "{}", error.message);
        // Where a panic caught elsewhere was raised is not where the next one was
        keep_locations();
        assert!(catch_unwind(|| panic!("caught elsewhere")).is_err());
        let error = caught("a close", || std::panic::resume_unwind(Box::new("later"))).unwrap_err();
        assert!(error.message.starts_with("a close panicked: later. "), "{}", error.message);
    }

    #[test]
    fn a_hold_let_go_closes_its_database_once_the_last_is() {
        let dir = tempfile::tempdir().unwrap();
        let open = || elmdb::Database::open(dir.path(), elmdb::OpenOptions { syncs: false, ..Default::default() });
        let held = Held::new(Arc::new(open().unwrap()));
        let again = held.again();
        assert_eq!(again.path(), held.path());
        drop(held);
        assert_eq!(open().err().map(|error| error.code), Some(elmdb::ErrorCode::AlreadyOpen), "one hold is left");
        drop(again);
        assert!(open().is_ok());
    }

    #[test]
    fn a_handle_that_panicked_refuses_what_it_is_asked_but_what_always_runs() {
        let health = Health::default();
        assert_eq!(health.caught("Database.write", || 3).unwrap(), 3);
        assert!(!health.panicked() && health.refusal("Database.write").is_none());
        let first = health.caught("Database.write", || panic!("once")).unwrap_err();
        assert!(first.message.ends_with("refuses calls until it is closed and opened again"), "{}", first.message);
        assert!(health.panicked());
        // Refused without running, by the first panic, which a later one doesn't replace
        let mut ran = false;
        let refused = health.caught("Database.readSync", || ran = true).unwrap_err();
        assert!(!ran && refused.code == elmdb::ErrorCode::Panicked);
        assert!(refused.message.starts_with("Database.readSync is refused: "), "{}", refused.message);
        assert!(refused.message.contains("Database.write panicked at ") && refused.message.contains(": once. "));
        assert_eq!(health.always("Database.close", || 5).unwrap(), 5);
        health.always("Database.close", || panic!("twice")).unwrap_err();
        let still = health.clone().refusal("Unit.write").unwrap();
        assert!(still.message.contains(": once. ") && !still.message.contains("twice"), "{}", still.message);
    }
}
