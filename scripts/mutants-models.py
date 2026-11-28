"""Mutate the code the Shuttle models hold, and test each mutant against those models, which no other run does.

`python3 scripts/mutants-models.py [MODEL ...] [--iterations N] [--jobs J] [--list]`: for each model named (all by
default), cargo-mutants over the modelled source scope, built with `--cfg shuttle` on `--profile model-mutants`
(optimized without LTO), each mutant tested by that model's `--lib` cases alone at `N` schedules
(`ELMDB_SHUTTLE_ITERATIONS`). A mutant that survives is tested once more at ten times the schedules, since Shuttle's
schedules are randomized. Exits nonzero when one survives both, and writes `target/mutants-models/report.md`, which
`.github/workflows/model-mutants.yml` files as an issue.

The per-pull-request gate (`scripts/mutants-in-diff.py`) tests mutants against the suite a plain build runs, which
holds none of the models. This script excludes lines compiled out under `--cfg shuttle` (`shuttle-linux`), reads
inline `// mutant-skip:` comments (not `.cargo/mutants.toml`'s), and checks each model's concurrency scope.

A timeout is caught: a mutant that hangs a model's schedule is one the model tells apart. A mutant whose test allocates
without end would take the machine, so on Linux each test runs under an 8 GiB virtual-memory cap.
"""
import argparse
import importlib.util
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib

# Load `compiled_out` from `scripts/mutants-in-diff.py` so compiled-out `#[cfg(not(any(loom, shuttle)))]`
# and `#[cfg(loom)]` blocks are never mutated under `--cfg shuttle`.
_MID_SPEC = importlib.util.spec_from_file_location("mutants_in_diff", Path(__file__).with_name("mutants-in-diff.py"))
assert _MID_SPEC and _MID_SPEC.loader
_mid = importlib.util.module_from_spec(_MID_SPEC)
_MID_SPEC.loader.exec_module(_mid)

# Each model: (source file, test package, nextest regex prefix, optional --re scope regex for modelled methods)
MODELS = {
    "locks": (
        "crates/elmdb-storage/src/locks.rs",
        "elmdb-storage",
        "locks::model",
        r"(tracker::|DomainToken|SchemaEpochLock|MapGate|WriterLock|DbiPublishGate|MetaLock::(new_in_scope|lock)|MetaRwLock)",
    ),
    "durability": (
        "crates/elmdb-storage/src/durability.rs",
        "elmdb-sim",
        "durability_model",
        r"DurabilityGate<S, V>::(begin_read|begin_write\b|sync\b|acknowledge_restore|grow_map|adopt_and_pin_grouped|check_poisoned|poison\b|persist_durable_locked)",
    ),
    "lifecycle": (
        "crates/elmdb/src/lifecycle.rs",
        "elmdb",
        "lifecycle::model",
        None,
    ),
    "invalidation": (
        "crates/elmdb-invalidation/src/lib.rs",
        "elmdb-invalidation",
        "model",
        r"LiveQueryHub::(register_query_watch\b|rerun_query_watch\b|unregister_watch|notify_commit|notify_schema_change_with_maint|stop_watches)",
    ),
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


def compiled_out_excludes(path):
    """Regexes excluding lines compiled out under `--cfg shuttle` on Linux."""
    lines = sorted(_mid.compiled_out(root, path, "shuttle-linux"))
    if not lines:
        return []
    line_alts = "|".join(str(n) for n in lines)
    return [f"^{re.escape(path)}:({line_alts}):"]


def run(name, path, package, test_filter, scope_re, iterations, only=None):
    """cargo-mutants over `path`, `only` the mutants named if given: the names of those that survived"""
    output = out / name
    command = [
        "cargo", "mutants", "--no-config", "--file", path, "--test-package", package, "--test-tool", "nextest",
        "--profile", "model-mutants", "--timeout-multiplier", "3", "--jobs", args.jobs, "--output", str(output),
    ]
    for pattern in skips + compiled_out_excludes(path):
        command += ["--exclude-re", pattern]
    if only is not None:
        for mutant in only:
            command += ["--re", f"^{re.escape(mutant)}$"]
    elif scope_re:
        command += ["--re", scope_re]
    if args.list:
        command.append("--list")
    command += ["--", "--lib", "-E", f"test(/^{re.escape(test_filter)}::/)"]
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
    path, package, test_filter, scope_re = MODELS[name]
    missed = run(name, path, package, test_filter, scope_re, args.iterations)
    if args.list:
        continue
    # Each again, alone, at ten times the schedules: one that survives that is reported. A name in `missed.txt` is
    # the whole of what `--re` matches: its file, line and column, and what it replaces
    again = run(f"{name}-again", path, package, test_filter, scope_re, args.iterations * 10, only=missed) if missed else []
    survived += len(again)
    report.append(f"## `{name}`: `{path}`, held by `{package}` (`{test_filter}`)")
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
