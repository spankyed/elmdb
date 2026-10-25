//! Closing what is open as a Node environment ends. Node runs no destructor as a process exits, so a database still
//! open then would never close: LMDB removes its lock's named semaphores (macOS) as the last process closes, and a
//! database left open at a path no one opens again keeps them until the machine restarts, where a table of them fills.
//! Each environment keeps the databases it opened (its N-API instance data, so a worker's end closes only the worker's)
//! and closes those still open at `process.on('exit')`, which a natural end and `process.exit` both emit, and at its
//! cleanup hook, which a worker thread's end runs. A signal that kills the process runs neither.

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use napi::bindgen_prelude::*;

type Opened = Arc<Mutex<Vec<Weak<elmdb::Database>>>>;

/// `process.once(event, listener)`
type Once<'env> = Function<'env, FnArgs<(&'static str, Function<'env, (), ()>)>, Unknown<'env>>;

/// How long a close waits for what still holds a database (a unit left open) before leaving it as it is
const CLOSE_WAIT: Duration = Duration::from_secs(1);

fn close_all(opened: &Opened) {
    let open = std::mem::take(&mut *opened.lock().expect("the opened list's lock is never poisoned"));
    for db in open.iter().filter_map(Weak::upgrade) {
        let _ = db.close_within(Some(CLOSE_WAIT));
    }
}

/// The environment's list of the databases it opened, made on its first open with the listeners that close them
fn opened(env: &Env) -> Result<Opened> {
    if let Some(opened) = env.get_instance_data::<Opened>()? {
        return Ok(Arc::clone(opened));
    }
    let opened: Opened = Arc::default();
    env.set_instance_data(Arc::clone(&opened), (), |_| {})?;
    let at_exit = Arc::clone(&opened);
    let closer = env.create_function_from_closure::<(), (), _>("closeElmdbDatabases", move |_| {
        close_all(&at_exit);
        Ok(())
    })?;
    let process: Object = env.get_global()?.get_named_property("process")?;
    let once: Once = process.get_named_property("once")?;
    once.apply(process, FnArgs::from(("exit", closer)))?;
    let at_teardown = Arc::clone(&opened);
    env.add_env_cleanup_hook((), move |()| close_all(&at_teardown))?;
    Ok(opened)
}

/// Keeps `db` to be closed as the environment ends, if it is still open then
pub fn close_at_exit(env: &Env, db: &Arc<elmdb::Database>) -> Result<()> {
    let opened = opened(env)?;
    let mut list = opened.lock().expect("the opened list's lock is never poisoned");
    list.retain(|held| held.strong_count() > 0);
    list.push(Arc::downgrade(db));
    Ok(())
}
