//! Whose write transaction it is, kept so that one whose holder died is let go.
//!
//! On macOS elmdb builds LMDB with POSIX semaphores (`Cargo.toml` says why), and LMDB's write lock is then a semaphore
//! that nothing releases when the process holding it dies: only an environment's first opener resets it, so while any
//! other process holds the database open, a writer killed inside a transaction leaves every later writer waiting for
//! good. Linux and Windows keep LMDB's robust mutexes, which the system releases, and need none of this.
//!
//! So on macOS a writer first takes an exclusive lock on `writer.lock` in the database's directory, a lock the kernel
//! releases however its holder ends, and keeps it until its transaction has ended. Whoever holds it knows no live elmdb
//! writer is inside a transaction, so LMDB's semaphore taken then was taken by one that died, and is released before the
//! transaction begins. A writer waiting for another waits on the file lock rather than inside LMDB, so one already
//! waiting when the holder dies is let go by the kernel too.

use std::path::Path;

use crate::error::Result;

/// A writer's hold on the database's write transaction, kept until it has ended
pub(crate) struct WriterLock {
    #[cfg(target_os = "macos")]
    _held: Option<macos::Hold>,
}

impl WriterLock {
    /// Lets the next writer in, as dropping it would; spelled out where that has to happen before the scope ends
    pub(crate) fn release(self) {}
}

/// What a writer takes before LMDB's write lock
pub(crate) struct Writers {
    #[cfg(target_os = "macos")]
    held: Option<macos::Held>,
}

impl Writers {
    /// For the environment in `dir`, opened already. A read-only handle never writes and takes nothing
    pub(crate) fn open(dir: &Path, read_only: bool) -> Result<Self> {
        #[cfg(target_os = "macos")]
        return Ok(Self { held: if read_only { None } else { Some(macos::Held::open(dir)?) } });
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (dir, read_only);
            Ok(Self {})
        }
    }

    /// Waits until no other writer is inside a write transaction, freeing LMDB's write lock if one that died left it
    /// held, and holds that until the returned lock is dropped, after the transaction has ended
    pub(crate) fn hold(&self) -> Result<WriterLock> {
        #[cfg(target_os = "macos")]
        return Ok(WriterLock { _held: self.held.as_ref().map(macos::Held::hold).transpose()? });
        #[cfg(not(target_os = "macos"))]
        Ok(WriterLock {})
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::path::Path;
    use std::sync::Arc;

    use crate::error::{Error, ErrorCode, Result};

    /// LMDB's mutex id for the lock file, from which its semaphores' names are made: the FNV-1a hash of the lock file's
    /// device and inode as the C struct `{ dev_t dev; ino_t ino; }` holds them, zeroed padding included (`mdb.c`,
    /// `mdb_env_setup_locks` and `mdb_hash`). Computed from a `stat` rather than read from the lock file: opening and
    /// closing it would drop the record lock LMDB holds on it, and the next opener would take itself for the first and
    /// make new semaphores, leaving two writers each holding a lock of its own
    pub(super) fn mutex_id(device: i32, inode: u64) -> u64 {
        let mut layout = [0u8; 16];
        layout[..4].copy_from_slice(&device.to_ne_bytes());
        layout[8..].copy_from_slice(&inode.to_ne_bytes());
        layout
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3))
    }
    /// LMDB's own alphabet for the name (`mdb_a85`)
    const A85: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";

    /// LMDB's write semaphore's name: `/MDBw` then the mutex id, base 85, least significant digit first (`mdb_pack85`)
    pub(super) fn semaphore_name(mutex_id: u64) -> String {
        let mut name = String::from("/MDBw");
        let mut rest = mutex_id;
        for _ in 0..10 {
            if rest == 0 {
                break;
            }
            name.push(A85[(rest % 85) as usize] as char);
            rest /= 85;
        }
        name
    }

    pub(super) struct Held {
        /// `writer.lock`, opened once. A lock is the open file's, so this process's threads take turns on `turn` first
        file: Arc<std::fs::File>,
        turn: Arc<parking_lot::Mutex<()>>,
        semaphore: *mut libc::sem_t,
    }

    /// One writer's turn, with the file locked for it, both given back when dropped
    pub(super) struct Hold {
        file: Arc<std::fs::File>,
        _turn: parking_lot::ArcMutexGuard<parking_lot::RawMutex, ()>,
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            // Before `_turn` goes, so the next thread here finds the file unlocked
            let _ = self.file.unlock();
        }
    }

    // SAFETY: a named semaphore's handle is the system's, and its calls are safe from any thread
    unsafe impl Send for Held {}
    // SAFETY: as for Send: sem_trywait and sem_post may run on the same handle from several threads at once
    unsafe impl Sync for Held {}

    impl Held {
        pub(super) fn open(dir: &Path) -> Result<Self> {
            use std::os::unix::fs::MetadataExt;
            let lock = std::fs::metadata(dir.join("lock.mdb"))?;
            // `dev_t` is 32 bits on macOS, which `MetadataExt` widens
            let id = mutex_id(lock.dev() as i32, lock.ino());
            let name = CString::new(semaphore_name(id)).expect("a semaphore's name holds no NUL");
            // SAFETY: the name is a valid C string, and opening an existing semaphore creates nothing
            let semaphore = unsafe { libc::sem_open(name.as_ptr(), 0) };
            if semaphore == libc::SEM_FAILED {
                return Err(Error::new(
                    ErrorCode::Io,
                    format!("LMDB's write lock {name:?} wouldn't open: {}", std::io::Error::last_os_error()),
                ));
            }
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(dir.join("writer.lock"))?;
            Ok(Self { file: Arc::new(file), turn: Arc::default(), semaphore })
        }

        pub(super) fn hold(&self) -> Result<Hold> {
            let turn = self.turn.lock_arc();
            self.file.lock()?;
            let hold = Hold { file: Arc::clone(&self.file), _turn: turn };
            loop {
                // SAFETY: the handle was opened in `open` and stays open until drop
                if unsafe { libc::sem_trywait(self.semaphore) } == 0 {
                    // Free, as it should be with no live writer inside a transaction: given back for LMDB to take
                    // SAFETY: as above
                    unsafe { libc::sem_post(self.semaphore) };
                    return Ok(hold);
                }
                match std::io::Error::last_os_error().raw_os_error() {
                    Some(libc::EINTR) => continue,
                    Some(libc::EAGAIN) => {
                        // Taken, though no live elmdb writer can be inside a transaction: one died there
                        // SAFETY: as above
                        unsafe { libc::sem_post(self.semaphore) };
                        return Ok(hold);
                    }
                    _ => return Err(std::io::Error::last_os_error().into()),
                }
            }
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            // SAFETY: opened in `open`, closed once, here
            unsafe { libc::sem_close(self.semaphore) };
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn makes_the_id_lmdb_does_from_a_device_and_inode() {
        // FNV-1a of sixteen zero bytes, as `mdb_hash` makes it
        assert_eq!(super::macos::mutex_id(0, 0), 0x8820_1fb9_60ff_6465);
        assert_ne!(super::macos::mutex_id(1, 0), super::macos::mutex_id(0, 1));
    }

    #[test]
    fn names_the_write_lock_as_lmdb_does() {
        assert_eq!(super::macos::semaphore_name(0), "/MDBw");
        assert_eq!(super::macos::semaphore_name(1), "/MDBw1");
        assert_eq!(super::macos::semaphore_name(85), "/MDBw01");
        assert_eq!(super::macos::semaphore_name(86), "/MDBw11");
    }
}
