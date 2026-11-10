//! `pre-push`: every check CI fails a change on that this machine can run, in one command, cheapest first, stopping at
//! the first that fails. What a push would have CI find is found here instead, the change's mutants included, which
//! nothing else runs before a push.
//!
//! What it can't run is CI's alone: the other platforms' builds and suites, and the Valgrind readings (instruction
//! counts, which only report, and the diagnostics overhead, which runs on Linux pull requests).

use std::path::Path;
use std::process::Command;
use std::time::Instant;

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
    step(
        "clippy with diagnostics",
        &[
            "cargo",
            "clippy",
            "-p",
            "elmdb",
            "-p",
            "elmdb-node",
            "--all-targets",
            "--features",
            "elmdb/diagnostics,elmdb-node/diagnostics",
            "--",
            "-D",
            "warnings",
        ],
    ),
];

/// After the repository checks: the suites, the heap bound and the benchmarks' smoke, the concurrency models with CI's
/// configuration and target directories, so a model run shares nothing with the suite's build, and the binding
const THEN: &[Step] = &[
    step("tests", &["cargo", "nextest", "run", "--workspace"]),
    step("tests with diagnostics", &["cargo", "nextest", "run", "-p", "elmdb", "--features", "diagnostics"]),
    step("doc tests", &["cargo", "test", "--workspace", "--doc"]),
    Step {
        // An exact filter that matches nothing passes, so the test is held to having run
        expect: Some("test maintenance_heap_bounds ... ok"),
        ..step(
            "heap bound",
            &[
                "cargo",
                "test",
                "-p",
                "elmdb",
                "--release",
                "--test",
                "maintenance_heap",
                "--",
                "--ignored",
                "--exact",
                "maintenance_heap_bounds",
                "--nocapture",
                "--color",
                "never",
            ],
        )
    },
    step("maintenance benchmark smoke", &["cargo", "bench", "-p", "elmdb", "--bench", "maintenance", "--", "--test"]),
    step("reads benchmark smoke", &["cargo", "bench", "-p", "elmdb", "--bench", "reads", "--", "--test"]),
    Step {
        env: &[("RUSTFLAGS", "--cfg loom"), ("LOOM_MAX_PREEMPTIONS", "3"), ("CARGO_TARGET_DIR", "target/loom")],
        ..step("Loom models", &["cargo", "test", "-p", "elmdb", "--release", "--lib", "store::unit_slot::model"])
    },
    Step {
        env: &[("RUSTFLAGS", "--cfg shuttle"), ("CARGO_TARGET_DIR", "target/shuttle")],
        ..step(
            "Shuttle models",
            &[
                "cargo",
                "test",
                "-p",
                "elmdb",
                "--release",
                "--lib",
                "--",
                "store::unit_slot::model",
                "engine::watch::model",
                "engine::builder::model",
            ],
        )
    },
    Step {
        stdout_to: Some("read-allocations.json"),
        ..step(
            "read allocations",
            &["cargo", "run", "-q", "-p", "elmdb", "--example", "read-allocations", "--", "--json"],
        )
    },
    step("binding build", &[NPM, "run", "build"]),
    step("binding typecheck", &[NPM, "run", "typecheck"]),
    step("binding tests", &[NPM, "test"]),
    step(
        "generated loader committed",
        &["git", "diff", "--exit-code", "--", "crates/elmdb-node/index.js", "crates/elmdb-node/index.d.ts"],
    ),
    Step {
        stdout_to: Some("read-codecs.json"),
        ..step("read codecs", &["node", "crates/elmdb-node/bench/reads.mjs", "--json", "--runs", "2"])
    },
    step("read profile's own tests", &[PYTHON, "-B", "scripts/test-read-profile.py"]),
    step(
        "read profile",
        &[
            PYTHON,
            "scripts/check-read-profile.py",
            "--allocations",
            "target/pre-push/read-allocations.json",
            "--codecs",
            "target/pre-push/read-codecs.json",
        ],
    ),
    Step {
        // Small enough to run in a second: that it still runs and the two engines answer alike, not a reading
        stdout_to: Some("against-sqlite.json"),
        ..step(
            "against SQLite, at 2,000 notes",
            &["node", "crates/elmdb-node/bench/against-sqlite.mjs", "--json", "--notes", "2000"],
        )
    },
];

/// Linux's own: the process claim as a user with no account, home or /proc, and its fallbacks' assertions
#[cfg(target_os = "linux")]
const LINUX: &[Step] = &[
    step("process claim without an account", &[PYTHON, "scripts/check-process-claim-linux.py"]),
    step("process publication mutations", &[PYTHON, "scripts/check-process-publication-mutations.py"]),
];
#[cfg(not(target_os = "linux"))]
const LINUX: &[Step] = &[];

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
    std::fs::create_dir_all(root.join(READINGS)).map_err(|e| format!("{READINGS}: {e}"))?;
    let started = Instant::now();
    for step in FIRST {
        run(root, step, &[])?;
    }
    for (name, check) in checks {
        println!("pre-push: {name}");
        check().map_err(|e| format!("pre-push: {name} failed: {e}"))?;
    }
    for step in THEN.iter().chain(LINUX) {
        run(root, step, &[])?;
    }
    run(root, &step("mutants of the change", &[PYTHON, "scripts/mutants-in-diff.py"]), &[base])?;
    println!("pre-push: everything passed in {:.0?}", started.elapsed());
    Ok(())
}
