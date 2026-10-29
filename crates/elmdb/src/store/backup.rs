//! Owned temporary files, destination exclusion and durable publication of compacted backups.

use std::io::Seek;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use super::{Registration, Store, open_paths};
use crate::error::{Error, ErrorCode, Result};

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
    run_point!(BACKUP_READY);
    failure_point!(BACKUP_PUBLISH, Error::new(ErrorCode::Io, "injected before publication"));
    publish(&partial, &target.join("data.mdb"))?;
    let synced = (|| -> Result<()> {
        failure_point!(BACKUP_PUBLISHED, Error::new(ErrorCode::Io, "injected sync failure"));
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
    #[cfg(not(windows))]
    {
        // tempfile renames without replacing (Linux's RENAME_NOREPLACE, macOS's RENAME_EXCL), and links where the
        // kernel or the filesystem refuses that call (ENOSYS, EINVAL). Refused, the source stays the caller's
        let renamed = tempfile::TempPath::try_from_path(source)?.persist_noclobber(target).map_err(|refused| {
            let _ = refused.path.keep();
            refused.error
        });
        #[cfg(target_os = "linux")]
        {
            exclusive_rename_or_link(source, target, renamed)
        }
        #[cfg(not(target_os = "linux"))]
        {
            renamed
        }
    }
}

#[cfg(target_os = "linux")]
fn exclusive_rename_or_link(source: &Path, target: &Path, renamed: std::io::Result<()>) -> std::io::Result<()> {
    match renamed {
        // A filesystem may refuse an exclusive rename with EOPNOTSUPP, which tempfile answers rather than linking.
        // Linking the already synced private file publishes it atomically and also refuses an existing target; the
        // caller's staging directory removes the source.
        Err(error) if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)) => {
            std::fs::hard_link(source, target)
        }
        result => result,
    }
}

#[cfg(all(test, windows))]
mod windows_publication_tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;

    const HIDDEN: u32 = 0x2;
    const ARCHIVE: u32 = 0x20;
    const NORMAL: u32 = 0x80;
    const TEMPORARY: u32 = 0x100;

    fn set_attributes(path: &Path, attributes: u32) {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn SetFileAttributesW(path: *const u16, attributes: u32) -> i32;
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: wide is a live NUL-terminated UTF-16 path for the call's duration
        assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), attributes) }, 0, "{}", std::io::Error::last_os_error());
    }

    #[test]
    fn publishing_clears_the_temporary_attribute_and_keeps_every_other() {
        for (before, after) in [(TEMPORARY, 0), (TEMPORARY | HIDDEN, HIDDEN), (HIDDEN, HIDDEN)] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            let target = dir.path().join("target");
            std::fs::write(&source, b"complete").unwrap();
            set_attributes(&source, before);
            publish_new(&source, &target).unwrap();
            // Windows marks a moved file for archiving, and NORMAL is only ever a file's sole attribute
            let published = std::fs::metadata(&target).unwrap().file_attributes();
            assert_eq!(published & !(ARCHIVE | NORMAL), after, "published {published:#x} from {before:#x}");
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod publication_tests {
    use super::*;

    #[test]
    fn unsupported_exclusive_rename_falls_back_without_overwriting() {
        for code in [libc::ENOSYS, libc::EINVAL, libc::EOPNOTSUPP] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("source");
            let target = dir.path().join("target");
            std::fs::write(&source, b"complete").unwrap();
            exclusive_rename_or_link(&source, &target, Err(std::io::Error::from_raw_os_error(code))).unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), b"complete");
            std::fs::remove_file(&source).unwrap();
            std::fs::write(&source, b"replacement").unwrap();
            assert_eq!(
                exclusive_rename_or_link(&source, &target, Err(std::io::Error::from_raw_os_error(code)))
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::AlreadyExists
            );
            assert_eq!(std::fs::read(&target).unwrap(), b"complete");
        }
    }

    #[test]
    fn other_rename_errors_do_not_attempt_publication() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        std::fs::write(&source, b"complete").unwrap();
        for code in [libc::EEXIST, libc::EACCES, libc::EIO] {
            assert_eq!(
                exclusive_rename_or_link(&source, &target, Err(std::io::Error::from_raw_os_error(code)))
                    .unwrap_err()
                    .raw_os_error(),
                Some(code)
            );
            assert!(!target.exists());
        }
    }
}
