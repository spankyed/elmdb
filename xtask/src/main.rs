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
//! - `mutant-exclusions`: writes `target/mutants.toml`, the configuration a mutation run reads with `--config`, and
//!   prints its path: `.cargo/mutants.toml` with an `exclude_re` for each `// mutant-skip:` comment in the source, which
//!   sits above the line it excuses and holds the reason. Nothing it writes is committed, so the comment is the one
//!   place a line's exclusion lives, and no merge has one to reconcile. It writes nothing from a `.cargo/mutants.toml`
//!   whose `exclude_re` names a line, so no run reads one.
//! - `check-mutant-exclusions`: fails when a `// mutant-skip:` comment, or an `exclude_re` of `.cargo/mutants.toml`,
//!   matches none of the mutants cargo-mutants lists, so the mutant it was written for is reported again; and when an
//!   `exclude_re` there names a line, which only a comment may.
//! - `compare`: runs one command on two revisions in turn and prints each reading beside the load as its run ended; it
//!   reports and decides nothing (`docs/architecture/testing.md`, Comparing two revisions).
//! - `sweep`: removes from the build directory what no recent build used, through `cargo-sweep` (`docs/architecture/
//!   testing.md`, The build directory).
//! - `pre-push`: every check CI runs on a change, the change's mutants included, cheapest first, stopping at the first
//!   that fails: what to run before a push.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod compare;
mod generate;
mod pre_push;

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
        Some("mutant-exclusions") => mutant_exclusions(&workspace_root()),
        Some("check-mutant-exclusions") => check_mutant_exclusions(&workspace_root()),
        Some("pre-push") => {
            let root = workspace_root();
            pre_push::pre_push(
                &root,
                &std::env::args().skip(2).collect::<Vec<_>>(),
                &[
                    ("generated protocol", &check_generated),
                    ("one LMDB", &|| check_one_lmdb(&root)),
                    ("diagnostics off by default", &|| check_diagnostics_off(&root)),
                    ("serial Leiden", &|| check_leiden_serial(&root)),
                    ("crate::sync", &|| check_sync(&root)),
                    ("mutant exclusions", &|| check_mutant_exclusions(&root)),
                ],
            )
        }
        _ => Err(
            "usage: cargo xtask <generate | check-generated | check-one-lmdb | check-diagnostics-off | check-leiden-serial | check-sync | mutant-exclusions | check-mutant-exclusions | compare | sweep | pre-push>"
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
    ("engine/order.rs", "a per-thread cache of each collation's collator and its stamp, no shared state"),
    ("engine/values.rs", "a per-thread decompression buffer, no shared state"),
    ("engine/watch.rs", "the schema read lock; the tracks and their delivery go through crate::sync"),
    ("failpoints.rs", "test-only failure points, armed per thread"),
    ("store/idle_reads.rs", "idle read snapshot release and its waiters: no model yet"),
    (
        "store/mod.rs",
        "the writer gate, a parking_lot RecursiveRwLock no model provides, and the paths open in the process",
    ),
    ("store/sync.rs", "the group commit sync worker, tested by its own deterministic harness (store/sync/tests.rs)"),
    (
        "store/writers.rs",
        "a writer's turn, a mutex across this process's threads, tested with real threads \
         (engine/tests/lifecycle.rs), and on macOS a file lock across processes, tested with real killed processes",
    ),
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

/// A `// mutant-skip: PATTERN` comment, ` (as above)` for a reason given by the one before it. Its reason follows it, as a comment
const MUTANT_SKIP: &str = "// mutant-skip: ";

/// The configuration cargo-mutants reads by default, which is committed and names no line
const CONFIG: &str = ".cargo/mutants.toml";
/// The one a run reads, relative to the workspace root: `CONFIG` with the comments' exclusions, never committed
const DERIVED_CONFIG: &str = "target/mutants.toml";

/// One `// mutant-skip:` comment: the file and line it is on, and the `exclude_re` it makes, which names the first
/// line after it that isn't a comment
#[derive(Debug, PartialEq)]
struct Skip {
    path: String,
    line: usize,
    pattern: String,
}

/// A file's `// mutant-skip:` comments, in line order
fn skips_in(path: &str, source: &str) -> Result<Vec<Skip>, String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut skips = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(pattern) = line.trim_start().strip_prefix(MUTANT_SKIP) else { continue };
        let pattern = pattern.strip_suffix(" (as above)").unwrap_or(pattern);
        let code = lines[at..]
            .iter()
            .position(|l| !l.trim_start().starts_with("//") && !l.trim().is_empty())
            .ok_or_else(|| format!("{path}:{}: a mutant-skip with no code after it", at + 1))?;
        skips.push(Skip {
            path: path.to_owned(),
            line: at + 1,
            pattern: format!("{path}:{}:.*{pattern}", at + code + 1),
        });
    }
    Ok(skips)
}

/// Every crate's `// mutant-skip:` comments, in file and line order
fn skips(root: &Path) -> Result<Vec<Skip>, String> {
    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
        for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_dir() {
                rust_files(&path, out)?;
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
        Ok(())
    }
    let crates = root.join("crates");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).map_err(|e| format!("{}: {e}", crates.display()))? {
        let source = entry.map_err(|e| e.to_string())?.path().join("src");
        if source.is_dir() {
            rust_files(&source, &mut files)?;
        }
    }
    files.sort();
    let mut found = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let path = file.strip_prefix(root).map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
        found.extend(skips_in(&path, &text)?);
    }
    Ok(found)
}

/// Where `exclude_re`'s list starts in the configuration's text, and where the line closing it does
fn exclude_re_list(text: &str) -> Result<(usize, usize), String> {
    let start = 1 + text.find("\nexclude_re = [").ok_or(format!("no exclude_re list in {CONFIG}"))?;
    let end = text[start..].find("\n]").ok_or("exclude_re's list doesn't close")?;
    Ok((start, start + end + 1))
}

/// Whether a committed `exclude_re` names a line, as `file.rs:12:` or `file.rs:(12|34):`: a line's number in a
/// committed file is rewritten by every edit above the line, and two branches that each make one conflict in it
fn names_a_line(pattern: &str) -> bool {
    pattern.match_indices(".rs:").any(|(at, found)| {
        pattern[at + found.len()..].trim_start_matches('(').starts_with(|c: char| c.is_ascii_digit())
    })
}

/// What a committed `exclude_re` that names a line is refused with
fn line_named(pattern: &str) -> String {
    format!(
        "{CONFIG}: the exclude_re {pattern:?} names a line, which every edit above that line would move; remove it, \
         and excuse the mutant with a `// mutant-skip:` comment above its line"
    )
}

/// The configuration a run reads: the committed one's text with each comment's exclusion at the end of `exclude_re`.
/// A committed pattern that names a line is refused here too, so a run never reads one: a list of them kept through
/// a merge would excuse whatever sits on its lines now
fn with_skips(text: &str, skips: &[Skip]) -> Result<String, String> {
    let (start, close) = exclude_re_list(text)?;
    if let Some(pattern) = exclude_res(text)?.iter().find(|pattern| names_a_line(pattern)) {
        return Err(line_named(pattern));
    }
    let last = text[start..close].lines().map(str::trim).rfind(|line| !line.is_empty() && !line.starts_with('#'));
    if last.is_some_and(|line| !line.ends_with(',')) {
        return Err(format!(
            "exclude_re's last pattern in {CONFIG} has no comma after it, which the patterns written after it need"
        ));
    }
    let mut derived = format!(
        "# Written by `cargo xtask mutant-exclusions` and never committed: {CONFIG}, and at the end of its\n\
         # exclude_re a pattern for each `// mutant-skip:` comment in the source. Edit those, not this.\n\n{}",
        &text[..close]
    );
    for skip in skips {
        derived += &format!("    \"{}\",\n", skip.pattern.replace('\\', "\\\\").replace('"', "\\\""));
    }
    Ok(derived + &text[close..])
}

fn mutant_exclusions(root: &Path) -> Result<(), String> {
    let config = root.join(CONFIG);
    let text = std::fs::read_to_string(&config).map_err(|e| format!("{}: {e}", config.display()))?;
    let derived = root.join(DERIVED_CONFIG);
    let written = |e: std::io::Error| format!("{}: {e}", derived.display());
    std::fs::create_dir_all(derived.parent().expect("a file in target/")).map_err(written)?;
    std::fs::write(&derived, with_skips(&text, &skips(root)?)?).map_err(written)?;
    // The path alone, for `--config "$(cargo xtask mutant-exclusions)"` and for `scripts/mutants-in-diff.py`
    println!("{}", derived.display());
    Ok(())
}

fn check_mutant_exclusions(root: &Path) -> Result<(), String> {
    let config = root.join(CONFIG);
    let text = std::fs::read_to_string(&config).map_err(|e| format!("{}: {e}", config.display()))?;
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
    let skips = skips(root)?;
    let stale = stale_exclusions(&exclude_res(&text)?, &skips, &mutants)?;
    if !stale.is_empty() {
        return Err(stale.join("\n"));
    }
    // What a run reads has to be writable from this text, which no run in this job would otherwise find out
    with_skips(&text, &skips).map(|_| ())
}

/// What is wrong with the exclusions, given every mutant cargo-mutants lists: a committed `exclude_re` that names a
/// line, or one that matches no mutant, and a `// mutant-skip:` comment whose pattern matches no mutant of the line
/// under it
fn stale_exclusions(committed: &[String], skips: &[Skip], mutants: &[&str]) -> Result<Vec<String>, String> {
    let matches_none = |pattern: &str| -> Result<bool, String> {
        let pattern = regex::Regex::new(pattern).map_err(|e| format!("exclusion {pattern:?}: {e}"))?;
        Ok(!mutants.iter().any(|mutant| pattern.is_match(mutant)))
    };
    let mut stale = Vec::new();
    for pattern in committed {
        if names_a_line(pattern) {
            stale.push(line_named(pattern));
        } else if matches_none(pattern)? {
            stale.push(format!(
                "{CONFIG}: the exclude_re {pattern:?} matches none of the {} mutants listed; name the mutant as \
                 `cargo mutants --list --no-config` does now, or remove it",
                mutants.len()
            ));
        }
    }
    for skip in skips {
        if matches_none(&skip.pattern)? {
            stale.push(format!(
                "{}:{}: this `// mutant-skip:` excuses {:?}, which matches none of the {} mutants listed: the line \
                 under it holds no such mutant; move the comment to directly above the line its mutant is on now, \
                 or remove it",
                skip.path,
                skip.line,
                skip.pattern,
                mutants.len()
            ));
        }
    }
    Ok(stale)
}

/// The patterns of `exclude_re`, each a TOML basic string on a line of its own as the file writes them; an empty list
/// is an error, since that is a file this no longer reads
fn exclude_res(text: &str) -> Result<Vec<String>, String> {
    let (start, close) = exclude_re_list(text)?;
    let mut patterns = Vec::new();
    for line in text[start..close].lines().skip(1).map(str::trim) {
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
        return Err(format!("exclude_re in {CONFIG} holds no pattern this could read"));
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
    use super::{Skip, exclude_res, skips_in, stale_exclusions, with_skips};
    use super::{UNMODELLED, direct_dependencies, lmdb_sys_crates, locks_outside_sync, unmodelled_files};

    fn skip(line: usize, pattern: &str) -> Skip {
        Skip { path: "crates/x/src/a.rs".to_owned(), line, pattern: pattern.to_owned() }
    }

    #[test]
    fn a_mutant_skip_names_the_line_of_the_code_after_it_and_its_reason_stays_in_the_source() {
        let source = "\
fn a() {
    // mutant-skip: replace > with >= in a
    // Its reason, over
    //
    // more than one line
    if x > 1 {}

    // mutant-skip: replace \\+ with \\* in a (as above)
    y + 1;
    // not a skip: mutant-skip: nothing
}
";
        assert_eq!(
            skips_in("crates/x/src/a.rs", source).unwrap(),
            [
                skip(2, "crates/x/src/a.rs:6:.*replace > with >= in a"),
                skip(8, "crates/x/src/a.rs:9:.*replace \\+ with \\* in a"),
            ]
        );
        assert!(skips_in("a.rs", "// mutant-skip: x\n// and nothing after\n").is_err());
    }

    #[test]
    fn the_configuration_a_run_reads_is_the_committed_one_with_each_comments_exclusion() {
        let committed = "exclude_globs = [\n    \"a/**\",\n]\nexclude_re = [\n    # why\n    \"kept\",\n]\nlast = 1\n";
        let derived =
            with_skips(committed, &[skip(2, "a.rs:6:.*replace \\+ with \\* in a"), skip(8, "a.rs:9:\"")]).unwrap();
        // The committed text whole, after a heading that says what wrote it, with the comments' patterns added
        let (heading, rest) = derived.split_once("\n\n").unwrap();
        assert!(heading.lines().all(|line| line.starts_with("# ")), "{heading}");
        assert_eq!(
            rest,
            "exclude_globs = [\n    \"a/**\",\n]\nexclude_re = [\n    # why\n    \"kept\",\n    \
             \"a.rs:6:.*replace \\\\+ with \\\\* in a\",\n    \"a.rs:9:\\\"\",\n]\nlast = 1\n"
        );
        // And they read back as the patterns they were, so cargo-mutants excludes what the comments name
        assert_eq!(exclude_res(&derived).unwrap(), ["kept", "a.rs:6:.*replace \\+ with \\* in a", "a.rs:9:\""]);
        assert_eq!(with_skips(committed, &[]).unwrap(), format!("{heading}\n\n{committed}"));
        assert!(with_skips("exclude_globs = []\n", &[]).is_err());
        // A last pattern without its comma would run into the first one added
        let refused = with_skips("a = 1\nexclude_re = [\n    \"kept\"\n    # why\n]\n", &[]).unwrap_err();
        assert!(refused.contains("no comma"), "{refused}");
        // A committed pattern that names a line makes no configuration, whatever the comments are
        for named in ["crates/x/src/a.rs:6:.*replace > with >= in a", "a.rs:(6|9):.*replace"] {
            let kept = format!("a = 1\nexclude_re = [\n    \"kept.rs:.*replace 1\",\n    \"{named}\",\n]\n");
            let refused = with_skips(&kept, &[skip(2, "a.rs:6:x")]).unwrap_err();
            assert!(refused.contains("names a line") && refused.contains(named), "{refused}");
        }
    }

    #[test]
    fn an_exclusion_that_names_no_mutant_is_stale_and_a_committed_one_names_no_line() {
        let mutants = [
            "crates/x/src/a.rs:6:10: replace > with >= in a",
            "crates/x/src/a.rs:9:7: replace + with * in a",
            "crates/x/src/b.rs:3:5: replace b -> bool with true",
        ];
        let committed = ["b.rs:.*replace b -> bool with".to_owned()];
        let skips = [skip(2, "crates/x/src/a.rs:6:.*replace > with >= in a")];
        assert_eq!(stale_exclusions(&committed, &skips, &mutants).unwrap(), [""; 0]);

        // The comment's mutant is on another line than the one under the comment, as after code went in between
        let moved = stale_exclusions(&committed, &[skip(2, "crates/x/src/a.rs:5:.*replace > with >= in a")], &mutants);
        let moved = moved.unwrap();
        assert_eq!(moved.len(), 1, "{moved:?}");
        assert!(moved[0].starts_with("crates/x/src/a.rs:2: ") && moved[0].contains("matches none"), "{moved:?}");
        // The line under the comment holds a mutant, of another kind than the comment names
        let other = stale_exclusions(&committed, &[skip(8, "crates/x/src/a.rs:9:.*replace > with >= in a")], &mutants);
        assert_eq!(other.unwrap().len(), 1);

        // A committed pattern whose mutant is gone
        let gone = stale_exclusions(&["b.rs:.*replace b -> u8 with".to_owned()], &skips, &mutants).unwrap();
        assert!(gone.len() == 1 && gone[0].contains("matches none"), "{gone:?}");
        // A committed pattern that names a line is refused though it matches, one line or several
        for pattern in ["crates/x/src/a.rs:6:.*replace > with >= in a", "a.rs:(6|9):.*replace"] {
            let named = stale_exclusions(&[pattern.to_owned()], &[], &mutants).unwrap();
            assert!(named.len() == 1 && named[0].contains("names a line"), "{named:?}");
        }
        assert!(stale_exclusions(&["(".to_owned()], &[], &mutants).is_err());
    }

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
