//! Zero-cost scoped failpoints for testing real OS, LMDB, durability, and lifecycle fault seams.
//!
//! All failpoint code and symbols are compiled **only** when `feature = "failpoints"` is enabled;
//! in default, release, and npm builds, `failure_point!` and `run_point!` expand to `{}` (verified
//! by `cargo xtask check-failpoints-off`).
//!
//! Failpoints are strictly scoped so parallel tests never interfere:
//! - Thread-scoped (`fail_once_here`, `run_once_here`): fires once on the calling thread only.
//! - Path-scoped (`fail_once_at`, `fail_while_at`, `run_once_at`): fires on any thread operating
//!   on the armed database directory path.

/// Named failpoint sites shared between production injection seams and tests.
pub mod points {
    /// Before `mdb_txn_commit` in `LmdbRwTxn::commit_raw`.
    pub const LMDB_TXN_COMMIT: &str = "lmdb::txn_commit";
    /// Before `mdb_env_sync` in `LmdbStore::sync` / `sync_under_write`.
    pub const LMDB_ENV_SYNC: &str = "lmdb::env_sync";
    /// Before `mdb_env_set_mapsize` in `LmdbStore::grow_map_locked`.
    pub const LMDB_ENV_SET_MAPSIZE: &str = "lmdb::env_set_mapsize";
    /// Before writing the durability sidecar (`synced.v2` or temp file).
    pub const SIDECAR_WRITE: &str = "durability::sidecar_write";
    /// Before fsyncing the durability sidecar (`synced.v2` or temp file).
    pub const SIDECAR_FSYNC: &str = "durability::sidecar_fsync";
    /// Before renaming the initial temp durability sidecar into `synced.v2`.
    pub const SIDECAR_RENAME: &str = "durability::sidecar_rename";
    /// Negative-control point: skips installing the Grouped `MDB_RDONLY` durable pin while still
    /// running `DurabilityMode::Grouped` + `store.sync()` + `synced.v2` sidecar sealing.
    pub const SKIP_GROUPED_DURABLE_PIN: &str = "durability::skip_grouped_durable_pin";
    /// Before atomic file/directory publication rename in `fs_publish`.
    pub const FS_PUBLISH_RENAME: &str = "elmdb::fs_publish_rename";
    /// Inside `DbLifecycle::close` right after taking the handle and before waiting on `drain_cv`.
    pub const CLOSE_INNER_BEFORE_DRAIN: &str = "elmdb::close_inner_before_drain";
}

/// Evaluates to `return Err($error)` when failpoint `$name` is armed on the current thread (or for `$path`).
/// Compiles to `{}` when `feature = "failpoints"` is disabled.
#[macro_export]
#[cfg(feature = "failpoints")]
macro_rules! failure_point {
    ($name:ident, $error:expr) => {
        if $crate::failpoints::fails_here($crate::failpoints::points::$name) {
            return Err($error);
        }
    };
    ($name:ident at $path:expr, $error:expr) => {
        if $crate::failpoints::fails_at($crate::failpoints::points::$name, $path) {
            return Err($error);
        }
    };
}

/// Zero-cost no-op expansion when `feature = "failpoints"` is disabled.
#[macro_export]
#[cfg(not(feature = "failpoints"))]
macro_rules! failure_point {
    ($name:ident, $error:expr) => {};
    ($name:ident at $path:expr, $error:expr) => {};
}

/// Invokes an armed callback (if any) on the current thread or for `$path`, then continues execution.
/// Compiles to `{}` when `feature = "failpoints"` is disabled.
#[macro_export]
#[cfg(feature = "failpoints")]
macro_rules! run_point {
    ($name:ident) => {
        $crate::failpoints::runs_here($crate::failpoints::points::$name);
    };
    ($name:ident at $path:expr) => {
        $crate::failpoints::runs_at($crate::failpoints::points::$name, $path);
    };
}

/// Zero-cost no-op expansion when `feature = "failpoints"` is disabled.
#[macro_export]
#[cfg(not(feature = "failpoints"))]
macro_rules! run_point {
    ($name:ident) => {};
    ($name:ident at $path:expr) => {};
}

#[cfg(feature = "failpoints")]
mod imp {
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    use crate::locks::sync::Mutex;

    type LocalCallback = Box<dyn FnOnce() + 'static>;
    type GlobalCallback = Box<dyn FnOnce() + Send + 'static>;

    #[derive(Default)]
    struct LocalRegistry {
        fail_counts: HashMap<&'static str, usize>,
        callbacks: HashMap<&'static str, VecDeque<LocalCallback>>,
    }

    crate::locks::sync::thread_local! {
        static LOCAL: RefCell<LocalRegistry> = RefCell::new(LocalRegistry::default());
    }

    #[derive(Default)]
    struct PathEntry {
        fail_once: usize,
        fail_while: usize,
        callbacks: VecDeque<GlobalCallback>,
    }

    #[derive(Default)]
    struct PathRegistry {
        entries: HashMap<(&'static str, PathBuf), PathEntry>,
    }

    fn path_registry() -> &'static Mutex<PathRegistry> {
        static REGISTRY: OnceLock<Mutex<PathRegistry>> = OnceLock::new();
        REGISTRY.get_or_init(|| Mutex::new(PathRegistry::default()))
    }

    fn normalize_path(path: &Path) -> PathBuf {
        if let Ok(canon) = std::fs::canonicalize(path) {
            return canon;
        }
        // Walk up to the longest existing ancestor and rejoin trailing components so arming a
        // failpoint before `Database::open` creates the directory still matches after creation
        // (e.g. macOS `/var` -> `/private/var`, Windows `\\?\`, or symlinked temp dirs).
        let mut tail = Vec::new();
        let mut cur = path;
        while let Some(parent) = cur.parent() {
            if let Some(name) = cur.file_name() {
                tail.push(name.to_os_string());
            }
            if !parent.as_os_str().is_empty()
                && let Ok(mut base) = std::fs::canonicalize(parent)
            {
                for part in tail.into_iter().rev() {
                    base.push(part);
                }
                return base;
            }
            cur = parent;
        }
        path.to_path_buf()
    }

    /// RAII guard keeping a path-scoped persistent failpoint armed until dropped.
    pub struct FailWhileGuard {
        name: &'static str,
        path: PathBuf,
    }

    impl Drop for FailWhileGuard {
        fn drop(&mut self) {
            let mut reg = path_registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = reg.entries.get_mut(&(self.name, self.path.clone())) {
                entry.fail_while = entry.fail_while.saturating_sub(1);
                if entry.fail_once == 0 && entry.fail_while == 0 && entry.callbacks.is_empty() {
                    reg.entries.remove(&(self.name, self.path.clone()));
                }
            }
        }
    }

    /// Arms `$name` to fail once on the calling thread.
    pub fn fail_once_here(name: &'static str) {
        LOCAL.with(|cell| {
            *cell.borrow_mut().fail_counts.entry(name).or_insert(0) += 1;
        });
    }

    /// Arms `$name` to invoke `callback` once on the calling thread.
    pub fn run_once_here(name: &'static str, callback: impl FnOnce() + 'static) {
        LOCAL.with(|cell| {
            cell.borrow_mut().callbacks.entry(name).or_default().push_back(Box::new(callback));
        });
    }

    /// Arms `$name` to fail once on any thread operating on `path`.
    pub fn fail_once_at(name: &'static str, path: &Path) {
        let key = (name, normalize_path(path));
        let mut reg = path_registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        reg.entries.entry(key).or_default().fail_once += 1;
    }

    /// Arms `$name` to fail repeatedly on any thread operating on `path` until the returned guard drops.
    pub fn fail_while_at(name: &'static str, path: &Path) -> FailWhileGuard {
        let norm = normalize_path(path);
        let mut reg = path_registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        reg.entries.entry((name, norm.clone())).or_default().fail_while += 1;
        FailWhileGuard { name, path: norm }
    }

    /// Arms `$name` to invoke `callback` once on any thread operating on `path`.
    pub fn run_once_at(name: &'static str, path: &Path, callback: impl FnOnce() + Send + 'static) {
        let key = (name, normalize_path(path));
        let mut reg = path_registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        reg.entries.entry(key).or_default().callbacks.push_back(Box::new(callback));
    }

    /// Disarms any one-shot failure or callback armed for `(name, path)`.
    pub fn disarm_at(name: &'static str, path: &Path) {
        let key = (name, normalize_path(path));
        let mut reg = path_registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = reg.entries.get_mut(&key) {
            entry.fail_once = 0;
            entry.callbacks.clear();
            if entry.fail_while == 0 {
                reg.entries.remove(&key);
            }
        }
    }

    /// Disarms any thread-local failure or callback armed on the calling thread for `name`.
    pub fn disarm_here(name: &'static str) {
        LOCAL.with(|cell| {
            let mut local = cell.borrow_mut();
            local.fail_counts.remove(name);
            local.callbacks.remove(name);
        });
    }

    /// Checks and consumes a thread-local failure arming for `name`.
    pub fn fails_here(name: &'static str) -> bool {
        LOCAL.with(|cell| {
            let mut local = cell.borrow_mut();
            if let Some(count) = local.fail_counts.get_mut(name) {
                *count -= 1;
                if *count == 0 {
                    local.fail_counts.remove(name);
                }
                true
            } else {
                false
            }
        })
    }

    /// Checks and consumes a path-scoped (or thread-local) failure arming for `(name, path)`.
    pub fn fails_at(name: &'static str, path: &Path) -> bool {
        if fails_here(name) {
            return true;
        }
        let key = (name, normalize_path(path));
        let mut reg = path_registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(entry) = reg.entries.get_mut(&key) else {
            return false;
        };
        if entry.fail_while > 0 {
            return true;
        }
        if entry.fail_once > 0 {
            entry.fail_once -= 1;
            if entry.fail_once == 0 && entry.callbacks.is_empty() {
                reg.entries.remove(&key);
            }
            return true;
        }
        false
    }

    /// Pops and runs one thread-local callback for `name`, if armed.
    pub fn runs_here(name: &'static str) -> bool {
        let cb = LOCAL.with(|cell| {
            let mut local = cell.borrow_mut();
            let cb = local.callbacks.get_mut(name).and_then(VecDeque::pop_front);
            if local.callbacks.get(name).is_some_and(VecDeque::is_empty) {
                local.callbacks.remove(name);
            }
            cb
        });
        if let Some(f) = cb {
            f();
            true
        } else {
            false
        }
    }

    /// Pops and runs one path-scoped (or thread-local) callback for `(name, path)`, if armed.
    pub fn runs_at(name: &'static str, path: &Path) -> bool {
        if runs_here(name) {
            return true;
        }
        let key = (name, normalize_path(path));
        let cb = {
            let mut reg = path_registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let cb = reg.entries.get_mut(&key).and_then(|e| e.callbacks.pop_front());
            if reg.entries.get(&key).is_some_and(|e| e.fail_once == 0 && e.fail_while == 0 && e.callbacks.is_empty()) {
                reg.entries.remove(&key);
            }
            cb
        };
        if let Some(f) = cb {
            f();
            true
        } else {
            false
        }
    }

    #[cfg(test)]
    pub(super) fn has_path_entry(name: &'static str, path: &Path) -> bool {
        let key = (name, normalize_path(path));
        path_registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner).entries.contains_key(&key)
    }
}

#[cfg(feature = "failpoints")]
pub use imp::{
    FailWhileGuard, disarm_at, disarm_here, fail_once_at, fail_once_here, fail_while_at, fails_at, fails_here,
    run_once_at, run_once_here, runs_at, runs_here,
};

#[cfg(all(test, feature = "failpoints"))]
mod tests {
    use std::cell::Cell;
    use std::path::Path;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn thread_scoped_failpoint_fires_only_on_arming_thread_and_once() {
        fail_once_here(points::LMDB_TXN_COMMIT);
        fail_once_here(points::LMDB_TXN_COMMIT);
        assert!(!std::thread::spawn(|| fails_here(points::LMDB_TXN_COMMIT)).join().unwrap());
        assert!(fails_here(points::LMDB_TXN_COMMIT));
        assert!(fails_at(points::LMDB_TXN_COMMIT, Path::new("/unarmed-path")));
        assert!(!fails_here(points::LMDB_TXN_COMMIT));

        fail_once_here(points::LMDB_TXN_COMMIT);
        disarm_here(points::LMDB_TXN_COMMIT);
        assert!(!fails_here(points::LMDB_TXN_COMMIT));
    }

    #[test]
    fn path_scoped_failpoint_fires_across_threads_for_matching_path_only() {
        let p1 = Path::new("/elmdb-fp-one");
        let p2 = Path::new("/elmdb-fp-two");

        fail_once_at(points::SIDECAR_WRITE, p1);
        fail_once_at(points::SIDECAR_WRITE, p1);
        assert!(!fails_at(points::SIDECAR_WRITE, p2));
        assert!(std::thread::spawn(|| fails_at(points::SIDECAR_WRITE, Path::new("/elmdb-fp-one"))).join().unwrap());
        assert!(fails_at(points::SIDECAR_WRITE, p1));
        assert!(!fails_at(points::SIDECAR_WRITE, p1));
        assert!(!imp::has_path_entry(points::SIDECAR_WRITE, p1), "exhausted fail_once entry must be pruned");

        fail_once_at(points::SIDECAR_FSYNC, p1);
        fail_once_at(points::SIDECAR_FSYNC, p2);
        disarm_at(points::SIDECAR_FSYNC, p1);
        assert!(!fails_at(points::SIDECAR_FSYNC, p1));
        assert!(!imp::has_path_entry(points::SIDECAR_FSYNC, p1));
        assert!(fails_at(points::SIDECAR_FSYNC, p2));
        assert!(!imp::has_path_entry(points::SIDECAR_FSYNC, p2));

        let g1 = fail_while_at(points::LMDB_ENV_SYNC, p1);
        let g2 = fail_while_at(points::LMDB_ENV_SYNC, p1);
        assert!(fails_at(points::LMDB_ENV_SYNC, p1));
        assert!(!fails_at(points::LMDB_ENV_SYNC, p2));
        drop(g1);
        assert!(fails_at(points::LMDB_ENV_SYNC, p1), "second guard must keep fail_while active");
        disarm_at(points::LMDB_ENV_SYNC, p1);
        assert!(fails_at(points::LMDB_ENV_SYNC, p1), "disarm_at must not cancel live FailWhileGuard");
        drop(g2);
        assert!(!fails_at(points::LMDB_ENV_SYNC, p1));
        assert!(!imp::has_path_entry(points::LMDB_ENV_SYNC, p1), "dropped FailWhileGuard must prune empty entry");

        // Arming a non-existent child path before directory creation must still match after creation.
        let base = std::env::temp_dir().join(format!("elmdb-fp-norm-{}", std::process::id()));
        let child = base.join("pending-db-dir");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        fail_once_at(points::LMDB_ENV_SET_MAPSIZE, &child);
        std::fs::create_dir_all(&child).unwrap();
        assert!(fails_at(points::LMDB_ENV_SET_MAPSIZE, &child));
        assert!(!imp::has_path_entry(points::LMDB_ENV_SET_MAPSIZE, &child));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn run_points_fire_once_on_thread_or_path_and_coexist_with_fail_entries() {
        let local_count = Rc::new(Cell::new(0usize));
        let c1 = Rc::clone(&local_count);
        let c2 = Rc::clone(&local_count);
        run_once_here(points::CLOSE_INNER_BEFORE_DRAIN, move || c1.set(c1.get() + 1));
        run_once_here(points::CLOSE_INNER_BEFORE_DRAIN, move || c2.set(c2.get() + 1));
        assert!(!std::thread::spawn(|| runs_here(points::CLOSE_INNER_BEFORE_DRAIN)).join().unwrap());
        assert_eq!(local_count.get(), 0);
        assert!(runs_here(points::CLOSE_INNER_BEFORE_DRAIN));
        assert!(runs_at(points::CLOSE_INNER_BEFORE_DRAIN, Path::new("/unarmed-path")));
        assert!(!runs_here(points::CLOSE_INNER_BEFORE_DRAIN));
        assert_eq!(local_count.get(), 2);

        let path = Path::new("/elmdb-run-mixed");
        let path_count = Arc::new(AtomicUsize::new(0));
        let pc1 = Arc::clone(&path_count);
        let pc2 = Arc::clone(&path_count);
        run_once_at(points::FS_PUBLISH_RENAME, path, move || {
            pc1.fetch_add(1, Ordering::SeqCst);
        });
        // `fails_at` on a callback-only entry (`fail_once == 0 && fail_while == 0`) must return false!
        assert!(!fails_at(points::FS_PUBLISH_RENAME, path));

        // Mix a second callback + `fail_once_at` + `fail_while_at` on the same `(name, path)`.
        run_once_at(points::FS_PUBLISH_RENAME, path, move || {
            pc2.fetch_add(1, Ordering::SeqCst);
        });
        fail_once_at(points::FS_PUBLISH_RENAME, path);
        let guard = fail_while_at(points::FS_PUBLISH_RENAME, path);

        assert!(!runs_at(points::FS_PUBLISH_RENAME, Path::new("/elmdb-run-other")));
        assert!(
            std::thread::spawn(|| runs_at(points::FS_PUBLISH_RENAME, Path::new("/elmdb-run-mixed"))).join().unwrap()
        );
        assert!(imp::has_path_entry(points::FS_PUBLISH_RENAME, path), "second callback & fails still live");
        assert!(runs_at(points::FS_PUBLISH_RENAME, path));
        assert!(!runs_at(points::FS_PUBLISH_RENAME, path));
        assert_eq!(path_count.load(Ordering::SeqCst), 2);
        assert!(
            imp::has_path_entry(points::FS_PUBLISH_RENAME, path),
            "fail_while & fail_once must survive callback drain"
        );

        // Dropping `guard` while `fail_once > 0` must keep the entry until `fail_once` is consumed.
        drop(guard);
        assert!(imp::has_path_entry(points::FS_PUBLISH_RENAME, path));
        assert!(fails_at(points::FS_PUBLISH_RENAME, path));
        assert!(!fails_at(points::FS_PUBLISH_RENAME, path));
        assert!(!imp::has_path_entry(points::FS_PUBLISH_RENAME, path));

        // Dropping `guard` while a callback is still armed must keep the entry until the callback runs.
        let g3 = fail_while_at(points::FS_PUBLISH_RENAME, path);
        let pc3 = Arc::clone(&path_count);
        run_once_at(points::FS_PUBLISH_RENAME, path, move || {
            pc3.fetch_add(1, Ordering::SeqCst);
        });
        drop(g3);
        assert!(imp::has_path_entry(points::FS_PUBLISH_RENAME, path));
        assert!(runs_at(points::FS_PUBLISH_RENAME, path));
        assert_eq!(path_count.load(Ordering::SeqCst), 3);
        assert!(!imp::has_path_entry(points::FS_PUBLISH_RENAME, path));
    }
}
