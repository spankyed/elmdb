"""Check that an instruction count doesn't depend on the machine it was counted on.

`python3 scripts/instruction-determinism.py run OUT`: `benches/instructions.rs` under Valgrind, its counts and this
machine's CPU written to OUT as JSON. `python3 scripts/instruction-determinism.py compare DIR`: the JSON files under DIR
against each other, failing when a gated case differs past its bound or spends differently on allocating. Which
CPUs a run gets is luck, so it takes several machines: `.github/workflows/instruction-determinism.yml`, dispatched after
a change of allocator, toolchain, Valgrind or runner image.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from callgrind import allocator_instructions
from instruction_gates import GATED, allowed_growth, undeclared_or_missing


def run(out):
    home = Path(os.environ.get("GUNGRAUN_HOME") or tempfile.mkdtemp(prefix="instruction-determinism-"))
    env = {**os.environ, "GUNGRAUN_HOME": str(home)}
    subprocess.run(["cargo", "bench", "-p", "elmdb", "--bench", "instructions", "--", "--save-summary=json"], env=env,
                   check=True)
    cases = {}
    for summary in home.rglob("summary.json"):
        data = json.loads(summary.read_text())
        function = data["module_path"].rsplit("::", 1)[-1]
        given = data["description"].removeprefix("(").removesuffix(")")
        cases[f"{function} {given}"] = {
            "function": function,
            "instructions": data["profiles"][0]["data"]["total"]["metrics"]["Ir"]["values"]["new"],
            "allocating": allocator_instructions(summary.parent),
        }
    # aarch64 Linux names no model
    cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                if line.startswith("model name")), "unnamed")
    Path(out).write_text(json.dumps({"cpu": cpu, "cases": cases}))


def compare(directory):
    machines = [json.loads(path.read_text()) for path in Path(directory).rglob("*.json")]
    if len(machines) < 2:
        sys.exit(f"{len(machines)} machines to compare in {directory}: nothing to hold each other to")
    cpus = sorted({machine["cpu"] for machine in machines})
    print(f"{len(machines)} machines, {len(cpus)} CPU models: {', '.join(cpus)}")
    if len(cpus) < 2:
        print("These machines were all one CPU model, so this run shows nothing about the difference between models")
    cases = sorted({case for machine in machines for case in machine["cases"]})
    failures = undeclared_or_missing({each["function"] for machine in machines for each in machine["cases"].values()})
    for case in cases:
        held = [machine["cases"][case] for machine in machines if case in machine["cases"]]
        if len(held) < len(machines):
            failures.append(f"{case} was counted on {len(held)} of {len(machines)} machines")
            continue
        counts = [each["instructions"] for each in held]
        spread = max(counts) - min(counts)
        allocating = {each["allocating"] for each in held}
        print(f"{case}: {min(counts):,} to {max(counts):,} (spread {spread:,}), allocating {sorted(allocating)}")
        if held[0]["function"] not in GATED:
            continue
        # No operation costs nothing and every gated one allocates, so a 0 is a run that measured nothing
        if 0 in counts or 0 in allocating:
            failures.append(f"{case} counted {sorted(set(counts))} instructions, {sorted(allocating)} of them allocating")
            continue
        if spread > allowed_growth(min(counts)):
            failures.append(f"{case} differs by {spread:,} instructions between machines")
        if len(allocating) > 1:
            failures.append(f"{case} spends {sorted(allocating)} instructions allocating between machines")
    for failure in failures:
        print(f"Failed: {failure}")
    sys.exit(1 if failures else 0)


if sys.argv[1:2] == ["run"]:
    run(sys.argv[2])
elif sys.argv[1:2] == ["compare"]:
    compare(sys.argv[2])
else:
    sys.exit(__doc__)
