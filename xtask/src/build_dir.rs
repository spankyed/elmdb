//! The build directories a checkout builds into, kept within bounds without anyone having to remember to
//! (`docs/architecture/testing.md`, The build directory).
//!
//! Cargo never removes an artifact: each build of other dependencies, features, flags or profile hashes a new one beside
//! the last. `sweep` bounds each build directory this checkout uses: its target, the concurrency models' (`target/loom`,
//! `target/shuttle`, which `pre-push` builds into), and the scratch target a mutated or scratch copy of the tree builds
//! into (`scratch`). cargo-sweep removes the artifacts, and this the incremental compilation directories cargo-sweep
//! leaves, one for every variant of a crate ever built. `pre-push` sweeps before it builds anything, and then refuses
//! to start below `FLOOR` free, so a heavy run never fills the disk part way through.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// The cargo-sweep this task runs, pinned as the repository pins its dependencies
const CARGO_SWEEP: &str = "0.8.0";
/// Artifacts no build has used for this many days go
const SWEEP_DAYS: u64 = 14;
/// And past these sizes, the oldest go until each directory fits: the checkout's target and the scratch target, which
/// hold the suites' builds, and the models', which hold one crate's
const TARGET_MAX: &str = "15GB";
const MODELS_MAX: &str = "5GB";
/// The incremental directories of one crate's variants kept besides those used within `SWEEP_DAYS`: a variant used
/// since is rebuilt from scratch once, which is slower and no less right
const INCREMENTAL_KEPT: usize = 2;
/// An incremental directory written to within this long may be a build's that is running now, from another checkout
/// that shares the directory, and is left
const IN_USE: Duration = Duration::from_secs(60 * 60);
/// What a run that builds must find free before it starts: the suites, the models and the binding build tens of
/// gigabytes between them on a cold directory
pub const FLOOR: u64 = 20 << 30;

/// The directory this checkout's builds go to, as cargo resolves it: `CARGO_TARGET_DIR`, or the workspace's `target/`
pub fn target_dir(root: &Path) -> Result<PathBuf, String> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo metadata failed: {}", String::from_utf8_lossy(&output.stderr)));
    }
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata's answer: {e}"))?;
    metadata["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .ok_or_else(|| "cargo metadata names no target directory".to_owned())
}

/// Where a mutated or scratch copy of the tree builds: beside this checkout's target and not inside it, so a sweep of
/// either measures and removes only its own, and shared by every such copy, whatever its path, so one sweep bounds
/// them all
pub fn scratch(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push("-scratch");
    target.with_file_name(name)
}

/// Each build directory to sweep, with the size it is held to: those that exist of the target, the scratch target and
/// the models'
pub fn build_dirs(root: &Path, target: &Path) -> Vec<(PathBuf, &'static str)> {
    let models = [root.join("target/loom"), root.join("target/shuttle")];
    let mut dirs = vec![(target.to_owned(), TARGET_MAX), (scratch(target), TARGET_MAX)];
    dirs.extend(models.into_iter().map(|dir| (dir, MODELS_MAX)));
    dirs.retain(|(dir, _)| dir.is_dir());
    dirs.dedup_by(|a, b| a.0 == b.0);
    dirs
}

/// `cargo xtask sweep`: every build directory of this checkout bounded, printing what each pass freed
pub fn sweep(root: &Path) -> Result<(), String> {
    let installed = Command::new("cargo").args(["sweep", "--version"]).output().is_ok_and(|out| out.status.success());
    if !installed {
        return Err(format!(
            "cargo-sweep isn't installed, and every build directory is bounded with it: cargo install cargo-sweep \
             --version '={CARGO_SWEEP}' --locked"
        ));
    }
    let days = SWEEP_DAYS.to_string();
    for (dir, max) in build_dirs(root, &target_dir(root)?) {
        println!("sweep: {}", dir.display());
        for pass in [vec!["--installed"], vec!["--time", &days], vec!["--maxsize", max]] {
            // cargo-sweep finds the directory as cargo does, so it is named the way a build would name it
            let status = Command::new("cargo")
                .arg("sweep")
                .args(&pass)
                .arg(root)
                .env("CARGO_TARGET_DIR", &dir)
                .status()
                .map_err(|e| format!("cargo sweep {}: {e}", pass.join(" ")))?;
            if !status.success() {
                return Err(format!("cargo sweep {} of {} failed with {status}", pass.join(" "), dir.display()));
            }
        }
        let freed = prune_incremental(&dir, SystemTime::now())?;
        println!("sweep: {} of incremental compilation directories freed", gigabytes(freed));
    }
    Ok(())
}

/// Removes, from each `incremental/` directory under `dir`, a crate's variants that no build has written to within
/// `SWEEP_DAYS`, and of those it has, all but the `INCREMENTAL_KEPT` latest that are not in use: rustc keeps a crate's
/// latest session within one variant's directory, and nothing removes the directories of the variants. Answers the
/// bytes freed
pub fn prune_incremental(dir: &Path, now: SystemTime) -> Result<u64, String> {
    let mut freed = 0;
    for incremental in incremental_dirs(dir) {
        let mut variants: Vec<(String, PathBuf, SystemTime)> = std::fs::read_dir(&incremental)
            .map_err(|e| format!("{}: {e}", incremental.display()))?
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let (krate, _) = name.rsplit_once('-')?;
                Some((krate.to_owned(), entry.path(), latest_write(&entry.path())))
            })
            .collect();
        // Each crate's latest first
        variants.sort_by(|a, b| a.0.cmp(&b.0).then(b.2.cmp(&a.2)));
        let mut kept_of = (String::new(), 0usize);
        for (krate, path, written) in variants {
            if kept_of.0 != krate {
                kept_of = (krate, 0);
            }
            let age = now.duration_since(written).unwrap_or_default();
            let recent = age < Duration::from_secs(SWEEP_DAYS * 24 * 60 * 60);
            if age < IN_USE {
                continue;
            }
            if recent && kept_of.1 < INCREMENTAL_KEPT {
                kept_of.1 += 1;
                continue;
            }
            freed += size(&path);
            std::fs::remove_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    Ok(freed)
}

/// The `incremental/` directories of a build directory's profiles, its targets' among them (`<triple>/<profile>/`)
fn incremental_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut under = vec![(dir.to_owned(), 0)];
    while let Some((at, depth)) = under.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else { continue };
        for entry in entries.filter_map(Result::ok).filter(|entry| entry.path().is_dir()) {
            if entry.file_name() == "incremental" {
                found.push(entry.path());
            } else if depth < 2 {
                under.push((entry.path(), depth + 1));
            }
        }
    }
    found
}

/// When a build last wrote in `path`: the latest of the sessions and locks rustc keeps in it, or the directory's own
/// time where it holds none
fn latest_write(path: &Path) -> SystemTime {
    let written = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| written(&entry.path()))
        .max()
        .unwrap_or_else(|| written(path))
}

/// The bytes the files under `path` hold
fn size(path: &Path) -> u64 {
    let mut total = 0;
    let mut under = vec![path.to_owned()];
    while let Some(at) = under.pop() {
        let Ok(entries) = std::fs::read_dir(&at) else { continue };
        for entry in entries.filter_map(Result::ok) {
            match entry.metadata() {
                Ok(metadata) if metadata.is_dir() => under.push(entry.path()),
                Ok(metadata) => total += metadata.len(),
                Err(_) => {}
            }
        }
    }
    total
}

fn gigabytes(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / f64::from(1 << 30))
}

/// The bytes free to this user on the filesystem `path` is on, where the platform says
#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a NUL-terminated string that outlives the call, and statvfs fills the struct it is given when
    // it answers 0, which is checked before the struct is read
    let stat = unsafe {
        if libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return None;
        }
        stat.assume_init()
    };
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

/// No reading where the platform gives none here: the run goes on as it did
#[cfg(not(unix))]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Refuses a run that builds when the filesystem holding `dir` has less than `floor` free, saying what to do
pub fn refuse_below(dir: &Path, free: Option<u64>, floor: u64) -> Result<(), String> {
    match free {
        Some(free) if free < floor => Err(format!(
            "{} has {} free, less than the {} a run that builds needs, which would fill the disk part way through. \
             Free space first: `cargo xtask sweep` bounds this checkout's build directories, and a build directory \
             no checkout uses any more can be deleted",
            dir.display(),
            gigabytes(free),
            gigabytes(floor)
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scratch_target_is_beside_the_target_and_not_in_it() {
        assert_eq!(scratch(Path::new("/w/value-blocks/target")), Path::new("/w/value-blocks/target-scratch"));
        assert_eq!(scratch(Path::new("/w/order-ties/target")), Path::new("/w/order-ties/target-scratch"));
    }

    #[test]
    fn the_build_dirs_are_those_that_exist_each_once() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir_all(target.join("loom")).unwrap();
        let found = build_dirs(root.path(), &target);
        assert_eq!(found, vec![(target.clone(), TARGET_MAX), (target.join("loom"), MODELS_MAX)]);
        std::fs::create_dir_all(scratch(&target)).unwrap();
        assert_eq!(build_dirs(root.path(), &target).len(), 3);
    }

    /// An incremental directory of `krate` whose session was last written `ago`
    fn variant(incremental: &Path, krate: &str, hash: &str, ago: Duration, now: SystemTime) -> PathBuf {
        let dir = incremental.join(format!("{krate}-{hash}"));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("s-session");
        std::fs::write(&session, vec![0u8; 1000]).unwrap();
        std::fs::File::options().write(true).open(&session).unwrap().set_modified(now - ago).unwrap();
        dir
    }

    #[test]
    fn a_crate_s_old_and_extra_incremental_directories_go_and_its_latest_and_those_in_use_stay() {
        let dir = tempfile::tempdir().unwrap();
        let incremental = dir.path().join("debug/incremental");
        let now = SystemTime::now();
        let day = Duration::from_secs(24 * 60 * 60);
        let latest = variant(&incremental, "elmdb", "aaa", day, now);
        let second = variant(&incremental, "elmdb", "bbb", 2 * day, now);
        let third = variant(&incremental, "elmdb", "ccc", 3 * day, now);
        let stale = variant(&incremental, "elmdb", "ddd", 30 * day, now);
        let running = variant(&incremental, "elmdb", "eee", Duration::from_secs(60), now);
        let other = variant(&incremental, "xtask", "fff", 3 * day, now);
        let triple = variant(&dir.path().join("aarch64-apple-darwin/debug/incremental"), "cli", "ggg", 30 * day, now);
        let freed = prune_incremental(dir.path(), now).unwrap();
        for kept in [&latest, &second, &running, &other] {
            assert!(kept.exists(), "{} was removed", kept.display());
        }
        for removed in [&third, &stale, &triple] {
            assert!(!removed.exists(), "{} was kept", removed.display());
        }
        assert_eq!(freed, 3000);
    }

    #[test]
    fn a_run_is_refused_below_the_floor_and_not_at_it() {
        let dir = Path::new("/w/target");
        assert!(refuse_below(dir, Some(FLOOR - 1), FLOOR).unwrap_err().contains("cargo xtask sweep"));
        assert!(refuse_below(dir, Some(FLOOR), FLOOR).is_ok());
        assert!(refuse_below(dir, None, FLOOR).is_ok(), "no reading refuses nothing");
    }

    #[cfg(unix)]
    #[test]
    fn the_free_space_read_is_the_filesystem_s() {
        let free = free_bytes(Path::new(env!("CARGO_MANIFEST_DIR"))).expect("a unix filesystem answers");
        assert!(free > 0);
        assert_eq!(free_bytes(Path::new("/no/such/path/at/all")), None);
    }
}
