"""Fail when the engine's diagnostics, compiled in with nothing collecting them, cost more than a declared bound.

`python3 scripts/diagnostics-overhead.py`: `benches/instructions.rs` under Valgrind (`docs/architecture/testing.md`,
Diagnostics), twice on this commit: built without the `diagnostics` feature, the default, and then with it and no
collector, compared with the first. An instruction count doesn't move with the machine's load, so the comparison can be
a gate. Exits nonzero when a gated case grew past its bound or spends anything more on allocating, when a case the
benchmark runs is neither gated nor named as reported only, or when a gated case is missing or shows no allocator, so it
can't pass having compared nothing. A failure names the functions that grew.

The bound is declared: with no collector a span costs one cached check of its callsite, an operation's spans a couple of hundred
instructions, while what the gate is for — work per row, such as an event or a clock read — grows with the rows. One
uncollected event per sorted entry costs a 1,000-entity rebuild about 0.1%, twice the bound (`docs/evidence/
diagnostics.md`, where the firing cases are).
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from callgrind import allocator_instructions, grew_most
from instruction_gates import FLOOR, GATED, SHARE, allowed_growth, undeclared_or_missing

BASELINE = "off"

root = Path.cwd()
# `GUNGRAUN_HOME`, when set, names the directory the runs write, which CI keeps on a failure
home = Path(os.environ.get("GUNGRAUN_HOME") or tempfile.mkdtemp(prefix="diagnostics-overhead-"))
home.mkdir(parents=True, exist_ok=True)
env = {**os.environ, "GUNGRAUN_HOME": str(home), "CARGO_TARGET_DIR": str(root / "target")}


def bench(*args):
    run = subprocess.run(["cargo", "bench", "-p", "elmdb", "--bench", "instructions", *args], env=env)
    if run.returncode != 0:
        sys.exit(f"the instruction-count benchmark failed with exit code {run.returncode}")


bench("--", f"--save-baseline={BASELINE}")
bench("--features", "diagnostics", "--", f"--baseline={BASELINE}", "--save-summary=json")

rows = []
for summary in sorted(home.rglob("summary.json")):
    data = json.loads(summary.read_text())
    instructions = data["profiles"][0]["data"]["total"]["metrics"]["Ir"]
    function = data["module_path"].rsplit("::", 1)[-1]
    given = data["description"].removeprefix("(").removesuffix(")")
    rows.append((function, given, instructions["values"].get("old"), instructions["values"]["new"], summary.parent))

seen = {row[0] for row in rows}
failures = undeclared_or_missing(seen)
lines = ["| Operation | Without | With, nothing collecting | Change | Bound |", "|---|---:|---:|---:|---|"]
for function, given, old, new, directory in rows:
    if old is None:
        failures.append(f"`{function} {given}` has no count without the feature to compare with")
        continue
    # No operation the benchmark runs costs nothing: a count of 0 is a measurement that collected nothing
    if old == 0 or new == 0:
        failures.append(f"`{function} {given}` counted {old:,} instructions without the feature and {new:,} with it: "
                        "Valgrind collected nothing in a run, so there is nothing to compare")
        continue
    allowed = allowed_growth(old)
    grew = new - old
    gated = function in GATED
    if gated and grew > allowed:
        failures.append(f"`{function} {given}` grew {grew:,} instructions, past its bound of {allowed:,}. "
                        f"What grew: {grew_most(directory, BASELINE)}")
    # Nothing collecting adds no allocation, and an allocation costs the same everywhere, so this one has no bound
    allocating = allocator_instructions(directory, f"base@{BASELINE}"), allocator_instructions(directory)
    if gated and 0 in allocating:
        failures.append(f"`{function} {given}` shows no allocator ({allocating[0]:,} and {allocating[1]:,} instructions): "
                        "scripts/callgrind.py finds it by the name benches/instructions.rs gives it")
    elif gated and allocating[1] != allocating[0]:
        failures.append(f"`{function} {given}` spends {allocating[1]:,} instructions allocating with the feature and "
                        f"{allocating[0]:,} without. What grew: {grew_most(directory, BASELINE)}")
    bound = f"+{allowed:,}" if gated else "reported only"
    lines.append(f"| `{function} {given}` | {old:,} | {new:,} | {grew:+,} ({grew / old:+.4%}) | {bound} |")
report = ("## Diagnostics overhead\n\nOne commit's instruction counts without the `diagnostics` feature and with it, "
          f"nothing collecting. A gated case may grow {SHARE:.2%} or {FLOOR} instructions, whichever is more, and spend nothing "
          "more on allocating.\n\n"
          + "\n".join(lines) + "\n")
if failures:
    report += "\n" + "\n".join(f"- **Failed:** {failure}" for failure in failures) + "\n"
print(report)
if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
    with open(summary, "a") as written:
        written.write(report)
sys.exit(1 if failures else 0)
