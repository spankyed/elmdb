//! Owned temporary files, destination exclusion and durable publication of compacted backups.

use std::io::Seek;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use super::{Registration, Store, open_paths};
use crate::error::{Error, ErrorCode, Result};

/// What a backup may replace in the directory it is made in. Whichever it is, the backup is refused where that
/// directory is a file or inside the database backed up, where another process has a database there open, and where
/// its data file is no elmdb database's (`TARGET_REFUSED`, `TARGET_IN_USE`)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackupInto {
    /// The data file of any elmdb database closed there: an earlier backup, or a database written in place, whose
    /// record the backup removes
    #[default]
    AnyClosedDatabase,
    /// Only a new directory, an empty one or an earlier backup, as the `elmdb` command makes them: a directory holding
    /// a database an open has written in place, or anything no backup or read-only open of one leaves, is refused, so
    /// that a backup named in the wrong place replaces nothing
    Backups,
}

pub(super) fn copy(store: &Store, target: &Path, into: BackupInto) -> Result<()> {
    let target = std::path::absolute(target)?;
    // The database's listeners' `wake`, whose every commit removes what isn't a mailbox, would remove the backup.
    // Looked at before anything is made, as the directory will be named
    let named = as_it_will_be_named(&target).map_err(|error| cannot_look_at(&target, &error))?;
    if is_in_wake(&named, &store.path) {
        let wake = store.path.join("wake");
        let why =
            format!("is in {}, where the database's commits remove what isn't a listener's mailbox", wake.display());
        return Err(refused(&target, &why));
    }
    // What can't be looked at was refused above, as the name it will have couldn't be read: what isn't there is made
    if let Ok(meta) = std::fs::metadata(&target) {
        if !meta.is_dir() {
            return Err(refused(&target, "is not a directory"));
        }
        // Looked at before the decision lock is made there too, so that a directory refused is left as it was. When a
        // lock file there is already held by an open or another backup, `data.mdb` is left to the check under the lock
        // below rather than read unlocked against a live writer
        may_replace_entries(&target, into)?;
        if !target_busy(&target)? {
            check_data_is_elmdb(&target)?;
        }
    }
    #[cfg(unix)]
    let sync_dirs = directories_to_sync(&target);
    std::fs::create_dir_all(&target).map_err(|error| {
        Error::new(ErrorCode::Io, format!("Cannot make the backup's directory {}: {error}", target.display()))
    })?;
    let target = target.canonicalize().map_err(|error| cannot_look_at(&target, &error))?;
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
    // Every open decides under the decision lock, made where it isn't, before it takes its presence lock, which it
    // holds shared for as long as it is open. Held here from before the presence lock is looked at until the copy is
    // published and its directory synced, the decision lock keeps any open from beginning meanwhile, so a presence
    // lock that isn't there now can't be taken before the backup ends; one that is, held by an open elsewhere, refuses
    // the backup, whose file that open would have replaced beneath it
    let deciding = super::record::open_lock_file(&target, super::record::RECORD_LOCK)
        .map_err(|error| lock_failed(&target, super::record::RECORD_LOCK, error.into()))?;
    let _deciding =
        super::record::exclusive(&deciding).map_err(|error| lock_failed(&target, super::record::RECORD_LOCK, error))?;
    let present = existing_lock(&target, super::record::PRESENCE)?;
    if let Some(presence) = &present {
        match presence.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Error::new(
                    ErrorCode::TargetInUse,
                    format!(
                        "The database at {} is open in another process, and a backup there would replace its file \
                         beneath it; close it there, or back up into another directory",
                        target.display()
                    ),
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(lock_failed(&target, super::record::PRESENCE, error.into()));
            }
        }
    }
    // Looked at here, under the decision lock: no open there begins before the copy is published, so what is looked at
    // is what the copy replaces
    run_point!(BACKUP_DECIDING);
    may_replace(&target, into)?;
    let mut partial = tempfile::Builder::new().prefix(".elmdb-backup-").tempfile_in(&target)?;
    loop {
        partial.as_file_mut().set_len(0)?;
        partial.as_file_mut().rewind()?;
        let guard = store.gate();
        match super::with_reader_recovery(&store.env, || {
            store.env.copy_to_file(partial.as_file_mut(), heed::CompactionOption::Enabled)
        }) {
            Ok(()) => {
                partial.as_file().sync_all()?;
                break;
            }
            Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                drop(guard);
                run_point!(BACKUP_MAP_GROWN at &store.path);
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
        for dir in &sync_dirs {
            std::fs::File::open(dir)?.sync_all()?;
        }
        // A record beside a data file the directory held names that file, and none of the copy's: let go once the
        // copy is there, so no open takes the copy for a file the record doesn't bear out, refused where it says
        // `failed`. Not before: a publication that failed leaves the old file, which its record still guards. And not
        // before the copy's name is synced, above: a filesystem that orders what a directory holds as it likes could
        // otherwise keep the record's removal and lose the copy, leaving the old file unguarded. Stopped between the
        // two, the copy is opened as it is under a `kept` record and refused under a `failed` one, which a backup
        // made again clears
        for gone in [super::record::RECORD, super::record::RESTORED] {
            if let Err(error) = std::fs::remove_file(target.join(gone))
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error.into());
            }
        }
        #[cfg(unix)]
        std::fs::File::open(&target)?.sync_all()?;
        Ok(())
    })();
    synced.map_err(|error| Error::new(ErrorCode::Io, format!(
        "The backup at {} was published, but syncing its directory or removing the record a database there left failed: {error}. The complete backup is visible; retrying backup is safe",
        target.display())))
}

/// `path` with links resolved as far as it exists and trailing `..` folded, the rest as it is: the directory it names
/// once made
fn as_it_will_be_named(path: &Path) -> std::io::Result<std::path::PathBuf> {
    enum Step {
        Parent,
        Normal(std::ffi::OsString),
    }
    let mut rest = Vec::new();
    let mut at = path.to_path_buf();
    loop {
        match at.canonicalize() {
            Ok(mut named) => {
                for step in rest.into_iter().rev() {
                    match step {
                        Step::Parent => {
                            named.pop();
                        }
                        Step::Normal(name) => named.push(name),
                    }
                }
                return Ok(named);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let step = match at.components().next_back() {
            Some(std::path::Component::ParentDir) => Step::Parent,
            Some(std::path::Component::Normal(name)) => Step::Normal(name.to_owned()),
            _ => return Ok(path.to_owned()),
        };
        rest.push(step);
        at.pop();
    }
}

/// Whether `path` is `store_path/wake` or inside it (matching `"wake"` case-insensitively on case-insensitive OSes so
/// a backup into `WAKE` before `wake/` exists cannot be wiped by the next commit)
fn is_in_wake(path: &Path, store_path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(store_path) else { return false };
    let Some(std::path::Component::Normal(first)) = relative.components().next() else { return false };
    #[cfg(any(windows, target_os = "macos"))]
    {
        first.to_str().is_some_and(|name| name.eq_ignore_ascii_case("wake"))
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        first == "wake"
    }
}

/// Whether an open, restore or backup already holds one of `dir`'s lock files
fn target_busy(dir: &Path) -> Result<bool> {
    for name in [super::record::RECORD_LOCK, super::record::PRESENCE] {
        if let Some(lock) = existing_lock(dir, name)? {
            match lock.try_lock() {
                Ok(()) => {}
                Err(std::fs::TryLockError::WouldBlock) => return Ok(true),
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(lock_failed(dir, name, error.into()));
                }
            }
        }
    }
    Ok(false)
}

/// Refuses what a backup into `dir` mustn't replace: under `BackupInto::Backups`, a database an open has written in
/// place and anything no backup leaves, and whatever `into` is, a data file that is no elmdb database's
fn may_replace(dir: &Path, into: BackupInto) -> Result<()> {
    may_replace_entries(dir, into)?;
    check_data_is_elmdb(dir)
}

/// Directory-level target checks that never read `data.mdb`'s bytes
fn may_replace_entries(dir: &Path, into: BackupInto) -> Result<()> {
    if into == BackupInto::Backups {
        if super::record::written_in(dir).map_err(|error| cannot_look_at(dir, &error))? {
            return Err(refused(
                dir,
                "holds a database that has been opened for writing, which a backup would replace",
            ));
        }
        let mut holds_data = false;
        let mut holds_lmdb_lock = false;
        for entry in std::fs::read_dir(dir).map_err(|error| cannot_look_at(dir, &error))? {
            let entry = entry.map_err(|error| cannot_look_at(dir, &error))?;
            let name = entry.file_name();
            // Finder's, beside anything a person has looked at on macOS
            if name == ".DS_Store" {
                continue;
            }
            if !super::record::left_by_a_backup(&name) {
                let held = name.to_string_lossy();
                return Err(refused(dir, &format!("holds {held}, which no backup leaves, so it may not be a backup")));
            }
            let kind = entry.file_type().map_err(|error| cannot_look_at(&entry.path(), &error))?;
            // `wake` is a directory of listeners' mailboxes; all else a backup leaves is a file, never a link
            let as_left = if name == "wake" { kind.is_dir() } else { kind.is_file() };
            if !as_left {
                let held = name.to_string_lossy();
                return Err(refused(dir, &format!("holds {held} of a kind no backup leaves")));
            }
            if name == "data.mdb" {
                holds_data = true;
            } else if name == "lock.mdb" {
                holds_lmdb_lock = true;
            }
        }
        if holds_lmdb_lock && !holds_data {
            let why = "holds lock.mdb and no data.mdb, which no backup leaves, so it may not be a backup";
            return Err(refused(dir, why));
        }
    } else {
        let data = dir.join("data.mdb");
        match std::fs::symlink_metadata(&data) {
            Ok(meta) if !meta.is_file() => return Err(refused(dir, "holds data.mdb of a kind no backup leaves")),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(cannot_look_at(&data, &error)),
        }
    }
    Ok(())
}

/// Refuses a `data.mdb` in `dir` that is not an elmdb database's data file
fn check_data_is_elmdb(dir: &Path) -> Result<()> {
    let data = dir.join("data.mdb");
    let mut file = match std::fs::File::open(&data) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(cannot_look_at(&data, &error)),
    };
    if !super::reach::is_elmdb_file(&mut file).map_err(|error| cannot_look_at(&data, &error))? {
        return Err(refused(
            dir,
            "holds a data.mdb that is no elmdb database's, another program's maybe, which a backup would replace",
        ));
    }
    Ok(())
}

/// A backup refused for the directory it is made in, with the fix
fn refused(dir: &Path, why: &str) -> Error {
    Error::new(
        ErrorCode::TargetRefused,
        format!("{} {why}; back up into a new directory, an empty one or an earlier backup", dir.display()),
    )
}

/// What a backup couldn't look at in the directory it is made in, which it never takes as absent
fn cannot_look_at(path: &Path, error: &dyn std::fmt::Display) -> Error {
    Error::new(
        ErrorCode::Io,
        format!("Cannot look at {}, which the backup would replace: {error}. Let this process read it", path.display()),
    )
}

/// The lock file `name` in `dir`, when an open made it there: opened to be locked, never made. Opened to write where
/// it may be, since a filesystem that emulates such locks by ranges, NFS's among them, takes an exclusive one only on
/// a file open for writing; read where it may not be, a lock file another user made
fn existing_lock(dir: &Path, name: &str) -> Result<Option<std::fs::File>> {
    let path = dir.join(name);
    let opened = std::fs::OpenOptions::new().read(true).write(true).open(&path).or_else(|_| std::fs::File::open(&path));
    match opened {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(lock_failed(dir, name, error.into())),
    }
}

/// A lock file of a backup's target that couldn't be opened or locked, named, with what to do
fn lock_failed(dir: &Path, name: &str, error: Error) -> Error {
    Error::new(
        ErrorCode::Io,
        format!(
            "Cannot lock {}, which keeps an open of a database there from meeting the backup: {}. Back up into a \
             directory this process may write, or let it read and write that file",
            dir.join(name).display(),
            error.message
        ),
    )
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
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TEMPORARY, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW, SetFileAttributesW,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: source is a live terminated path. Preserve unrelated attributes while clearing tempfile's hint
    // before publishing a durable file; a failed attribute update leaves the owned temporary file unpublished.
    let attributes = unsafe { GetFileAttributesW(source.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return Err(std::io::Error::last_os_error());
    }
    // mutant-skip: replace & with \| in move_file
    // move_file's temporary-attribute check: clearing an attribute a file doesn't have sets the attributes it has
    if attributes & FILE_ATTRIBUTE_TEMPORARY != 0 {
        let remaining = attributes & !FILE_ATTRIBUTE_TEMPORARY;
        // NORMAL is a file's attribute only when it has no other
        let kept = if remaining == 0 { FILE_ATTRIBUTE_NORMAL } else { remaining };
        // SAFETY: the same live, NUL-terminated source path as above
        if unsafe { SetFileAttributesW(source.as_ptr(), kept) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    // SAFETY: both pointers refer to live, NUL-terminated UTF-16 path buffers. The temporary file is closed and in
    // the destination directory, so the replacement stays on one volume; no delayed-reboot or cross-volume flags.
    let success = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            (if replace { MOVEFILE_REPLACE_EXISTING } else { 0 }) | MOVEFILE_WRITE_THROUGH,
        )
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
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_ARCHIVE as ARCHIVE, FILE_ATTRIBUTE_HIDDEN as HIDDEN, FILE_ATTRIBUTE_NORMAL as NORMAL,
        FILE_ATTRIBUTE_TEMPORARY as TEMPORARY, SetFileAttributesW,
    };

    fn set_attributes(path: &Path, attributes: u32) {
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
