//! `pre-push`: every check CI fails a change on that this machine can run, in one command, cheapest first, stopping at
//! the first that fails. What a push would have CI find is found here instead, the change's mutants included, which
//! nothing else runs before a push.
//!
//! What it can't run is CI's alone: the other platforms' builds and suites, and the Valgrind readings (instruction
//! counts, which only report, and the diagnostics overhead, which runs on Linux pull requests).

use std::path::Path;
use std::process::Command;
use std::time::{Instant, SystemTime};

/// npm and Python by the names this platform runs them under
#[cfg(windows)]
const NPM: &str = "npm.cmd";
#[cfg(not(windows))]
const NPM: &str = "npm";
#[cfg(windows)]
const PYTHON: &str = "python";
#[cfg(not(windows))]
const PYTHON: &str = "python3";

/// Where the readings a step writes for a later one go
const READINGS: &str = "target/pre-push";

/// One check: what it is called, the command it runs from the workspace root with what it adds to the environment,
/// where its standard output goes when a later step reads it, and what that output must hold for the check to count
struct Step {
    name: &'static str,
    command: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
    stdout_to: Option<&'static str>,
    expect: Option<&'static str>,
}

const fn step(name: &'static str, command: &'static [&'static str]) -> Step {
    Step { name, command, env: &[], stdout_to: None, expect: None }
}

/// As CI runs them (`.github/workflows/ci.yml`): the checks job, then the repository's
const FIRST: &[Step] = &[
    step("locked dependencies", &["cargo", "fetch", "--locked"]),
    step("format", &["cargo", "fmt", "--all", "--check"]),
    step("clippy", &["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"]),
    step("mutants-in-diff's own tests", &[PYTHON, "-B", "scripts/test-mutants-in-diff.py"]),
];

/// After the repository checks: workspace test suites, doc tests, and concurrency models
const THEN: &[Step] = &[
    step("tests", &["cargo", "nextest", "run", "--workspace"]),
    step("doc tests", &["cargo", "test", "--workspace", "--doc"]),
    Step {
        name: "concurrency models (Loom: locks)",
        command: &["cargo", "test", "-p", "elmdb-storage", "--release", "--lib", "locks::model"],
        env: &[("RUSTFLAGS", "--cfg loom"), ("LOOM_MAX_PREEMPTIONS", "3"), ("CARGO_TARGET_DIR", "target/loom")],
        stdout_to: None,
        expect: None,
    },
    Step {
        name: "concurrency models (Loom: lifecycle)",
        command: &["cargo", "test", "-p", "elmdb", "--release", "--lib", "lifecycle::model"],
        env: &[("RUSTFLAGS", "--cfg loom"), ("LOOM_MAX_PREEMPTIONS", "3"), ("CARGO_TARGET_DIR", "target/loom")],
        stdout_to: None,
        expect: None,
    },
    Step {
        name: "concurrency models (Shuttle: locks)",
        command: &["cargo", "test", "-p", "elmdb-storage", "--release", "--lib", "locks::model"],
        env: &[("RUSTFLAGS", "--cfg shuttle"), ("CARGO_TARGET_DIR", "target/shuttle")],
        stdout_to: None,
        expect: None,
    },
    Step {
        name: "concurrency models (Shuttle: durability)",
        command: &["cargo", "test", "-p", "elmdb-sim", "--release", "--lib", "durability_model"],
        env: &[("RUSTFLAGS", "--cfg shuttle"), ("CARGO_TARGET_DIR", "target/shuttle")],
        stdout_to: None,
        expect: None,
    },
    Step {
        name: "concurrency models (Shuttle: lifecycle)",
        command: &["cargo", "test", "-p", "elmdb", "--release", "--lib", "lifecycle::model"],
        env: &[("RUSTFLAGS", "--cfg shuttle"), ("CARGO_TARGET_DIR", "target/shuttle")],
        stdout_to: None,
        expect: None,
    },
    Step {
        name: "concurrency models (Shuttle: invalidation)",
        command: &["cargo", "test", "-p", "elmdb-invalidation", "--release", "--lib", "model"],
        env: &[("RUSTFLAGS", "--cfg shuttle"), ("CARGO_TARGET_DIR", "target/shuttle")],
        stdout_to: None,
        expect: None,
    },
];

/// The binding and EARS checks as CI runs them
const NPM_STEPS: &[Step] = &[
    step("build: the binding, then EARS' dist", &[NPM, "run", "build"]),
    step("typecheck: the binding and EARS", &[NPM, "run", "typecheck"]),
    step("tests: the binding's, then EARS'", &[NPM, "test"]),
    step("EARS' API reports", &[NPM, "run", "api:check", "-w", "@elmdb/ears"]),
    step(
        "generated loader committed",
        &["git", "diff", "--exit-code", "--", "crates/elmdb-node/index.js", "crates/elmdb-node/index.d.ts"],
    ),
];

/// Linux's own: the process claim as a user with no account, home or /proc, and its fallbacks' assertions
#[cfg(target_os = "linux")]
const LINUX: &[Step] = &[
    step("process claim without an account", &[PYTHON, "scripts/check-process-claim-linux.py"]),
    step("process publication mutations", &[PYTHON, "scripts/check-process-publication-mutations.py"]),
];
#[cfg(not(target_os = "linux"))]
const LINUX: &[Step] = &[];

/// Whether `npm ci` is owed before the npm steps: nothing is installed, or the lockfile changed after npm last installed
/// from it, so the steps would test the dependencies it named before. `installed` is when npm last wrote
/// `node_modules/.package-lock.json`, which it does at every install
fn install_owed(lockfile: Option<SystemTime>, installed: Option<SystemTime>) -> bool {
    match (lockfile, installed) {
        (_, None) => true,
        (Some(lockfile), Some(installed)) => lockfile > installed,
        (None, Some(_)) => false,
    }
}

/// A repository check this task runs in-process, by the name it prints
pub type Check<'a> = (&'a str, &'a dyn Fn() -> Result<(), String>);

fn run(root: &Path, step: &Step, extra: &[String]) -> Result<(), String> {
    let (program, args) = step.command.split_first().expect("a step runs something");
    let started = Instant::now();
    println!("pre-push: {}", step.name);
    let mut command = Command::new(program);
    command.args(args.iter().copied()).args(extra).envs(step.env.iter().copied()).current_dir(root);
    let failed = |detail: String| format!("pre-push: {} failed ({detail}): the push would fail CI", step.name);
    if step.stdout_to.is_some() || step.expect.is_some() {
        let output = command
            .stderr(std::process::Stdio::inherit())
            .output()
            .map_err(|e| format!("pre-push: {} didn't start: {e}", step.name))?;
        let text = String::from_utf8_lossy(&output.stdout);
        if let Some(file) = step.stdout_to {
            std::fs::write(root.join(READINGS).join(file), &output.stdout)
                .map_err(|e| format!("pre-push: {} wrote nothing: {e}", step.name))?;
        } else {
            print!("{text}");
        }
        if !output.status.success() {
            return Err(failed(output.status.to_string()));
        }
        if let Some(expected) = step.expect
            && !text.contains(expected)
        {
            return Err(failed(format!("its output doesn't say {expected:?}")));
        }
    } else {
        let status = command.status().map_err(|e| format!("pre-push: {} didn't start: {e}", step.name))?;
        if !status.success() {
            return Err(failed(status.to_string()));
        }
    }
    println!("pre-push: {} passed in {:.0?}", step.name, started.elapsed());
    Ok(())
}

/// The tracked files with changes not committed: what is checked has to be what is pushed, and the change's mutants are
/// read from what is committed alone
fn uncommitted(root: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .current_dir(root)
        .output()
        .map_err(|e| format!("git status: {e}"))?;
    if !output.status.success() {
        return Err(format!("git status failed: {}", String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().map(str::to_owned).collect())
}

/// `cargo xtask pre-push [--base REF]`: REF is what the change merges into, origin/main unless given, against which its
/// mutants are read (`scripts/mutants-in-diff.py`)
pub fn pre_push(root: &Path, args: &[String], checks: &[Check<'_>]) -> Result<(), String> {
    let base = match args {
        [] => "origin/main".to_owned(),
        [flag, base] if flag == "--base" => base.clone(),
        _ => return Err("usage: cargo xtask pre-push [--base REF]".to_owned()),
    };
    let dirty = uncommitted(root)?;
    if !dirty.is_empty() {
        return Err(format!("pre-push checks what is committed; commit or set aside first:\n{}", dirty.join("\n")));
    }
    let started = Instant::now();
    // Before anything builds: every build directory bounded, and the run refused where what is left free would fill
    // part way through it
    println!("pre-push: build directories");
    crate::build_dir::sweep(root).map_err(|e| format!("pre-push: build directories failed: {e}"))?;
    let target = crate::build_dir::target_dir(root)?;
    // The target's filesystem, which a shared target may put on another than the checkout's; the checkout's before a
    // first build has made it
    let free = crate::build_dir::free_bytes(&target).or_else(|| crate::build_dir::free_bytes(root));
    crate::build_dir::refuse_below(&target, free, crate::build_dir::FLOOR).map_err(|e| format!("pre-push: {e}"))?;
    std::fs::create_dir_all(root.join(READINGS)).map_err(|e| format!("{READINGS}: {e}"))?;
    for step in FIRST {
        run(root, step, &[])?;
    }
    for (name, check) in checks {
        println!("pre-push: {name}");
        check().map_err(|e| format!("pre-push: {name} failed: {e}"))?;
    }
    for step in THEN {
        run(root, step, &[])?;
    }
    let modified = |file: &str| std::fs::metadata(root.join(file)).and_then(|m| m.modified()).ok();
    if install_owed(modified("package-lock.json"), modified("node_modules/.package-lock.json")) {
        run(root, &step("npm ci, the lockfile having moved since the last install", &[NPM, "ci"]), &[])?;
    }
    for step in NPM_STEPS.iter().chain(LINUX) {
        run(root, step, &[])?;
    }
    run(root, &step("mutants of the change", &[PYTHON, "scripts/mutants-in-diff.py"]), &[base])?;
    println!("pre-push: everything passed in {:.0?}", started.elapsed());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::install_owed;
    use std::time::{Duration, SystemTime};

    #[test]
    fn npm_ci_is_owed_where_nothing_is_installed_or_the_lockfile_is_newer() {
        let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let later = then + Duration::from_secs(1);
        assert!(install_owed(Some(then), None), "nothing installed");
        assert!(install_owed(None, None), "nothing installed, and no lockfile to install from");
        assert!(install_owed(Some(later), Some(then)), "the lockfile moved after the install");
        assert!(!install_owed(Some(then), Some(later)), "installed from the lockfile as it is");
        assert!(!install_owed(Some(then), Some(then)), "installed in the same instant");
        assert!(!install_owed(None, Some(then)), "no lockfile to be newer");
    }
}
