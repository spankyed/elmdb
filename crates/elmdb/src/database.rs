//! A database: one directory holding one LMDB environment, opened at most once per process.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use heed::{Env, EnvOpenOptions, WithoutTls};

use crate::error::{Error, ErrorCode, Result};

/// How a database is opened
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Open existing files without writing: every write is refused
    pub read_only: bool,
}

/// An open database. Closing it, or dropping it, releases the environment.
pub struct Database {
    path: PathBuf,
    read_only: bool,
    env: Option<Env<WithoutTls>>,
}

/// The environments open in this process, by canonical path. LMDB's locks are per process, so an environment opened
/// twice in one process shares no locks with itself and can corrupt its own file.
fn open_paths() -> &'static Mutex<HashSet<PathBuf>> {
    static OPEN: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    OPEN.get_or_init(Default::default)
}

impl Database {
    /// Opens the database at `path`, creating the directory and its files unless `read_only`.
    pub fn open(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self> {
        let path = path.as_ref();
        if options.read_only {
            if !path.join("data.mdb").is_file() {
                return Err(Error::new(ErrorCode::NoDatabase, format!("No database at {}", path.display())));
            }
        } else {
            std::fs::create_dir_all(path)?;
        }
        let canonical = path.canonicalize()?;
        {
            let mut open = open_paths().lock().expect("the open-paths lock is never poisoned");
            if !open.insert(canonical.clone()) {
                return Err(Error::new(
                    ErrorCode::AlreadyOpen,
                    format!("The database at {} is already open in this process; use the open one", path.display()),
                ));
            }
        }
        let mut builder = EnvOpenOptions::new().read_txn_without_tls();
        builder.max_dbs(8);
        if options.read_only {
            // SAFETY: the flag only stops LMDB writing; it changes nothing the binding relies on
            unsafe { builder.flags(heed::EnvFlags::READ_ONLY) };
        }
        // SAFETY: `open_paths` guarantees this process holds no other handle to the environment, and the files are
        // LMDB's, written only through LMDB by this or another process
        let env = match unsafe { builder.open(&canonical) } {
            Ok(env) => env,
            Err(error) => {
                open_paths().lock().expect("the open-paths lock is never poisoned").remove(&canonical);
                return Err(error.into());
            }
        };
        Ok(Self { path: canonical, read_only: options.read_only, env: Some(env) })
    }

    /// The database's directory
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn is_open(&self) -> bool {
        self.env.is_some()
    }

    /// Releases the environment; closing a closed database does nothing
    pub fn close(&mut self) {
        if let Some(env) = self.env.take() {
            env.prepare_for_closing().wait();
            open_paths().lock().expect("the open-paths lock is never poisoned").remove(&self.path);
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_creates_and_closes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let mut db = Database::open(&path, OpenOptions::default()).unwrap();
        assert!(db.is_open());
        assert!(path.join("data.mdb").is_file());
        db.close();
        assert!(!db.is_open());
        db.close();
    }

    #[test]
    fn refuses_a_second_open_in_one_process_and_allows_one_after_close() {
        let dir = tempfile::tempdir().unwrap();
        let mut first = Database::open(dir.path(), OpenOptions::default()).unwrap();
        let second = Database::open(dir.path(), OpenOptions::default()).err().unwrap();
        assert_eq!(second.code, ErrorCode::AlreadyOpen);
        first.close();
        Database::open(dir.path(), OpenOptions::default()).unwrap();
    }

    #[test]
    fn a_read_only_open_needs_a_database() {
        let dir = tempfile::tempdir().unwrap();
        let error = Database::open(dir.path(), OpenOptions { read_only: true }).err().unwrap();
        assert_eq!(error.code, ErrorCode::NoDatabase);
        drop(Database::open(dir.path(), OpenOptions::default()).unwrap());
        let db = Database::open(dir.path(), OpenOptions { read_only: true }).unwrap();
        assert!(db.is_read_only());
    }
}
