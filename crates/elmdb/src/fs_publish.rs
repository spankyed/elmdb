use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use elmdb_storage::{OsVfs, Vfs};
use elmdb_wire::{ElmdbError, ErrorCode, Result};

/// Returns the parent directory of `path`, defaulting to `.` when `path` is a bare filename.
pub(crate) fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Lexically normalizes `.` and `..` components onto an absolute path without requiring
/// intermediate segments to exist on disk.
fn normalize_absolute(path: &Path) -> Result<PathBuf> {
    let abs = std::env::current_dir()
        .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot resolve current directory: {e}")))?
        .join(path);
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

/// Resolves `path` to a canonicalized path even if trailing path components do not exist yet,
/// by lexically normalizing `.`/`..`, canonicalizing the deepest existing ancestor, and
/// re-appending the remaining suffix.
fn resolve_path_preserving_missing(path: &Path) -> Result<PathBuf> {
    if let Ok(canon) = fs::canonicalize(path) {
        return Ok(canon);
    }
    let normalized = normalize_absolute(path)?;
    let mut missing = Vec::new();
    let mut cur = normalized.as_path();
    loop {
        if let Ok(canon) = fs::canonicalize(cur) {
            let mut out = canon;
            for part in missing.into_iter().rev() {
                out.push(part);
            }
            return Ok(out);
        }
        let Some(name) = cur.file_name() else {
            return Ok(normalized);
        };
        missing.push(name.to_os_string());
        let Some(parent) = cur.parent() else {
            return Ok(normalized);
        };
        cur = parent;
    }
}

/// Rejects publishing into `source_dir` itself, any descendant inside `source_dir`,
/// or any ancestor containing `source_dir`.
pub(crate) fn ensure_not_self_or_descendant(source_dir: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    if let (Ok(sm), Ok(tm)) = (fs::metadata(source_dir), fs::metadata(target)) {
        use std::os::unix::fs::MetadataExt;
        if (sm.dev(), sm.ino()) == (tm.dev(), tm.ino()) {
            return Err(ElmdbError::new(
                ErrorCode::InvalidArgument,
                format!("cannot publish into live database directory '{}'", target.display()),
            ));
        }
    }

    let src_canon = fs::canonicalize(source_dir)
        .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot canonicalize database directory: {e}")))?;
    let target_resolved = resolve_path_preserving_missing(target)?;
    if target_resolved.starts_with(&src_canon) || src_canon.starts_with(&target_resolved) {
        return Err(ElmdbError::new(
            ErrorCode::InvalidArgument,
            format!("cannot publish into live database directory, ancestor, or subdirectory '{}'", target.display()),
        ));
    }
    Ok(())
}

/// Atomically publishes a pre-fsynced sibling temporary file `tmp_path` to `target` without
/// overwriting an existing file, and fsyncs the parent directory.
pub(crate) fn atomic_publish_file_no_overwrite(tmp_path: &Path, target: &Path) -> Result<()> {
    match fs::hard_link(tmp_path, target) {
        Ok(()) => {
            let _ = fs::remove_file(tmp_path);
            OsVfs::new().sync_dir(parent_dir(target))?;
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            Err(ElmdbError::new(ErrorCode::AlreadyExists, format!("target '{}' already exists", target.display())))
        }
        Err(e) => Err(ElmdbError::new(ErrorCode::Io, format!("cannot publish file '{}': {e}", target.display()))),
    }
}

/// Atomically publishes a pre-synced sibling staging directory `staging` to `target_dir`,
/// accepting only a non-existent or empty `target_dir`, and fsyncs both `staging` and `parent`.
pub(crate) fn atomic_publish_dir_new(staging: &Path, target_dir: &Path) -> Result<()> {
    let vfs = OsVfs::new();
    vfs.sync_dir(staging)?;

    #[cfg(unix)]
    {
        // POSIX `rename(2)` atomically replaces an existing empty directory in a single syscall
        // and rejects a non-empty directory or non-directory with `ENOTEMPTY` / `EEXIST` / `ENOTDIR`.
        match fs::rename(staging, target_dir) {
            Ok(()) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::DirectoryNotEmpty | ErrorKind::AlreadyExists | ErrorKind::NotADirectory
                ) =>
            {
                return Err(ElmdbError::new(
                    ErrorCode::AlreadyExists,
                    format!("target directory '{}' is not empty", target_dir.display()),
                ));
            }
            Err(e) => {
                return Err(ElmdbError::new(
                    ErrorCode::Io,
                    format!("cannot move directory into place '{}': {e}", target_dir.display()),
                ));
            }
        }
    }

    #[cfg(not(unix))]
    {
        let removed_empty = if target_dir.exists() {
            match fs::remove_dir(target_dir) {
                Ok(()) => true,
                Err(e) if e.kind() == ErrorKind::DirectoryNotEmpty => {
                    return Err(ElmdbError::new(
                        ErrorCode::AlreadyExists,
                        format!("target directory '{}' is not empty", target_dir.display()),
                    ));
                }
                Err(e) => {
                    return Err(ElmdbError::new(
                        ErrorCode::Io,
                        format!("cannot prepare empty target directory '{}': {e}", target_dir.display()),
                    ));
                }
            }
        } else {
            false
        };
        if let Err(e) = fs::rename(staging, target_dir) {
            if removed_empty {
                let _ = fs::create_dir(target_dir);
            }
            let code = if matches!(e.kind(), ErrorKind::DirectoryNotEmpty | ErrorKind::AlreadyExists) {
                ErrorCode::AlreadyExists
            } else {
                ErrorCode::Io
            };
            return Err(ElmdbError::new(
                code,
                format!("cannot move directory into place '{}': {e}", target_dir.display()),
            ));
        }
    }

    vfs.sync_dir(parent_dir(target_dir))?;
    Ok(())
}

/// Atomically publishes a pre-synced sibling staging directory `staging` onto `target_dir`,
/// replacing an existing non-empty directory ONLY if it contains `backup_marker`.
pub(crate) fn atomic_publish_dir_replace(
    staging: &Path,
    target_dir: &Path,
    backup_marker: &str,
    nonce: u64,
) -> Result<()> {
    match fs::symlink_metadata(target_dir) {
        Err(e) if e.kind() == ErrorKind::NotFound => return atomic_publish_dir_new(staging, target_dir),
        Err(e) => {
            return Err(ElmdbError::new(ErrorCode::Io, format!("cannot inspect backup target: {e}")));
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(ElmdbError::new(
                ErrorCode::InvalidArgument,
                format!("refusing to replace symlink backup target '{}'", target_dir.display()),
            ));
        }
        Ok(_) => {}
    }

    // Drop `ReadDir` immediately before renaming/removing so Windows never hits `ERROR_ACCESS_DENIED`
    // / `ERROR_SHARING_VIOLATION` from an open directory search handle.
    let is_empty = fs::read_dir(target_dir)
        .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot read backup target: {e}")))?
        .next()
        .is_none();
    if is_empty {
        return atomic_publish_dir_new(staging, target_dir);
    }

    if !target_dir.join(backup_marker).exists() {
        return Err(ElmdbError::new(
            ErrorCode::AlreadyExists,
            format!("refusing to overwrite non-backup directory '{}' (missing {backup_marker})", target_dir.display()),
        ));
    }

    let vfs = OsVfs::new();
    vfs.sync_dir(staging)?;

    let parent = parent_dir(target_dir);
    let old_aside = parent.join(format!(".elmdb-backup-old-{}-{nonce}", std::process::id()));
    fs::rename(target_dir, &old_aside)
        .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot stage old backup for swap: {e}")))?;
    if let Err(e) = fs::rename(staging, target_dir) {
        let _ = fs::rename(&old_aside, target_dir);
        return Err(ElmdbError::new(ErrorCode::Io, format!("cannot move backup into place: {e}")));
    }
    vfs.sync_dir(parent)?;
    let _ = fs::remove_dir_all(&old_aside);
    Ok(())
}
