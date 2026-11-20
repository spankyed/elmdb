"""Mutate the code the Shuttle models hold, and test each mutant against those models, which no other run does.

`python3 scripts/mutants-models.py [MODEL ...] [--iterations N] [--jobs J] [--list]`: for each model named (all by default),
cargo-mutants over the one file the model holds, built with `--cfg shuttle` in release, each mutant tested by that
model's cases alone, at `N` schedules (`ELMDB_SHUTTLE_ITERATIONS`). A mutant that survives is tested once more at ten
times the schedules, since Shuttle's are random and a weak mutant can pass one run by chance. Exits nonzero when one
survives both, and writes `target/mutants-models/report.md`, which the weekly workflow (`.github/workflows/
model-mutants.yml`) files as an issue.

The per-pull-request gate (`scripts/mutants-in-diff.py`) tests mutants against the suite a plain build runs, which
holds none of the models: so `.cargo/mutants.toml` excludes from it what only a model tells apart, saying so. This run
is what checks those exclusions. It reads the source's `// mutant-skip:` comments, each naming a mutant no input tells
from the code, and none of `.cargo/mutants.toml`'s: a mutant that file excludes because "the model holds it" is tested
here, and reported where the model doesn't.

A timeout is caught: a mutant that hangs a model's schedule is one the model tells apart. A mutant whose test allocates
without end would take the machine, so on Linux each test runs under a limit of its address space.
"""
import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib

# Each model, the one file it holds, and the filter that runs its cases alone
MODELS = {
    "builder": ("crates/elmdb/src/engine/builder.rs", "engine::builder::model"),
    "call_reads": ("crates/elmdb/src/store/call_reads.rs", "store::call_reads::model"),
    "unit_slot": ("crates/elmdb/src/store/unit_slot.rs", "store::unit_slot::model"),
    "watch": ("crates/elmdb/src/engine/watch.rs", "engine::watch::model"),
}

parser = argparse.ArgumentParser()
parser.add_argument("models", nargs="*", help=f"of {', '.join(MODELS)}; all by default")
parser.add_argument("--iterations", type=int, default=200_000, help="Shuttle's schedules for each case")
parser.add_argument("--jobs", default="2")
parser.add_argument("--list", action="store_true", help="list each model's mutants, and test none")
args = parser.parse_args()
unknown = [name for name in args.models if name not in MODELS]
if unknown:
    sys.exit(f"no model is named {', '.join(unknown)}: the models are {', '.join(MODELS)}")
models = args.models or list(MODELS)

root = Path.cwd()
out = root / "target/mutants-models"
shutil.rmtree(out, ignore_errors=True)
out.mkdir(parents=True)

# The source's skips: what `cargo xtask mutant-exclusions` writes, less what `.cargo/mutants.toml` commits
written = Path(subprocess.check_output(["cargo", "xtask", "mutant-exclusions"], text=True).strip())
committed = set(tomllib.loads((root / ".cargo/mutants.toml").read_text())["exclude_re"])
skips = [pattern for pattern in tomllib.loads(written.read_text())["exclude_re"] if pattern not in committed]

env = {key: value for key, value in os.environ.items() if key != "CARGO_TARGET_DIR"}
env["RUSTFLAGS"] = (env.get("RUSTFLAGS", "") + " --cfg shuttle").strip()
if sys.platform == "linux":
    # Each test under a limit of its address space, 8 GiB: a model needs a fraction of that, and a mutant that
    # allocates in a loop is ended rather than ending the runner
    guard = out / "guard.sh"
    guard.write_text('#!/bin/sh\nulimit -v 8388608\nexec "$@"\n')
    guard.chmod(0o755)
    env["CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER"] = str(guard)


def run(name, path, filter, iterations, only=None):
    """cargo-mutants over `path`, `only` the mutants named if given: the names of those that survived"""
    output = out / name
    command = [
        "cargo", "mutants", "--no-config", "--file", path, "--test-package", "elmdb", "--test-tool", "nextest",
        "--profile", "release", "--timeout-multiplier", "3", "--jobs", args.jobs, "--output", str(output),
    ]
    for pattern in skips:
        command += ["--exclude-re", pattern]
    for mutant in only or []:
        command += ["--re", f"^{re.escape(mutant)}$"]
    if args.list:
        command.append("--list")
    command += ["--", "-E", f"test(/^{re.escape(filter)}::/)"]
    run_env = {**env, "ELMDB_SHUTTLE_ITERATIONS": str(iterations)}
    finished = subprocess.run(command, env=run_env)
    # 0: all caught; 2: a mutant missed; 3: a timeout, which is caught here
    if finished.returncode not in (0, 2, 3):
        sys.exit(f"cargo-mutants failed on {path} with exit code {finished.returncode}: its log is in {output}")
    missed = output / "mutants.out/missed.txt"
    return [line.strip() for line in missed.read_text().splitlines() if line.strip()] if missed.exists() else []


report = ["# Mutants the Shuttle models don't catch", ""]
survived = 0
for name in models:
    path, filter = MODELS[name]
    missed = run(name, path, filter, args.iterations)
    if args.list:
        continue
    # Each again, alone, at ten times the schedules: one that survives that is reported. A name in `missed.txt` is
    # the whole of what `--re` matches: its file, line and column, and what it replaces
    again = run(f"{name}-again", path, filter, args.iterations * 10, only=missed) if missed else []
    survived += len(again)
    report.append(f"## `{name}`: `{path}`, held by `{filter}`")
    report.append("")
    if again:
        report += [f"- `{mutant}`" for mutant in again]
        report += ["", "Each needs a model case that fails on it, or a `// mutant-skip:` saying why no input tells it "
                   "from the code."]
    else:
        report.append("Every mutant caught.")
    report.append("")

(out / "report.md").write_text("\n".join(report))
print("\n".join(report))
sys.exit(1 if survived else 0)
