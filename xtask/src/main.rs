//! The repository's own tasks, run as `cargo xtask <task>`:
//!
//! - `generate`: writes the TypeScript declarations of the protocol's types into `crates/elmdb-node/protocol/`.
//! - `check-generated`: fails when what `generate` would write differs from what is committed.
//! - `check-one-lmdb`: fails unless the dependency tree holds exactly one LMDB `-sys` crate (one version of one).
//! - `check-diagnostics-off`: fails when a default build of the workspace has `elmdb` depend on `tracing` itself,
//!   which only its `diagnostics` feature may make it do.
//! - `check-leiden-serial`: fails when the workspace builds `leiden-rs` with any feature, `rayon` among them, whose
//!   parallel moves would make a partition depend on the threads' timing rather than on its seed alone.
//! - `check-sync`: fails when engine code outside `src/sync/` locks, waits or keeps per-thread state through `std` or
//!   parking_lot directly, rather than through `crate::sync`, which the concurrency models swap for Loom's and
//!   Shuttle's, unless the file is listed in `UNMODELLED` with its reason; a listed file that no longer does is stale.
//! - `check-mutant-exclusions`: fails when an `exclude_re` in `.cargo/mutants.toml` matches none of the mutants
//!   cargo-mutants lists: each names a mutant by its line, so an edit above it leaves the exclusion naming nothing, the
//!   mutant it was written for reported again, and the line free for another mutant to be skipped unread.
//! - `compare`: runs one command on two revisions in turn and prints each reading beside the load as its run ended; it
//!   reports and decides nothing (`docs/architecture/testing.md`, Comparing two revisions).
//! - `sweep`: removes from the build directory what no recent build used, through `cargo-sweep` (`docs/architecture/
//!   testing.md`, The build directory).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod compare;
mod generate;

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("generate") => generate::write(&generated_dir()),
        Some("compare") => compare::compare(&workspace_root(), &std::env::args().skip(2).collect::<Vec<_>>()),
        Some("check-generated") => check_generated(),
        Some("check-one-lmdb") => check_one_lmdb(&workspace_root()),
        Some("check-diagnostics-off") => check_diagnostics_off(&workspace_root()),
        Some("check-leiden-serial") => check_leiden_serial(&workspace_root()),
        Some("sweep") => sweep(&workspace_root()),
        Some("check-sync") => check_sync(&workspace_root()),
        Some("check-mutant-exclusions") => check_mutant_exclusions(&workspace_root()),
        _ => Err(
            "usage: cargo xtask <generate | check-generated | check-one-lmdb | check-diagnostics-off | check-leiden-serial | check-sync | check-mutant-exclusions | compare | sweep>"
                .to_owned(),
        ),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// The cargo-sweep this task runs, pinned as the repository pins its dependencies
const CARGO_SWEEP: &str = "0.8.0";
/// Artifacts no build has used for this many days go
const SWEEP_DAYS: &str = "14";
/// And past this size, the oldest go until the build directory fits
const SWEEP_MAX_SIZE: &str = "15GB";

/// Cargo never removes an artifact from a build directory: a build of other code or other features hashes a new one
/// beside the last, so a long-lived build directory grows with every branch built in it. This removes what a toolchain
/// no longer installed made, what no build has used for `SWEEP_DAYS`, and then the oldest until it is under
/// `SWEEP_MAX_SIZE`, printing what each pass freed
fn sweep(root: &Path) -> Result<(), String> {
    let installed = Command::new("cargo").args(["sweep", "--version"]).output().is_ok_and(|out| out.status.success());
    if !installed {
        return Err(format!(
            "cargo-sweep isn't installed: cargo install cargo-sweep --version '={CARGO_SWEEP}' --locked"
        ));
    }
    for pass in [vec!["--installed"], vec!["--time", SWEEP_DAYS], vec!["--maxsize", SWEEP_MAX_SIZE]] {
        let status = Command::new("cargo")
            .arg("sweep")
            .args(&pass)
            .arg(root)
            .status()
            .map_err(|e| format!("cargo sweep {}: {e}", pass.join(" ")))?;
        if !status.success() {
            return Err(format!("cargo sweep {} failed with {status}", pass.join(" ")));
        }
    }
    Ok(())
}

/// The engine's files that lock, wait or keep per-thread state outside `crate::sync`, each with why: what isn't
/// listed here goes through it, so the models can run it on their own threads. A model of one of these is how it
/// leaves the list
const UNMODELLED: &[(&str, &str)] = &[
    ("database.rs", "the open store, the schema and the index builder behind the handle: no model of opening yet"),
    ("diagnostics/collector.rs", "a collector an embedding program installs, outside the engine's own concurrency"),
    ("engine/derive/sorted.rs", "a test-only scratch budget per thread, no shared state"),
    ("engine/mod.rs", "the schema read lock, a parking_lot RwLock no model provides"),
    ("engine/values.rs", "a per-thread decompression buffer, no shared state"),
    ("engine/watch.rs", "the schema read lock; the tracks and their delivery go through crate::sync"),
    ("failpoints.rs", "test-only failure points, armed per thread"),
    ("store/idle_reads.rs", "idle read snapshot release and its waiters: no model yet"),
    (
        "store/mod.rs",
        "the writer gate, a parking_lot RecursiveRwLock no model provides, and the paths open in the process",
    ),
    ("store/sync.rs", "the group commit sync worker, tested by its own deterministic harness (store/sync/tests.rs)"),
];

/// Fails on an engine file that locks outside `crate::sync` and isn't in `UNMODELLED`, and on a listed file that no
/// longer does
fn check_sync(root: &Path) -> Result<(), String> {
    let source = root.join("crates/elmdb/src");
    let found = unmodelled_files(&source)?;
    let listed: std::collections::BTreeSet<String> = UNMODELLED.iter().map(|(file, _)| (*file).to_owned()).collect();
    let unlisted: Vec<_> = found.difference(&listed).collect();
    let stale: Vec<_> = listed.difference(&found).collect();
    if unlisted.is_empty() && stale.is_empty() {
        return Ok(());
    }
    let mut message = String::new();
    if !unlisted.is_empty() {
        message += &format!(
            "these lock, wait or keep per-thread state outside crate::sync, which the concurrency models can't run: \
             {unlisted:?}; use crate::sync's primitives, or list the file in xtask's UNMODELLED with why\n"
        );
    }
    if !stale.is_empty() {
        message += &format!("UNMODELLED lists files that no longer lock outside crate::sync: {stale:?}; remove them\n");
    }
    Err(message.trim_end().to_owned())
}

fn check_mutant_exclusions(root: &Path) -> Result<(), String> {
    let config = root.join(".cargo/mutants.toml");
    let text = std::fs::read_to_string(&config).map_err(|e| format!("{}: {e}", config.display()))?;
    let exclusions = exclude_res(&text)?;
    // Every mutant, the configuration's exclusions and globs not applied, so each exclusion is tried against them all;
    // uncoloured, since CI's `CARGO_TERM_COLOR=always` would wrap each name's parts in escapes no pattern matches
    let output = Command::new("cargo")
        .args(["mutants", "--list", "--no-config", "--colors", "never"])
        .current_dir(root)
        .output()
        .map_err(|e| {
            format!("cargo mutants --list: {e}; install cargo-mutants, as .github/workflows/mutants.yml pins it")
        })?;
    if !output.status.success() {
        return Err(format!("cargo mutants --list failed: {}", String::from_utf8_lossy(&output.stderr).trim()));
    }
    let listed = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let mutants: Vec<&str> = listed.lines().filter(|line| !line.is_empty()).collect();
    if mutants.is_empty() {
        return Err("cargo mutants --list listed no mutants, so no exclusion could be tried".to_owned());
    }
    // A pattern naming several lines, `file.rs:(12|34):`, is tried a line at a time, so one line still matching doesn't
    // hide another that has moved
    let lines = regex::Regex::new(r"\.rs:\(([0-9|]+)\):").expect("a valid pattern");
    let mut unmatched = Vec::new();
    for exclusion in &exclusions {
        let each: Vec<String> = match lines.captures(exclusion) {
            Some(found) => found[1]
                .split('|')
                .map(|line| lines.replace(exclusion, format!(".rs:{line}:").as_str()).into_owned())
                .collect(),
            None => vec![exclusion.clone()],
        };
        for one in each {
            let pattern = regex::Regex::new(&one).map_err(|e| format!("exclude_re {exclusion:?}: {e}"))?;
            if !mutants.iter().any(|mutant| pattern.is_match(mutant)) {
                unmatched.push(one);
            }
        }
    }
    if unmatched.is_empty() {
        return Ok(());
    }
    Err(format!(
        "these exclude_re in .cargo/mutants.toml match none of the {} mutants listed, most likely since the line they \
         name moved: {unmatched:#?}; point each at the line its mutant is on now, or remove it",
        mutants.len()
    ))
}

/// The patterns of `exclude_re`, each a TOML basic string on a line of its own as the file writes them; an empty list
/// is an error, since that is a file this no longer reads
fn exclude_res(text: &str) -> Result<Vec<String>, String> {
    let start = 1 + text.find("\nexclude_re = [").ok_or("no exclude_re list in .cargo/mutants.toml")?;
    let body = &text[start..];
    let end = body.find("\n]").ok_or("exclude_re's list doesn't close")?;
    let mut patterns = Vec::new();
    for line in body[..end].lines().skip(1).map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let quoted = line.strip_suffix(',').unwrap_or(line);
        let inner = quoted
            .strip_prefix('"')
            .and_then(|l| l.strip_suffix('"'))
            .ok_or_else(|| format!("exclude_re holds a line this doesn't read: {line}"))?;
        patterns.push(inner.replace("\\\\", "\\").replace("\\\"", "\""));
    }
    if patterns.is_empty() {
        return Err("exclude_re in .cargo/mutants.toml holds no pattern this could read".to_owned());
    }
    Ok(patterns)
}

/// The engine files, relative to `source`, holding a lock, a condition variable or a thread-local of `std`'s or
/// parking_lot's: their tests, `sync/` itself and the models excepted
fn unmodelled_files(source: &Path) -> Result<std::collections::BTreeSet<String>, String> {
    let mut found = std::collections::BTreeSet::new();
    let mut pending = vec![source.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let relative = path.strip_prefix(source).expect("under source").to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                if relative != "sync" && !relative.ends_with("tests") {
                    pending.push(path);
                }
                continue;
            }
            if !relative.ends_with(".rs") || relative.ends_with("tests.rs") || relative.ends_with("model.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            if text.lines().any(locks_outside_sync) {
                found.insert(relative);
            }
        }
    }
    Ok(found)
}

/// Whether a line of code reaches a lock, a condition variable or a thread-local other than `crate::sync`'s
fn locks_outside_sync(line: &str) -> bool {
    let code = line.trim_start();
    if code.starts_with("//") {
        return false;
    }
    let std_lock = code.contains("std::sync::")
        && ["Mutex", "Condvar", "RwLock", "Barrier"].iter().any(|primitive| code.contains(primitive));
    let thread_local = code.contains("thread_local!") && !code.contains("crate::sync::thread_local!");
    std_lock || code.contains("parking_lot::") || thread_local
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask sits in the workspace root").to_path_buf()
}

fn generated_dir() -> PathBuf {
    workspace_root().join("crates/elmdb-node/protocol")
}

fn check_generated() -> Result<(), String> {
    let fresh = tempfile::tempdir().map_err(|e| e.to_string())?;
    generate::write(fresh.path())?;
    let stale = generate::differences(fresh.path(), &generated_dir())?;
    if stale.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "The generated TypeScript is stale; run `cargo xtask generate` and commit the result:\n  {}",
            stale.join("\n  ")
        ))
    }
}

/// Every LMDB `-sys` crate in the build, by name and version. Two copies of LMDB linked into one binary share no locks,
/// and nothing at link time refuses them (heed's `lmdb-master-sys` declares no `links` key), so this does.
fn check_one_lmdb(root: &Path) -> Result<(), String> {
    let tree = dependency_tree(root)?;
    let found = lmdb_sys_crates(&tree);
    match found.len() {
        1 => Ok(()),
        0 => Err("No LMDB -sys crate in the dependency tree; heed's lmdb-master-sys was expected".to_owned()),
        _ => Err(format!(
            "The dependency tree holds {} LMDB -sys crates, and a binary may link only one LMDB:\n  {}",
            found.len(),
            found.join("\n  ")
        )),
    }
}

/// The workspace's crates at their default features and what they build with, one `<name> v<version>` a line;
/// development dependencies are left out, being in no build a user gets
fn dependency_tree(root: &Path) -> Result<String, String> {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "--workspace", "--edges", "normal,build", "--prefix", "none", "--format", "{p}"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo tree didn't run: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo tree failed:\n{}", String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Diagnostics are off by default (`crates/elmdb/src/diagnostics.rs`): without the `diagnostics` feature `elmdb` does
/// not depend on `tracing`, so its spans compile to nothing. `tracing` is in the build regardless, through hannoy, whose
/// own spans are its own; what this holds is that no workspace crate turns elmdb's on by default
fn check_diagnostics_off(root: &Path) -> Result<(), String> {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "--workspace", "--edges", "normal,build", "--prefix", "depth", "--format", "{p}"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo tree didn't run: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo tree failed:\n{}", String::from_utf8_lossy(&output.stderr)));
    }
    let tree = String::from_utf8_lossy(&output.stdout);
    let direct = direct_dependencies(&tree, "elmdb");
    if direct.is_empty() {
        return Err(format!("cargo tree listed no dependency of elmdb, so this checked nothing:\n{tree}"));
    }
    match direct.iter().find(|name| name.as_str() == "tracing") {
        None => Ok(()),
        Some(_) => Err("A default build has elmdb depend on tracing, which only its `diagnostics` feature may: a \
                        workspace crate enables that feature by default"
            .to_owned()),
    }
}

fn check_leiden_serial(root: &Path) -> Result<(), String> {
    let output = Command::new(env!("CARGO"))
        .args(["tree", "--workspace", "--edges", "features", "--invert", "leiden-rs", "--depth", "0"])
        .args(["--prefix", "none", "--format", "{p}|{f}"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo tree didn't run: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo tree failed:\n{}", String::from_utf8_lossy(&output.stderr)));
    }
    let tree = String::from_utf8_lossy(&output.stdout);
    let Some(features) = tree.lines().find_map(|line| line.strip_prefix("leiden-rs ")) else {
        return Err(format!("cargo tree listed no leiden-rs, so this checked nothing:\n{tree}"));
    };
    match features.split_once('|').map(|(_, f)| f.trim()) {
        Some("") => Ok(()),
        enabled => Err(format!(
            "The workspace builds leiden-rs with features {enabled:?}; communities need it with none, its rayon \
             feature making a partition depend on thread timing"
        )),
    }
}

/// The names of `package`'s direct dependencies in a `cargo tree --prefix depth` listing of the workspace
fn direct_dependencies(tree: &str, package: &str) -> Vec<String> {
    let mut under = false;
    let mut found = Vec::new();
    for line in tree.lines() {
        if let Some(root) = line.strip_prefix('0') {
            under = root.split_whitespace().next() == Some(package);
        } else if under && let Some(child) = line.strip_prefix('1') {
            found.extend(child.split_whitespace().next().map(str::to_owned));
        }
    }
    found
}

/// The distinct `<name> v<version>` entries of a `cargo tree` listing whose crate is an LMDB `-sys` crate
fn lmdb_sys_crates(tree: &str) -> Vec<String> {
    let mut found: Vec<String> = tree
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next()?;
            let version = words.next()?;
            (name.contains("lmdb") && name.ends_with("-sys")).then(|| format!("{name} {version}"))
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

#[cfg(test)]
mod tests {
    use super::{UNMODELLED, direct_dependencies, lmdb_sys_crates, locks_outside_sync, unmodelled_files};

    #[test]
    fn a_lock_outside_crate_sync_is_found_and_one_through_it_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path();
        std::fs::create_dir_all(src.join("engine/tests")).unwrap();
        std::fs::create_dir_all(src.join("sync")).unwrap();
        std::fs::write(src.join("engine/locked.rs"), "use std::sync::{Arc, Mutex};\n").unwrap();
        std::fs::write(src.join("engine/parked.rs"), "fn f(_: parking_lot::Mutex<()>) {}\n").unwrap();
        std::fs::write(src.join("engine/local.rs"), "thread_local! { static X: u8 = 0; }\n").unwrap();
        std::fs::write(
            src.join("engine/modelled.rs"),
            "use crate::sync::{Mutex, PlainCondvar};\ncrate::sync::thread_local! {}\n",
        )
        .unwrap();
        std::fs::write(src.join("engine/commented.rs"), "// std::sync::Mutex is not used here\nuse std::sync::Arc;\n")
            .unwrap();
        std::fs::write(src.join("engine/tests/uses.rs"), "use std::sync::Mutex;\n").unwrap();
        std::fs::write(src.join("sync/mod.rs"), "pub(crate) use std::sync::{Condvar, Mutex};\n").unwrap();
        let found: Vec<String> = unmodelled_files(src).unwrap().into_iter().collect();
        assert_eq!(found, ["engine/local.rs", "engine/locked.rs", "engine/parked.rs"]);
        assert!(!locks_outside_sync("    // parking_lot::Mutex"));
    }

    #[test]
    fn a_listed_file_that_no_longer_locks_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("crates/elmdb/src")).unwrap();
        let refused = super::check_sync(dir.path()).unwrap_err();
        assert!(refused.contains("no longer lock") && refused.contains("store/sync.rs"), "{refused}");
    }

    #[test]
    fn every_unmodelled_file_says_why() {
        assert!(UNMODELLED.iter().all(|(file, why)| file.ends_with(".rs") && !why.is_empty()));
    }

    #[test]
    fn names_only_the_packages_own_direct_dependencies() {
        let tree = "0elmdb v0.0.0 (/x)\n1hannoy v0.3.0\n2tracing v0.1.44\n1heed v0.23.0\n0elmdb-node v0.0.0 (/y)\n1tracing v0.1.44\n";
        assert_eq!(direct_dependencies(tree, "elmdb"), ["hannoy", "heed"]);
        assert_eq!(direct_dependencies(tree, "elmdb-node"), ["tracing"]);
        assert!(direct_dependencies(tree, "elmdb-cli").is_empty());
    }

    #[test]
    fn finds_each_lmdb_sys_crate_once() {
        let tree = "elmdb v0.0.0 (/x)\nheed v0.23.0\nlmdb-master-sys v0.2.6\nlmdb-master-sys v0.2.6 (*)\nlibc v0.2.1\n";
        assert_eq!(lmdb_sys_crates(tree), ["lmdb-master-sys v0.2.6"]);
    }

    #[test]
    fn counts_two_versions_and_two_crates_apart() {
        let tree = "lmdb-master-sys v0.2.6\nlmdb-master-sys v0.2.5\nlmdb-master3-sys v0.2.6\nlmdb-rkv v0.14.0\n";
        assert_eq!(lmdb_sys_crates(tree).len(), 3);
    }
}
