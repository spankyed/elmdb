//! One elmdb binary per process. Windows uses a process-named kernel event independent of environment settings.
//! On Unix, every binary independently opens the same process claim file in private per-account state and takes a
//! nonblocking exclusive lock before opening LMDB. The winning binary keeps its handle for the process lifetime,
//! across database closes and worker teardown; another copy cannot acquire it. The file is never unlinked: replacing
//! its inode while a lock is held would let a second binary lock the replacement. A stale file holds no claim once
//! the owning process ends, so PID reuse needs no cleanup.

#[cfg(unix)]
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use napi::Result;

use crate::Code;

#[cfg(unix)]
fn claim_file() -> io::Result<PathBuf> {
    use std::os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt},
    };
    // The account database is independent of HOME/TMP settings, including worker-local JS environments.
    // SAFETY: geteuid has no preconditions. getpwuid_r receives writable, correctly sized storage; returned
    // strings are copied before that storage is dropped, and only a successful non-null result is dereferenced.
    let uid = unsafe { libc::geteuid() };
    let mut buffer = vec![0u8; 16384];
    let home = loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found = std::ptr::null_mut();
        // SAFETY: all pointers refer to live output storage for the duration of this reentrant call.
        let status =
            unsafe { libc::getpwuid_r(uid, entry.as_mut_ptr(), buffer.as_mut_ptr().cast(), buffer.len(), &mut found) };
        if status == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if found.is_null() {
            return Err(io::Error::other("the current account has no home directory"));
        }
        // SAFETY: successful getpwuid_r initialized entry and its pw_dir string in buffer.
        let entry = unsafe { entry.assume_init() };
        if entry.pw_dir.is_null() {
            return Err(io::Error::other("the current account has no home directory"));
        }
        // SAFETY: pw_dir is a terminated string owned by the still-live buffer.
        break PathBuf::from(std::ffi::OsStr::from_bytes(unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) }.to_bytes()));
    };
    if !home.is_absolute() {
        return Err(io::Error::other("the account home directory must be absolute"));
    }
    let state = home.join(".elmdb");
    let directory = state.join("process-claims");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&directory)?;
    for path in [&state, &directory] {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} must be a private directory owned by this user", path.display()),
            ));
        }
    }
    #[cfg(target_os = "linux")]
    let namespace = std::fs::metadata("/proc/self/ns/pid")?.ino().to_string();
    #[cfg(not(target_os = "linux"))]
    let namespace = "host";
    Ok(directory.join(format!("elmdb-binding-{uid}-{}-{namespace}.lock", std::process::id())))
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
        acquire(&claim_file()?)
    }
    #[cfg(windows)]
    {
        acquire_event(&format!("Local\\elmdb-binding-{}", std::process::id()))
    }
}

/// Keep the accepted Windows image and its static claim mapped after its first worker unloads it. A leaked claim
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

/// Claims the process before this binary opens any database. Each independently loaded binary owns its own claim mutex
/// and independently acquired OS handle, so only one wins even when first opens race across worker threads.
pub fn claim() -> Result<(), Code> {
    static CLAIMED: Mutex<Option<Claim>> = Mutex::new(None);
    let mut held = CLAIMED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if held.is_some() {
        return Ok(());
    }
    match (|| {
        let claim = acquire_claim()?;
        #[cfg(windows)]
        pin_module()?;
        Ok::<_, io::Error>(claim)
    })() {
        Ok(claim) => {
            *held = Some(claim);
            Ok(())
        }
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
