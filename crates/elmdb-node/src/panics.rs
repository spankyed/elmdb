//! A panic in elmdb stopped at the binding. A panic that unwinds out of an export crosses into Node's C++, which
//! aborts the process; so every export runs its work here, and a panic becomes a JavaScript error whose code is
//! `PANICKED`, naming the export, where the panic was raised and what it said. It is a bug in elmdb whatever raised
//! it, and the process lives to report it.
//!
//! What the panic unwound is released as any early return releases it: a transaction it held is aborted, and a lock
//! is let go. The call's own work is not done.
//!
//! A test asks for a panic in each export by starting Node with `ELMDB_TEST_PANICS` set: each export then panics the
//! first time it is called, and works after (`docs/architecture/testing.md`, The binding). Nothing else reads the
//! variable, and it is read once.

use std::cell::RefCell;
use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, Once, OnceLock, PoisonError};

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

/// The exports that have panicked for a test, when the process was started to have each do so once
fn asked_for() -> Option<&'static Mutex<HashSet<&'static str>>> {
    static ASKED: OnceLock<Option<Mutex<HashSet<&'static str>>>> = OnceLock::new();
    ASKED.get_or_init(|| std::env::var_os("ELMDB_TEST_PANICS").map(|_| Mutex::default())).as_ref()
}

/// Runs an export's work, answering a panic in it as elmdb's `PANICKED`
pub fn caught<T>(export: &'static str, work: impl FnOnce() -> T) -> Result<T, elmdb::Error> {
    keep_locations();
    let ran = catch_unwind(AssertUnwindSafe(|| {
        if let Some(asked) = asked_for()
            && asked.lock().unwrap_or_else(PoisonError::into_inner).insert(export)
        {
            panic!("a panic a test asked for");
        }
        work()
    }));
    ran.map_err(|payload| {
        let said = payload
            .downcast_ref::<&str>()
            .map(|said| (*said).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic that says nothing".to_owned());
        // A payload whose own drop panics would unwind out of here: it is left undropped
        std::mem::forget(payload);
        let at = RAISED_AT.try_with(|raised| raised.borrow_mut().take()).ok().flatten();
        let at = at.map_or_else(String::new, |at| format!(" at {at}"));
        elmdb::Error::new(
            elmdb::ErrorCode::Panicked,
            format!("{export} panicked{at}: {said}. This is a bug in elmdb, and the call did not finish"),
        )
    })
}

/// `caught`, for an export that answers JavaScript itself: its own failure, or the panic's
pub fn guarded<T>(export: &'static str, work: impl FnOnce() -> napi::Result<T, Code>) -> napi::Result<T, Code> {
    match caught(export, work) {
        Ok(answered) => answered,
        Err(panicked) => Err(to_js(panicked)),
    }
}
