"""Fail when the engine's diagnostics, compiled in with nothing collecting them, cost more than a declared bound.

`python3 scripts/diagnostics-overhead.py`: `benches/instructions.rs` under Valgrind (`docs/architecture/testing.md`,
Diagnostics), twice on this commit: built without the `diagnostics` feature, the default, and then with it and no
collector, compared with the first. An instruction count doesn't move with the machine's load, so the comparison can be
a gate. Exits nonzero when a gated case grew past its bound, when a case the benchmark runs is neither gated nor named
as reported only, or when a gated case is missing, so it can't pass having compared nothing.

The bound is declared: with no collector a span costs one cached check of its callsite, a few dozen instructions an
operation, while what the gate is for — work per row, such as an event or a clock read — grows with the rows. One
uncollected event per sorted entry costs a 1,000-entity rebuild about 0.1%, twice the bound (`docs/evidence/
diagnostics.md`, where the firing cases are).
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

BASELINE = "off"
# Each case's allowed growth: this share of its count without the feature, or this many instructions if more
SHARE = 0.0005
FLOOR = 100
GATED = {"key_encode", "value_rebuild", "export", "indexed_limit", "read_bytes", "small_write", "graph"}
# The tokenizer seeds its hash map per process, which moves a text rebuild's count by a fraction of a percent between
# runs of one build (`docs/evidence/instruction-counts.md`)
REPORTED = {"text_rebuild"}

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
    rows.append((function, given, instructions["values"].get("old"), instructions["values"]["new"]))

seen = {function for function, *_ in rows}
undeclared = seen - GATED - REPORTED
missing = GATED - seen
failures = []
lines = ["| Operation | Without | With, nothing collecting | Change | Bound |", "|---|---:|---:|---:|---|"]
for function, given, old, new in rows:
    if old is None:
        failures.append(f"`{function} {given}` has no count without the feature to compare with")
        continue
    # No operation the benchmark runs costs nothing: a count of 0 is a measurement that collected nothing
    if old == 0 or new == 0:
        failures.append(f"`{function} {given}` counted {old:,} instructions without the feature and {new:,} with it: "
                        "Valgrind collected nothing in a run, so there is nothing to compare")
        continue
    allowed = max(FLOOR, int(old * SHARE))
    grew = new - old
    gated = function in GATED
    if gated and grew > allowed:
        failures.append(f"`{function} {given}` grew {grew:,} instructions, past its bound of {allowed:,}")
    bound = f"+{allowed:,}" if gated else "reported only"
    lines.append(f"| `{function} {given}` | {old:,} | {new:,} | {grew:+,} ({grew / old:+.4%}) | {bound} |")
if undeclared:
    failures.append(f"cases neither gated nor reported only: {sorted(undeclared)}; declare each in this script")
if missing:
    failures.append(f"gated cases the benchmark didn't run: {sorted(missing)}")

report = ("## Diagnostics overhead\n\nOne commit's instruction counts without the `diagnostics` feature and with it, "
          f"nothing collecting. A gated case may grow {SHARE:.2%} or {FLOOR} instructions, whichever is more.\n\n"
          + "\n".join(lines) + "\n")
if failures:
    report += "\n" + "\n".join(f"- **Failed:** {failure}" for failure in failures) + "\n"
print(report)
if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
    with open(summary, "a") as written:
        written.write(report)
sys.exit(1 if failures else 0)
