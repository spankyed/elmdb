//! The faults a test injects, as `fail`'s named failpoints (`docs/architecture/testing.md`, Failpoints).
//!
//! `fail`'s registry is the process's, and the suite's tests run in parallel on threads of one process, so a failpoint
//! fires only where a test armed it: once on the thread that armed it, or once for the database at a path. A site is
//! one of this module's macros, naming its point in `points`, which asks whether it is armed in `fail_point!`'s
//! condition:
//!
//! ```ignore
//! failure_point!(COMMIT, Error::new(ErrorCode::Io, "No space left on device"));        // armed on this thread
//! failure_point!(COMMIT at &self.path, Error::new(ErrorCode::Io, "No space left on device"));
//! run_point!(SCHEMA_WRITE);                                                            // runs what a test armed
//! ```
//!
//! Without `fail`'s `failpoints` feature, which only the crate's tests enable, `fail_point!` expands to nothing and its
//! condition is never evaluated: these functions are then unused, and no build but a test's reaches them. A test arms
//! a point through `fail_once_here`, `fail_once_at` or `run_once_here`, never `fail::cfg` itself, which would arm it for
//! every test running at the time.

/// A point a test may arm to make the enclosing function answer `Err($error)`: on this thread, or with `at`, for the
/// database at a path
macro_rules! failure_point {
    ($name:ident, $error:expr) => {
        ::fail::fail_point!(
            $crate::failpoints::points::$name,
            $crate::failpoints::fails_here($crate::failpoints::points::$name),
            |_| Err($error)
        )
    };
    ($name:ident at $path:expr, $error:expr) => {
        ::fail::fail_point!(
            $crate::failpoints::points::$name,
            $crate::failpoints::fails_at($crate::failpoints::points::$name, $path),
            |_| Err($error)
        )
    };
}

/// A point a test may arm to run code here, on its own thread, before the enclosing function goes on
macro_rules! run_point {
    ($name:ident) => {
        ::fail::fail_point!(
            $crate::failpoints::points::$name,
            $crate::failpoints::runs_here($crate::failpoints::points::$name),
            |_| unreachable!("a run point always goes on")
        )
    };
}

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The names of the points, which a site and the tests arming it share
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod points {
    /// A store's commit fails, as on a full disk
    pub const COMMIT: &str = "store::commit";
    /// A store's sync fails, while it is armed
    pub const SYNC: &str = "store::sync";
    /// Stamping a new environment fails, as on a full disk
    pub const STAMP: &str = "store::stamp";
    /// Writing the reset checkpoint fails
    pub const RESET_CHECKPOINT: &str = "store::reset_checkpoint";
    /// Runs before a write transaction is reserved
    pub const WRITE_RESERVATION: &str = "store::before_write_reservation";
    /// Runs once a backup's compacted copy is complete, before it is published
    pub const BACKUP_READY: &str = "backup::ready";
    /// A backup fails at publication, its copy complete and not yet visible
    pub const BACKUP_PUBLISH: &str = "backup::publish";
    /// A backup's publication fails after the copy is visible, before its directories are synced
    pub const BACKUP_PUBLISHED: &str = "backup::published";
    /// Runs before a schema that failed to apply is recovered
    pub const SCHEMA_RECOVERY: &str = "database::before_schema_recovery";
    /// Runs before a schema is written
    pub const SCHEMA_WRITE: &str = "database::before_schema_write";
    /// Runs before a load publishes its staged database
    pub const LOAD_PUBLISH: &str = "dump::before_load_publish";
    /// A load's sync fails after its database is published
    pub const LOAD_PUBLISHED: &str = "dump::after_load_publish";
    /// Runs before a read takes its snapshot
    pub const READ_SNAPSHOT: &str = "engine::before_read_snapshot";
    /// Reading the indexes still to build fails
    pub const INDEX_REFRESH: &str = "engine::index_refresh";
    /// A watch's read panics
    pub const WATCH_READ: &str = "watch::read";
    /// Creating a derivation's scratch directory fails
    pub const SCRATCH: &str = "derive::scratch";
}

/// What a point is armed with on one thread
#[cfg_attr(not(test), allow(dead_code))]
enum Armed {
    Fail,
    Run(Box<dyn FnOnce()>),
}

thread_local! {
    static HERE: RefCell<HashMap<&'static str, Armed>> = RefCell::new(HashMap::new());
}

/// The points armed for a database at a path, on whatever thread reaches them, each once or until disarmed
static AT: Mutex<Vec<At>> = Mutex::new(Vec::new());

#[cfg_attr(not(test), allow(dead_code))]
struct At {
    name: &'static str,
    path: PathBuf,
    once: bool,
}

/// Whether `name` is armed to fail on this thread, disarming it. A thread whose locals are being destroyed has nothing
/// armed: a watch can be delivered from another thread-local's destructor
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn fails_here(name: &str) -> bool {
    HERE.try_with(|armed| {
        let mut armed = armed.borrow_mut();
        match armed.get(name) {
            Some(Armed::Fail) => armed.remove(name).is_some(),
            _ => false,
        }
    })
    .unwrap_or(false)
}

/// Whether `name` is armed to fail for the database at `path`, disarming it if it was armed once
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn fails_at(name: &str, path: &Path) -> bool {
    let mut armed = AT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match armed.iter().position(|at| at.name == name && at.path == path) {
        Some(at) => {
            if armed[at].once {
                armed.remove(at);
            }
            true
        }
        None => false,
    }
}

/// Runs what `name` is armed with on this thread, disarming it. Answers false, so a site's `fail_point!` goes on as if
/// nothing happened
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn runs_here(name: &str) -> bool {
    let run = HERE
        .try_with(|armed| {
            let mut armed = armed.borrow_mut();
            match armed.get(name) {
                Some(Armed::Run(_)) => match armed.remove(name) {
                    Some(Armed::Run(run)) => Some(run),
                    _ => None,
                },
                _ => None,
            }
        })
        .ok()
        .flatten();
    if let Some(run) = run {
        run();
    }
    false
}

/// `fail`'s own registry answers a `fail_point!` whose condition holds only once the point has an action
#[cfg(test)]
fn registered(name: &'static str) {
    fail::cfg(name, "return").expect("a failpoint's action is valid");
}

/// Arms `name` to fail once, on this thread only
#[cfg(test)]
pub(crate) fn fail_once_here(name: &'static str) {
    registered(name);
    HERE.with_borrow_mut(|armed| armed.insert(name, Armed::Fail));
}

/// Arms `name` to fail once for the database at `path`, on whichever thread reaches it; arming it again before it fires
/// still fails it once
#[cfg(test)]
pub(crate) fn fail_once_at(name: &'static str, path: &Path) {
    registered(name);
    let mut armed = AT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if !armed.iter().any(|at| at.name == name && at.path == path) {
        armed.push(At { name, path: path.to_owned(), once: true });
    }
}

/// Arms `name` to fail for the database at `path` every time it is reached, until the answer drops
#[cfg(test)]
#[must_use = "the point is disarmed when this drops"]
pub(crate) fn fail_while_at(name: &'static str, path: &Path) -> DisarmAt {
    registered(name);
    AT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(At { name, path: path.to_owned(), once: false });
    DisarmAt { name, path: path.to_owned() }
}

/// Disarms what `fail_while_at` armed, as it drops
#[cfg(test)]
pub(crate) struct DisarmAt {
    name: &'static str,
    path: PathBuf,
}

#[cfg(test)]
impl Drop for DisarmAt {
    fn drop(&mut self) {
        AT.lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|at| !(at.name == self.name && at.path == self.path && !at.once));
    }
}

/// Arms `name` to run `run` once, on this thread only
#[cfg(test)]
pub(crate) fn run_once_here(name: &'static str, run: impl FnOnce() + 'static) {
    registered(name);
    HERE.with_borrow_mut(|armed| armed.insert(name, Armed::Run(Box::new(run))));
}

/// Whether `name` is still armed on this thread: a test's check that the point it armed was reached
#[cfg(test)]
pub(crate) fn is_armed_here(name: &'static str) -> bool {
    HERE.with_borrow(|armed| armed.contains_key(name))
}

/// Disarms what `fail_once_at` armed `name` with for the database at `path`, leaving other paths' armings
#[cfg(test)]
pub(crate) fn disarm_at(name: &'static str, path: &Path) {
    AT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).retain(|at| !(at.name == name && at.path == path));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A site, as the engine's are written
    fn site(name: &'static str) -> Result<(), &'static str> {
        fail::fail_point!(name, fails_here(name), |_| Err("failed"));
        Ok(())
    }

    #[test]
    fn a_point_armed_on_one_thread_fails_there_once_and_nowhere_else() {
        fail_once_here(points::INDEX_REFRESH);
        let elsewhere = std::thread::spawn(|| site(points::INDEX_REFRESH)).join().unwrap();
        assert_eq!(elsewhere, Ok(()), "another thread doesn't hear it");
        assert_eq!(site(points::INDEX_REFRESH), Err("failed"));
        assert_eq!(site(points::INDEX_REFRESH), Ok(()), "once");
    }

    #[test]
    fn a_point_armed_for_a_path_fails_for_that_path_once_on_any_thread() {
        let (one, other) = (Path::new("/one"), Path::new("/other"));
        fail_once_at(points::STAMP, one);
        fail_once_at(points::STAMP, one);
        assert!(!fails_at(points::STAMP, other));
        assert!(std::thread::spawn(move || fails_at(points::STAMP, Path::new("/one"))).join().unwrap());
        assert!(!fails_at(points::STAMP, one), "once");
        fail_once_at(points::STAMP, one);
        fail_once_at(points::STAMP, other);
        disarm_at(points::STAMP, one);
        assert!(!fails_at(points::STAMP, one), "disarmed");
        assert!(fails_at(points::STAMP, other), "another path's arming is its own");
        let held = fail_while_at(points::SYNC, one);
        assert!(fails_at(points::SYNC, one) && fails_at(points::SYNC, one), "until disarmed");
        drop(held);
        assert!(!fails_at(points::SYNC, one));
    }

    #[test]
    fn a_run_runs_once_on_its_thread_and_the_site_goes_on() {
        let ran = std::rc::Rc::new(std::cell::Cell::new(0));
        let counted = ran.clone();
        run_once_here(points::SCHEMA_WRITE, move || counted.set(counted.get() + 1));
        assert!(!std::thread::spawn(|| runs_here(points::SCHEMA_WRITE)).join().unwrap());
        assert_eq!(ran.get(), 0);
        assert!(!runs_here(points::SCHEMA_WRITE));
        assert!(!runs_here(points::SCHEMA_WRITE));
        assert_eq!(ran.get(), 1);
    }
}
