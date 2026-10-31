"""Mutate what a change touches, each crate against its own suite, and report the survivors without failing on them.

`python3 scripts/mutants-in-diff.py BASE [JOBS]`: cargo-mutants over `git diff BASE...HEAD`, what HEAD changed since it
left BASE, once per workspace crate, since `.cargo/mutants.toml` tests every mutant against the engine's suite and a
binding's or the CLI's mutant is tested by its own. Exits nonzero only when cargo-mutants itself fails (a bad diff, a baseline that doesn't pass); a
missed mutant or a timeout is listed, and in GitHub Actions annotated on its line and written to the step summary.
"""
import json
import os
from pathlib import Path
import subprocess
import sys

# Exit codes cargo-mutants answers for a run that tested what it was given: nothing missed, a mutant missed, a timeout
TESTED = {0, 2, 3}
# The engine's suite without the model harness, which caught none of the baseline's survivors
# (`docs/evidence/mutation-baseline.md`) and is most of its time
SUITE_ARGS = {"elmdb": ["--", "--", "--skip", "model::the_engine"]}

base = sys.argv[1]
jobs = sys.argv[2] if len(sys.argv) > 2 else "3"
root = Path.cwd()
out = root / "target/mutants-in-diff"
out.mkdir(parents=True, exist_ok=True)
diff = out / "branch.diff"
diff.write_bytes(subprocess.check_output(["git", "diff", f"{base}...HEAD"]))

metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1"]))
crates = sorted((p["name"], Path(p["manifest_path"]).parent.relative_to(root).as_posix(), p["features"])
                for p in metadata["packages"])
assert crates, "the workspace names no crate"

missed, timeouts = [], []
for name, path, features in crates:
    # A crate's diagnostics are compiled only with its feature, so a mutant of them builds and is tested with it
    with_features = ["--features", "diagnostics"] if "diagnostics" in features else []
    run = subprocess.run(["cargo", "mutants", "-j", jobs, "-o", str(out / name), "--in-diff", str(diff),
                          "--file", f"{path}/**", "--test-package", name, *with_features, *SUITE_ARGS.get(name, [])])
    if run.returncode not in TESTED:
        sys.exit(f"cargo-mutants failed for {name} with exit code {run.returncode}")
    results = out / name / "mutants.out"
    for found, file in ((missed, "missed.txt"), (timeouts, "timeout.txt")):
        if (results / file).exists():
            found += (results / file).read_text().splitlines()

print(f"{len(missed)} missed, {len(timeouts)} timeouts")
for line in missed:
    print(f"MISSED  {line}")
for line in timeouts:
    print(f"TIMEOUT {line}")

if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
    for line in missed:
        file, number, _ = line.split(":", 2)
        print(f"::warning file={file},line={number},title=Missed mutant::{line}")
    with open(summary, "a") as written:
        written.write(f"## Mutants in this change\n\n{len(missed)} missed, {len(timeouts)} timeouts\n\n")
        for heading, lines in (("Missed", missed), ("Timeouts", timeouts)):
            if lines:
                written.write(f"### {heading}\n\n" + "".join(f"- `{line}`\n" for line in lines) + "\n")
