"""Count each benchmarked operation's instructions on a base and on HEAD, and report the change without failing on it.

`python3 scripts/instruction-counts.py BASE`: `benches/instructions.rs` under Valgrind (`docs/architecture/testing.md`,
Instruction counts), on BASE checked out beside the tree and then on HEAD, compared with BASE's counts as a baseline.
A BASE without the benchmark is skipped, and HEAD's counts are reported alone. Exits nonzero only when a benchmark
fails to build, run or answer, a count of 0 being no answer; a count that moved is a row in the report, and in GitHub
Actions the step summary. `GUNGRAUN_HOME`, when set, names the directory the runs write, which CI keeps on a failure.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

BENCH = "crates/elmdb/benches/instructions.rs"
BASELINE = "base"

base = sys.argv[1]
root = Path.cwd()
# Both runs write one fresh directory, so HEAD's finds BASE's baseline and no earlier run's counts; and build into one
# target, so HEAD's build is incremental
home = Path(os.environ.get("GUNGRAUN_HOME") or tempfile.mkdtemp(prefix="instruction-counts-"))
home.mkdir(parents=True, exist_ok=True)
env = {**os.environ, "GUNGRAUN_HOME": str(home), "CARGO_TARGET_DIR": str(root / "target")}


def bench(cwd, *args):
    run = subprocess.run(["cargo", "bench", "-p", "elmdb", "--bench", "instructions", "--", *args], cwd=cwd, env=env)
    if run.returncode != 0:
        sys.exit(f"the instruction-count benchmark failed in {cwd} with exit code {run.returncode}")


compared = subprocess.run(["git", "cat-file", "-e", f"{base}:{BENCH}"]).returncode == 0
if compared:
    with tempfile.TemporaryDirectory() as scratch:
        tree = Path(scratch) / "base"
        subprocess.check_call(["git", "worktree", "add", "--detach", str(tree), base])
        try:
            bench(tree, f"--save-baseline={BASELINE}")
        finally:
            subprocess.check_call(["git", "worktree", "remove", "--force", str(tree)])
    bench(root, f"--baseline={BASELINE}", "--save-summary=json")
else:
    bench(root, "--save-summary=json")

rows = []
for summary in sorted(home.rglob("summary.json")):
    data = json.loads(summary.read_text())
    instructions = data["profiles"][0]["data"]["total"]["metrics"]["Ir"]
    # The case by what it was given, `(workload("page"))` read as `workload("page")`
    given = data["description"].removeprefix("(").removesuffix(")")
    name = f"{data['module_path'].removeprefix('instructions::')} {given}"
    rows.append((name, instructions["values"].get("old"), instructions["values"]["new"],
                 instructions.get("change", {}).get("diff_pct")))
assert rows, f"the benchmark wrote no summary under {home}"

lines = ["| Operation | Base | This change | Change |", "|---|---:|---:|---:|"]
for name, old, new, pct in rows:
    change = "" if pct is None else ("same" if float(pct) == 0 else f"{float(pct):+.3f}%")
    lines.append(f"| `{name}` | {'' if old is None else f'{old:,}'} | {new:,} | {change} |")
heading = "## Instruction counts\n\n" + (
    f"Against `{base}`, the commit this change merges into. Reported, never a failure.\n\n" if compared
    else f"`{base}` has no instruction-count benchmark, so these are this change's alone.\n\n")
report = heading + "\n".join(lines) + "\n"
print(report)
if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
    with open(summary, "a") as written:
        written.write(report)
# No operation the benchmark runs costs nothing: a count of 0 is a run Valgrind collected nothing from
if empty := [name for name, old, new, _ in rows if new == 0 or old == 0]:
    sys.exit(f"Valgrind counted 0 instructions for {', '.join(f'`{n}`' for n in empty)}: those runs measured nothing")
