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

/// Resolves `path` to a canonicalized path even if trailing path components do not exist yet,
/// walking components left-to-right and canonicalizing every existing prefix *before* applying `..`
/// so symlinks cannot be folded away lexically before resolution.
///
/// Rejects `..` after a nonexistent path segment and rejects dangling symlinks (fail-closed).
fn resolve_path_preserving_missing(path: &Path) -> Result<PathBuf> {
    if let Ok(canon) = fs::canonicalize(path) {
        return Ok(canon);
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| ElmdbError::new(ErrorCode::Io, format!("cannot resolve current directory: {e}")))?
            .join(path)
    };
    let mut resolved = PathBuf::new();
    let mut missing_started = false;
    for comp in abs.components() {
        match comp {
            Component::Prefix(p) => resolved.push(p.as_os_str()),
            Component::RootDir => resolved.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if missing_started {
                    return Err(ElmdbError::new(
                        ErrorCode::InvalidArgument,
                        format!("path '{}' cannot traverse '..' after a nonexistent segment", path.display()),
                    ));
                }
                resolved = fs::canonicalize(&resolved).map_err(|e| {
                    ElmdbError::new(ErrorCode::Io, format!("cannot canonicalize '{}': {e}", resolved.display()))
                })?;
                resolved.pop();
            }
            Component::Normal(seg) => {
                resolved.push(seg);
                if !missing_started {
                    match fs::canonicalize(&resolved) {
                        Ok(canon) => resolved = canon,
                        Err(e) if e.kind() == ErrorKind::NotFound => {
                            if let Ok(meta) = fs::symlink_metadata(&resolved)
                                && meta.file_type().is_symlink()
                            {
                                return Err(ElmdbError::new(
                                    ErrorCode::InvalidArgument,
                                    format!("cannot publish through dangling symlink '{}'", resolved.display()),
                                ));
                            }
                            missing_started = true;
                        }
                        Err(e) => {
                            return Err(ElmdbError::new(
                                ErrorCode::Io,
                                format!("cannot canonicalize '{}': {e}", resolved.display()),
                            ));
                        }
                    }
                }
            }
        }
    }
    Ok(resolved)
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

/// Rejects `path_b` when it equals, contains, or lies inside `path_a` (even when `path_b` does not exist yet).
pub(crate) fn ensure_paths_disjoint(path_a: &Path, path_b: &Path) -> Result<()> {
    let a_resolved = resolve_path_preserving_missing(path_a)?;
    let b_resolved = resolve_path_preserving_missing(path_b)?;
    if b_resolved.starts_with(&a_resolved) || a_resolved.starts_with(&b_resolved) {
        return Err(ElmdbError::new(
            ErrorCode::InvalidArgument,
            format!("paths '{}' and '{}' must not overlap", path_a.display(), path_b.display()),
        ));
    }
    Ok(())
}

/// Atomically publishes a pre-fsynced sibling temporary file `tmp_path` to `target` without
/// overwriting an existing file, and fsyncs the parent directory.
///
/// Uses `hard_link` on supporting filesystems and falls back to atomic `create_new` copy + fsync
/// when the filesystem rejects hard links (`Unsupported`, `CrossesDevices`, `PermissionDenied`).
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
        Err(e)
            if matches!(e.kind(), ErrorKind::Unsupported | ErrorKind::CrossesDevices | ErrorKind::PermissionDenied) =>
        {
            publish_file_create_new_fallback(tmp_path, target)
        }
        Err(e) => Err(ElmdbError::new(ErrorCode::Io, format!("cannot publish file '{}': {e}", target.display()))),
    }
}

pub(crate) fn publish_file_create_new_fallback(tmp_path: &Path, target: &Path) -> Result<()> {
    let mut dst = fs::OpenOptions::new().write(true).create_new(true).open(target).map_err(|e| {
        if e.kind() == ErrorKind::AlreadyExists {
            ElmdbError::new(ErrorCode::AlreadyExists, format!("target '{}' already exists", target.display()))
        } else {
            ElmdbError::new(ErrorCode::Io, format!("cannot create target file '{}': {e}", target.display()))
        }
    })?;
    let copy_res = (|| -> std::io::Result<()> {
        let mut src = fs::File::open(tmp_path)?;
        std::io::copy(&mut src, &mut dst)?;
        dst.sync_all()
    })();
    drop(dst);
    if let Err(e) = copy_res {
        let _ = fs::remove_file(target);
        return Err(ElmdbError::new(ErrorCode::Io, format!("cannot publish file '{}': {e}", target.display())));
    }
    let _ = fs::remove_file(tmp_path);
    OsVfs::new().sync_dir(parent_dir(target))?;
    Ok(())
}

/// Strips trailing path separators and rejects targets ending in `.`, `..`, root, or a symlink.
///
/// Checking `symlink_metadata` on the trailing-separator-stripped path ensures `lstat(2)` / Win32
/// inspects the symlink itself instead of following `link/` to its target directory.
pub(crate) fn normalize_non_symlink_target_dir(target_dir: &Path) -> Result<(PathBuf, Option<fs::Metadata>)> {
    // Reject trailing `/.` / `\.` explicitly because `Path::components()` silently drops non-leading `CurDir`
    // (empty paths, bare `.`, `..`, and root are caught by `components().next_back()` below).
    let raw = target_dir.to_string_lossy();
    let trimmed = raw.trim_end_matches(['/', '\\']);
    if trimmed.ends_with("/.") || trimmed.ends_with("\\.") {
        return Err(ElmdbError::new(
            ErrorCode::InvalidArgument,
            format!("invalid publish target directory '{}'", target_dir.display()),
        ));
    }
    match target_dir.components().next_back() {
        Some(Component::Normal(_)) => {}
        _ => {
            return Err(ElmdbError::new(
                ErrorCode::InvalidArgument,
                format!("invalid publish target directory '{}'", target_dir.display()),
            ));
        }
    }
    let clean: PathBuf = target_dir.components().collect();
    match fs::symlink_metadata(&clean) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(ElmdbError::new(
                    ErrorCode::InvalidArgument,
                    format!("refusing to publish into symlink target '{}'", target_dir.display()),
                ));
            }
            Ok((clean, Some(meta)))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok((clean, None)),
        Err(e) => Err(ElmdbError::new(
            ErrorCode::Io,
            format!("cannot inspect target directory '{}': {e}", target_dir.display()),
        )),
    }
}

/// Atomically publishes a pre-synced sibling staging directory `staging` to `target_dir`,
/// accepting only a non-existent or empty `target_dir`, and fsyncs both `staging` and `parent`.
pub(crate) fn atomic_publish_dir_new(staging: &Path, target_dir: &Path) -> Result<()> {
    let (clean_target, _existing) = normalize_non_symlink_target_dir(target_dir)?;
    let target_dir = clean_target.as_path();

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
    let (clean_target, existing) = normalize_non_symlink_target_dir(target_dir)?;
    let target_dir = clean_target.as_path();
    if existing.is_none() {
        return atomic_publish_dir_new(staging, target_dir);
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
    struct OldAsideCleanup(PathBuf);
    impl Drop for OldAsideCleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _old_cleanup = OldAsideCleanup(old_aside);
    vfs.sync_dir(parent)?;
    Ok(())
}
