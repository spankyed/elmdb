//! `pre-push`: every check CI runs on a change, in one command, cheapest first, stopping at the first that fails. What a
//! push would have CI find is found here instead, the change's mutants included, which nothing else runs before a push.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

/// One check: what it is called, and the command it runs from the workspace root, with what it adds to the environment
struct Step {
    name: &'static str,
    command: &'static [&'static str],
    env: &'static [(&'static str, &'static str)],
}

/// The commands, as CI runs them (`.github/workflows/ci.yml`, `mutants.yml`): the concurrency models with the same
/// configuration and target directories, so a model run here shares nothing with the suite's build
const BEFORE_CHECKS: &[Step] = &[
    Step { name: "format", command: &["cargo", "fmt", "--all", "--check"], env: &[] },
    Step {
        name: "clippy",
        command: &["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
        env: &[],
    },
];

const AFTER_CHECKS: &[Step] = &[
    Step { name: "tests", command: &["cargo", "nextest", "run", "--workspace"], env: &[] },
    Step { name: "doc tests", command: &["cargo", "test", "--workspace", "--doc"], env: &[] },
    Step {
        name: "Loom models",
        command: &["cargo", "test", "-p", "elmdb", "--release", "--lib", "store::unit_slot::model"],
        env: &[("RUSTFLAGS", "--cfg loom"), ("LOOM_MAX_PREEMPTIONS", "3"), ("CARGO_TARGET_DIR", "target/loom")],
    },
    Step {
        name: "Shuttle models",
        command: &[
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
        env: &[("RUSTFLAGS", "--cfg shuttle"), ("CARGO_TARGET_DIR", "target/shuttle")],
    },
    Step { name: "binding build", command: &["npm", "run", "build"], env: &[] },
    Step { name: "binding typecheck", command: &["npm", "run", "typecheck"], env: &[] },
    Step { name: "binding tests", command: &["npm", "test"], env: &[] },
];

/// A repository check this task runs in-process, by the name it prints
pub type Check<'a> = (&'a str, &'a dyn Fn() -> Result<(), String>);

fn run(root: &Path, step: &Step, extra: &[String]) -> Result<(), String> {
    let (program, args) = step.command.split_first().expect("a step runs something");
    let started = Instant::now();
    println!("pre-push: {}", step.name);
    let status = Command::new(program)
        .args(args.iter().copied())
        .args(extra)
        .envs(step.env.iter().copied())
        .current_dir(root)
        .status()
        .map_err(|e| format!("pre-push: {} didn't start: {e}", step.name))?;
    if !status.success() {
        return Err(format!("pre-push: {} failed ({status}): the push would fail CI", step.name));
    }
    println!("pre-push: {} passed in {:.0?}", step.name, started.elapsed());
    Ok(())
}

/// `cargo xtask pre-push [--base REF]`: REF is what the change merges into, origin/main unless given, against which its
/// mutants are read (`scripts/mutants-in-diff.py`, which reads only what is committed)
pub fn pre_push(root: &Path, args: &[String], checks: &[Check<'_>]) -> Result<(), String> {
    let base = match args {
        [] => "origin/main".to_owned(),
        [flag, base] if flag == "--base" => base.clone(),
        _ => return Err("usage: cargo xtask pre-push [--base REF]".to_owned()),
    };
    let started = Instant::now();
    for step in BEFORE_CHECKS {
        run(root, step, &[])?;
    }
    for (name, check) in checks {
        println!("pre-push: {name}");
        check().map_err(|e| format!("pre-push: {name} failed: {e}"))?;
    }
    for step in AFTER_CHECKS {
        run(root, step, &[])?;
    }
    let mutants = Step { name: "mutants of the change", command: &["python3", "scripts/mutants-in-diff.py"], env: &[] };
    run(root, &mutants, &[base])?;
    println!("pre-push: everything passed in {:.0?}", started.elapsed());
    Ok(())
}
