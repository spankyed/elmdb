//! Closing what is open as Node ends. Node runs no destructor as a process exits, so a database still open then would
//! never close: LMDB removes its lock's named semaphores (macOS) as the last process closes, and a database left open
//! at a path no one opens again keeps them until the machine restarts, where a table of them fills.
//!
//! - **A process exit** — a natural end or `process.exit` — runs C's `exit`, which runs the `atexit` handler this
//!   registers after every JavaScript `exit` listener, whatever a program did to those: it closes every database the
//!   process still has open.
//! - **A worker thread's end** runs its environment's cleanup hook, which closes the databases that environment opened.
//!
//! A signal that kills the process runs neither. A database a unit still holds is left as it is, since the unit's
//! transaction is the unit's to end; a later open at the same path clears what it left.

use std::os::raw::c_int;
use std::sync::{Arc, Mutex, Once, Weak};
use std::time::Duration;

use napi::bindgen_prelude::*;

type Opened = Arc<Mutex<Vec<Weak<elmdb::Database>>>>;

/// How long a close waits for an operation still holding a database before leaving it as it is
const CLOSE_WAIT: Duration = Duration::from_secs(1);

/// Every database this binary opened in the process, for the exit handler
static PROCESS: Mutex<Vec<Weak<elmdb::Database>>> = Mutex::new(Vec::new());
static AT_EXIT: Once = Once::new();

unsafe extern "C" {
    fn atexit(handler: extern "C" fn()) -> c_int;
}

fn close_all(open: Vec<Weak<elmdb::Database>>) {
    for db in open.iter().filter_map(Weak::upgrade) {
        let _ = db.close_within(Some(CLOSE_WAIT));
    }
}

extern "C" fn close_at_process_exit() {
    // A lock poisoned by a panic elsewhere still holds the list
    let open = std::mem::take(&mut *PROCESS.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    close_all(open);
}

/// The environment's list of the databases it opened, made with its cleanup hook on its first open
fn opened(env: &Env) -> Result<Opened> {
    if let Some(opened) = env.get_instance_data::<Opened>()? {
        return Ok(Arc::clone(opened));
    }
    let opened: Opened = Arc::default();
    env.set_instance_data(Arc::clone(&opened), (), |_| {})?;
    let at_teardown = Arc::clone(&opened);
    env.add_env_cleanup_hook((), move |()| {
        let open = std::mem::take(&mut *at_teardown.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        close_all(open);
    })?;
    Ok(opened)
}

/// Keeps `db` to be closed as its environment or the process ends, if it is still open then
pub fn close_at_exit(env: &Env, db: &Arc<elmdb::Database>) -> Result<()> {
    AT_EXIT.call_once(|| {
        // SAFETY: `close_at_process_exit` is a plain function that neither unwinds nor calls back into the runtime
        // `exit` is tearing down; registering it has no other effect
        unsafe {
            atexit(close_at_process_exit);
        }
    });
    let opened = opened(env)?;
    for list in [
        &mut *opened.lock().expect("the opened list's lock is never poisoned"),
        &mut *PROCESS.lock().expect("the process list's lock is never poisoned"),
    ] {
        list.retain(|held| held.strong_count() > 0);
        list.push(Arc::downgrade(db));
    }
    Ok(())
}
