//! The repository's own tasks, run as `cargo xtask <task>`:
//!
//! - `generate`: writes the TypeScript declarations of the protocol's types into `crates/elmdb-node/protocol/`.
//! - `check-generated`: fails when what `generate` would write differs from what is committed.
//! - `check-one-lmdb`: fails unless the dependency tree holds exactly one LMDB `-sys` crate (one version of one).
//! - `check-diagnostics-off`: fails when a default build of the workspace has `elmdb` depend on `tracing` itself,
//!   which only its `diagnostics` feature may make it do.
//! - `check-leiden-serial`: fails when the workspace builds `leiden-rs` with any feature, `rayon` among them, whose
//!   parallel moves would make a partition depend on the threads' timing rather than on its seed alone.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod generate;

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("generate") => generate::write(&generated_dir()),
        Some("check-generated") => check_generated(),
        Some("check-one-lmdb") => check_one_lmdb(&workspace_root()),
        Some("check-diagnostics-off") => check_diagnostics_off(&workspace_root()),
        Some("check-leiden-serial") => check_leiden_serial(&workspace_root()),
        _ => Err(
            "usage: cargo xtask <generate | check-generated | check-one-lmdb | check-diagnostics-off | check-leiden-serial>"
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
    use super::{direct_dependencies, lmdb_sys_crates};

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
