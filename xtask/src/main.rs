//! The repository's own tasks, run as `cargo xtask <task>`:
//!
//! - `generate`: writes the TypeScript declarations of the protocol's types into `crates/elmdb-node/protocol/`.
//! - `check-generated`: fails when what `generate` would write differs from what is committed.
//! - `check-one-lmdb`: fails unless the dependency tree holds exactly one LMDB `-sys` crate (one version of one).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod generate;

fn main() -> ExitCode {
    let task = std::env::args().nth(1);
    let result = match task.as_deref() {
        Some("generate") => generate::write(&generated_dir()),
        Some("check-generated") => check_generated(),
        Some("check-one-lmdb") => check_one_lmdb(&workspace_root()),
        _ => Err("usage: cargo xtask <generate | check-generated | check-one-lmdb>".to_owned()),
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
    let output = Command::new(env!("CARGO"))
        .args(["tree", "--workspace", "--edges", "normal,build", "--prefix", "none", "--format", "{p}"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("cargo tree didn't run: {e}"))?;
    if !output.status.success() {
        return Err(format!("cargo tree failed:\n{}", String::from_utf8_lossy(&output.stderr)));
    }
    let tree = String::from_utf8_lossy(&output.stdout);
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
    use super::lmdb_sys_crates;

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
