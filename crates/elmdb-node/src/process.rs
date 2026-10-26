//! One elmdb binary per process. Every binary independently opens the same process claim file and takes a
//! nonblocking exclusive lock before opening LMDB. The winning binary keeps its handle for the process lifetime,
//! across database closes and worker teardown; another copy cannot acquire it. The file is never unlinked: replacing
//! its inode while a lock is held would let a second binary lock the replacement. A stale file holds no claim once
//! the owning process ends, so PID reuse needs no cleanup.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use napi::Result;

use crate::Code;

fn claim_file() -> PathBuf {
    // A worker may have a different TMPDIR. Unix's shared temporary directory keeps every copy in this process in
    // the same namespace. Windows uses the process's native temporary directory, not a worker's JS environment.
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions; the real user id keeps private stale files of other users out of
        // this namespace when the OS reuses a PID.
        let user = unsafe { libc::getuid() };
        PathBuf::from("/tmp").join(format!("elmdb-binding-{user}-{}.lock", std::process::id()))
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir().join(format!("elmdb-binding-{}.lock", std::process::id()))
    }
}

fn acquire(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "the process is already claimed"))
        }
        Err(TryLockError::Error(error)) => Err(error),
    }
}

/// Claims the process before this binary opens any database. Each independently loaded binary owns its own `OnceLock`
/// and independently opened file handle, so only one wins even when first opens race across worker threads.
pub fn claim() -> Result<(), Code> {
    static CLAIMED: OnceLock<io::Result<File>> = OnceLock::new();
    match CLAIMED.get_or_init(|| acquire(&claim_file())) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            let mine = process_path::get_dylib_path()
                .map_or_else(|| "an unknown file".to_owned(), |p| p.display().to_string());
            Err(napi::Error::new(
                Code("ALREADY_LOADED"),
                format!(
                    "Another elmdb binary already claimed this process, so {mine} can't open a database. Load one elmdb package"
                ),
            ))
        }
        Err(error) => Err(napi::Error::new(
            Code("IO"),
            format!(
                "Cannot claim this process at {}: {error}. Ensure its temporary directory permits file creation and locking",
                claim_file().display()
            ),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_handles_conflict_and_an_unowned_file_can_be_reused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claim");
        let first = acquire(&path).unwrap();
        assert_eq!(acquire(&path).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        drop(first);
        acquire(&path).unwrap();
    }

    #[test]
    fn an_io_failure_does_not_grant_a_claim() {
        let directory = tempfile::tempdir().unwrap();
        assert!(acquire(directory.path()).is_err());
    }
}
