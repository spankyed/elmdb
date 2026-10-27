//! One elmdb binary per process. Windows uses a process-named kernel event independent of environment settings.
//! On Unix, every binary independently opens the same process claim file in a fixed shared directory and takes a
//! nonblocking exclusive lock before opening LMDB. The winning binary keeps its handle for the process lifetime,
//! across database closes and worker teardown; another copy cannot acquire it. The file is never unlinked: replacing
//! its inode while a lock is held would let a second binary lock the replacement. A stale file holds no claim once
//! the owning process ends, so PID reuse needs no cleanup.

#[cfg(unix)]
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use napi::Result;

use crate::Code;

#[cfg(unix)]
fn claim_file() -> PathBuf {
    // Every copy uses the same namespace, independent of mutable temporary-directory settings.
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions; the real user id keeps private stale files of other users out of
        // this namespace when the OS reuses a PID.
        let user = unsafe { libc::getuid() };
        PathBuf::from("/tmp").join(format!("elmdb-binding-{user}-{}.lock", std::process::id()))
    }
}

#[cfg(unix)]
fn acquire(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no preconditions. Only our effective user can own the claim inode; otherwise its owner
    // could unlink it in the shared sticky directory while we hold the lock and allow a second claim.
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the process claim must be a regular file owned by this user",
        ));
    }
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "the process is already claimed"))
        }
        Err(TryLockError::Error(error)) => Err(error),
    }
}

#[cfg(unix)]
type Claim = File;
#[cfg(windows)]
type Claim = std::os::windows::io::OwnedHandle;

#[cfg(windows)]
fn acquire_event(name: &str) -> io::Result<Claim> {
    use std::ffi::c_void;
    use std::os::windows::io::FromRawHandle;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateEventW(attributes: *const c_void, manual: i32, initial: i32, name: *const u16) -> *mut c_void;
        fn GetLastError() -> u32;
    }
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: name is a terminated wide string alive for the call. Null attributes create a non-inheritable
    // handle; the event is never waited on or signalled, so only its atomic named-object creation matters.
    let raw = unsafe { CreateEventW(std::ptr::null(), 1, 0, name.as_ptr()) };
    // SAFETY: GetLastError has no preconditions and must immediately follow CreateEventW.
    let error = unsafe { GetLastError() };
    if raw.is_null() {
        return Err(io::Error::from_raw_os_error(error as i32));
    }
    // SAFETY: CreateEventW returned an owned valid handle, transferred exactly once to OwnedHandle.
    let handle = unsafe { Claim::from_raw_handle(raw) };
    if error == 183 {
        return Err(io::Error::new(io::ErrorKind::WouldBlock, "the process is already claimed"));
    }
    Ok(handle)
}

fn acquire_claim() -> io::Result<Claim> {
    #[cfg(unix)]
    {
        acquire(&claim_file())
    }
    #[cfg(windows)]
    {
        acquire_event(&format!("Local\\elmdb-binding-{}", std::process::id()))
    }
}

/// Keep the accepted Windows image and its OnceLock mapped after its first worker unloads it. A leaked claim
/// handle alone survives DLL unloading, but a reloaded image would lose the static and reject its own old claim.
#[cfg(windows)]
fn pin_module() -> io::Result<()> {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleExW(flags: u32, address: *const u16, module: *mut *mut c_void) -> i32;
    }
    const FROM_ADDRESS: u32 = 0x0000_0004;
    const PIN: u32 = 0x0000_0001;
    let mut module = std::ptr::null_mut();
    // SAFETY: FROM_ADDRESS interprets the second argument as an address, not a wide string. This function's
    // address belongs to the currently executing binding image; module points to valid output storage. PIN retains
    // that image until process termination, so no FreeLibrary call is required or appropriate for this handle.
    let success = unsafe { GetModuleHandleExW(FROM_ADDRESS | PIN, pin_module as *const () as *const u16, &mut module) };
    if success == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Claims the process before this binary opens any database. Each independently loaded binary owns its own `OnceLock`
/// and independently acquired OS handle, so only one wins even when first opens race across worker threads.
pub fn claim() -> Result<(), Code> {
    static CLAIMED: OnceLock<io::Result<Claim>> = OnceLock::new();
    match CLAIMED.get_or_init(|| {
        let claim = acquire_claim()?;
        #[cfg(windows)]
        pin_module()?;
        Ok(claim)
    }) {
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
            format!("Cannot claim this process: {error}. Ensure the operating system permits the process claim"),
        )),
    }
}

#[cfg(all(test, unix))]
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
    fn a_symbolic_link_cannot_redirect_the_process_claim() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target");
        File::create(&target).unwrap();
        let path = directory.path().join("claim");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(acquire(&path).is_err());
    }

    #[test]
    fn an_io_failure_does_not_grant_a_claim() {
        let directory = tempfile::tempdir().unwrap();
        assert!(acquire(directory.path()).is_err());
    }
}
