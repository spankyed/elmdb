//! Owned temporary files, destination exclusion and durable publication of compacted backups.

use std::io::Seek;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use super::{Registration, Store, open_paths};
use crate::error::{Error, ErrorCode, Result};

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Point {
    Ready,
    Published,
}

#[cfg(test)]
type Hook = Box<dyn FnMut(Point, &Path) -> Result<()>>;

#[cfg(test)]
thread_local! {
    pub(super) static HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn hook(point: Point, path: &Path) -> Result<()> {
    HOOK.with_borrow_mut(|hook| hook.as_mut().map_or(Ok(()), |hook| hook(point, path)))
}

pub(super) fn copy(store: &Store, target: &Path) -> Result<()> {
    let target = std::path::absolute(target)?;
    #[cfg(unix)]
    let sync_dirs = directories_to_sync(&target);
    std::fs::create_dir_all(&target)?;
    let target = target.canonicalize()?;
    if !open_paths().lock().expect("the open-paths lock is never poisoned").insert(target.clone()) {
        return Err(Error::new(
            ErrorCode::AlreadyOpen,
            format!(
                "The database at {} is open or a backup is being published there; use another directory",
                target.display()
            ),
        ));
    }
    let _reservation = Registration(Some(target.clone()));
    let mut partial = tempfile::Builder::new().prefix(".elmdb-backup-").tempfile_in(&target)?;
    loop {
        partial.as_file_mut().set_len(0)?;
        partial.as_file_mut().rewind()?;
        let guard = store.gate();
        match store.env.copy_to_file(partial.as_file_mut(), heed::CompactionOption::Enabled) {
            Ok(()) => {
                partial.as_file().sync_all()?;
                break;
            }
            Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                drop(guard);
                store.adopt_map_size()?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    // Closing the handle before publication also permits replacement on Windows. TempPath owns cleanup on failure.
    let partial = partial.into_temp_path();
    #[cfg(test)]
    hook(Point::Ready, &partial)?;
    publish(&partial, &target.join("data.mdb"))?;
    let synced = (|| -> Result<()> {
        #[cfg(test)]
        hook(Point::Published, &target)?;
        #[cfg(unix)]
        for dir in sync_dirs {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    })();
    synced.map_err(|error| Error::new(ErrorCode::Io, format!(
        "The backup at {} was published, but directory syncing failed: {error}. The complete backup is visible; retrying backup is safe",
        target.display())))
}

/// Sync the destination and each newly created ancestor's parent, deepest first. Existing ancestors above the first
/// existing directory have no new entry to persist. No temporary filename belongs to another invocation.
#[cfg(unix)]
pub(crate) fn directories_to_sync(target: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![target.to_owned()];
    let mut current = target;
    while !current.exists() {
        let Some(parent) = current.parent().filter(|path| !path.as_os_str().is_empty()) else { break };
        dirs.push(parent.to_owned());
        current = parent;
    }
    dirs
}

#[cfg(not(windows))]
fn publish(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(windows)]
fn publish(source: &Path, target: &Path) -> std::io::Result<()> {
    move_file(source, target, true)
}

#[cfg(windows)]
fn move_file(source: &Path, target: &Path, replace: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(source: *const u16, target: *const u16, flags: u32) -> i32;
        fn GetFileAttributesW(path: *const u16) -> u32;
        fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32;
    }
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: source is a live terminated path. Preserve unrelated attributes while clearing tempfile's hint
    // before publishing a durable file; a failed attribute update leaves the owned temporary file unpublished.
    let attributes = unsafe { GetFileAttributesW(source.as_ptr()) };
    if attributes == u32::MAX {
        return Err(std::io::Error::last_os_error());
    }
    if attributes & 0x100 != 0 {
        let remaining = attributes & !0x100;
        // SAFETY: the same valid source path is retained; NORMAL is used when no other attribute remains.
        if unsafe { SetFileAttributesW(source.as_ptr(), if remaining == 0 { 0x80 } else { remaining }) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    const REPLACE_EXISTING: u32 = 1;
    const WRITE_THROUGH: u32 = 8;
    // SAFETY: both pointers refer to live, NUL-terminated UTF-16 path buffers. The temporary file is closed and in
    // the destination directory, so the replacement stays on one volume; no delayed-reboot or cross-volume flags.
    let success = unsafe {
        MoveFileExW(source.as_ptr(), target.as_ptr(), (if replace { REPLACE_EXISTING } else { 0 }) | WRITE_THROUGH)
    };
    if success == 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// Install a complete private file without replacing any existing destination.
pub(crate) fn publish_new(source: &Path, target: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        move_file(source, target, false)
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())?;
        let target = std::ffi::CString::new(target.as_os_str().as_bytes())?;
        #[cfg(target_os = "linux")]
        // SAFETY: both paths remain live terminated strings; AT_FDCWD uses their absolute paths and
        // RENAME_NOREPLACE atomically refuses an existing destination without requiring hard links.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        // SAFETY: the two live terminated paths are on the same volume; RENAME_EXCL refuses replacement atomically.
        let result = unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        std::fs::hard_link(source, target)
    }
}
