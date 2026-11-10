"""Mutate what a change touches, each crate against its own suite, and fail on a survivor.

`python3 scripts/mutants-in-diff.py BASE [JOBS] [--shard K/N] [--windows-only]`: cargo-mutants over
`git diff BASE...HEAD`, what HEAD changed since it left BASE, once per workspace crate, since `.cargo/mutants.toml` tests
every mutant against the engine's suite and a binding's or the CLI's mutant is tested by its own. Exits nonzero when a
mutant is missed, or when cargo-mutants itself fails (a bad diff, a baseline that doesn't pass): a survivor is a test
missing, or an equivalent mutant a `// mutant-skip:` names with why. A timeout is listed and doesn't fail the run, since
load alone can cause one. In GitHub Actions each is annotated on its line and written to the step summary.

`--shard K/N` tests the Kth of N shares of the mutants, so N runners split them; each runs the unmutated baseline,
which is what its timeouts are measured from. `--windows-only` mutates only the code that compiles on Windows alone,
which a Linux run can't build: the rest of the change is Linux's to mutate.

A mutant of code the runner doesn't compile, behind a `#[cfg(...)]` for another platform, builds unchanged and passes
every test, so it would be reported missed whatever the tests: a Linux run leaves out code compiled only on another
platform, and a Windows run takes only code compiled on Windows alone. macOS-only code is read on a Mac
(`docs/architecture/testing.md`, Mutation testing).
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

# Exit codes cargo-mutants answers for a run that tested what it was given: nothing missed, a mutant missed, a timeout
TESTED = {0, 2, 3}
# The engine's suite without the model harness, which caught none of the baseline's survivors
# (`docs/evidence/mutation-baseline.md`) and is most of its time
SUITE_ARGS = {"elmdb": ["--", "--", "--skip", "model::the_engine"]}

parser = argparse.ArgumentParser()
parser.add_argument("base")
parser.add_argument("jobs", nargs="?", default="3")
parser.add_argument("--shard", help="K/N: the Kth of N shares of the mutants")
parser.add_argument("--windows-only", action="store_true")
args = parser.parse_args()

root = Path.cwd()
out = root / "target/mutants-in-diff"
out.mkdir(parents=True, exist_ok=True)
diff = out / "branch.diff"
diff.write_bytes(subprocess.check_output(["git", "diff", f"{args.base}...HEAD"]))
changed = subprocess.check_output(["git", "diff", "--name-only", f"{args.base}...HEAD"], text=True).split()


# What each runner's platform answers for the cfg predicates that name one; anything else (test, a feature, loom) is
# unknown, and code behind an unknown predicate is never taken for another platform's
PLATFORMS = {
    "linux": {"unix": True, "windows": False, "target_os": "linux", "target_family": "unix"},
    "windows": {"unix": False, "windows": True, "target_os": "windows", "target_family": "windows"},
}


def holds(predicate, platform):
    """Whether a cfg predicate holds on `platform`: True, False, or None when it names something else"""
    tokens = re.findall(r'"[^"]*"|[A-Za-z_][A-Za-z0-9_]*|[(),=]', predicate)
    at = 0

    def parse():
        nonlocal at
        name = tokens[at]
        at += 1
        if at < len(tokens) and tokens[at] == "(":
            at += 1
            values = []
            while tokens[at] != ")":
                values.append(parse())
                if tokens[at] == ",":
                    at += 1
            at += 1
            if name == "all":
                return False if False in values else (True if all(v is True for v in values) else None)
            if name == "any":
                return True if True in values else (False if all(v is False for v in values) else None)
            if name == "not":
                return None if values[0] is None else not values[0]
            return None
        if at < len(tokens) and tokens[at] == "=":
            value = tokens[at + 1].strip('"')
            at += 2
            known = PLATFORMS[platform].get(name)
            return None if known is None else known == value
        return PLATFORMS[platform].get(name)

    return parse()


def compiled_out(path, platform):
    """The lines of a source file behind a `#[cfg(...)]` that doesn't hold on `platform`"""
    lines = (root / path).read_text(encoding="utf-8").splitlines()
    out = set()
    if any(re.match(r"\s*#!\[cfg\((.*)\)\]\s*$", line) and holds(re.match(r"\s*#!\[cfg\((.*)\)\]", line).group(1), platform) is False
           for line in lines):
        return set(range(1, len(lines) + 1))
    for at, line in enumerate(lines):
        found = re.match(r"\s*#\[cfg\((.*)\)\](.*)$", line)
        if not found or holds(found.group(1), platform) is not False:
            continue
        # The item the attribute is on: to its closing brace, to the `;` or `,` that ends a statement or a field first,
        # or to the brace closing what holds it
        depth, parens, opened, end = 0, 0, False, None
        rest = [found.group(2)] + lines[at + 1:]
        for offset, text in enumerate(rest):
            code = re.sub(r'"(\\.|[^"\\])*"', '""', text.split("//")[0])
            if code.lstrip().startswith("#["):
                continue
            for char in code:
                if char in "([":
                    parens += 1
                elif char in ")]":
                    parens -= 1
                elif char == "{":
                    depth, opened = depth + 1, True
                elif char == "}":
                    depth -= 1
                elif char in ";," and depth == 0 and parens == 0 and not opened:
                    end = offset
                    break
                if (opened and depth == 0) or depth < 0:
                    end = offset
                    break
            if end is not None:
                break
        out.update(range(at + 1, at + 1 + (end if end is not None else len(rest) - 1) + 1))
    return out


def lines_pattern(path, numbers):
    """A regex over cargo-mutants' names for the mutants on these lines of `path`"""
    return f"^{re.escape(path)}:({'|'.join(str(n) for n in sorted(numbers))}):"


rust = [path for path in changed if path.endswith(".rs") and (root / path).exists()]
if args.windows_only:
    # Only what compiles on Windows and not on Linux
    only_windows = {path: compiled_out(path, "linux") - compiled_out(path, "windows") for path in rust}
    only_windows = {path: numbers for path, numbers in only_windows.items() if numbers}
    scope = sorted(only_windows)
    platform_args = [arg for path, numbers in only_windows.items() for arg in ("--re", lines_pattern(path, numbers))]
else:
    scope = None
    elsewhere = {path: numbers for path in rust if (numbers := compiled_out(path, "linux"))}
    platform_args = [arg for path, numbers in elsewhere.items() for arg in ("--exclude-re", lines_pattern(path, numbers))]

metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1"]))
crates = sorted((p["name"], Path(p["manifest_path"]).parent.relative_to(root).as_posix(), p["features"])
                for p in metadata["packages"])
assert crates, "the workspace names no crate"

# cargo-mutants builds each job's copy of the tree in a directory of its own; a CARGO_TARGET_DIR from the caller would
# put every copy's build in one, where a job can run another's test binary and report a caught mutant as missed
env = {key: value for key, value in os.environ.items() if key != "CARGO_TARGET_DIR"}
sharding = ["--shard", args.shard] if args.shard else []
missed, timeouts = [], []
for name, path, features in crates:
    files = [f"{path}/**"] if scope is None else [file for file in scope if file.startswith(f"{path}/")]
    if not files:
        continue
    # A crate's diagnostics are compiled only with its feature, so a mutant of them builds and is tested with it
    with_features = ["--features", "diagnostics"] if "diagnostics" in features else []
    # A change that holds no mutant of the crate has cargo-mutants write no results, and those an earlier run left
    # would be read as this one's
    results = out / name / "mutants.out"
    shutil.rmtree(results, ignore_errors=True)
    run = subprocess.run(["cargo", "mutants", "-j", args.jobs, "-o", str(out / name), "--in-diff", str(diff),
                          *(arg for file in files for arg in ("--file", file)), *sharding, *platform_args, "--test-package", name,
                          *with_features, *SUITE_ARGS.get(name, [])],
                         env=env)
    if run.returncode not in TESTED:
        sys.exit(f"cargo-mutants failed for {name} with exit code {run.returncode}")
    for found, file in ((missed, "missed.txt"), (timeouts, "timeout.txt")):
        if (results / file).exists():
            found += (results / file).read_text().splitlines()

what = "Windows-only code" if args.windows_only else "the change"
what += f", shard {args.shard}" if args.shard else ""
if scope == []:
    print("The change holds no Windows-only code to mutate")
print(f"{len(missed)} missed, {len(timeouts)} timeouts ({what})")
for line in missed:
    print(f"MISSED  {line}")
for line in timeouts:
    print(f"TIMEOUT {line}")

if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
    for line in missed:
        file, number, _ = line.split(":", 2)
        print(f"::error file={file},line={number},title=Missed mutant::{line}")
    with open(summary, "a", encoding="utf-8") as written:
        written.write(f"## Mutants in {what}\n\n{len(missed)} missed, {len(timeouts)} timeouts\n\n")
        for heading, lines in (("Missed", missed), ("Timeouts", timeouts)):
            if lines:
                written.write(f"### {heading}\n\n" + "".join(f"- `{line}`\n" for line in lines) + "\n")

# A survivor fails the run: kill it with a test, or name it in a `// mutant-skip:` with why it is equivalent
sys.exit(1 if missed else 0)
