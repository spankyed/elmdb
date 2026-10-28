#!/usr/bin/env python3
"""Demonstrate heap/timing regressions on archived source, never mutate a production worktree.

Requires full git history (1723d5b) and a local Criterion remediation-20261027 baseline.
Run serially with all other local builds stopped; the disposable copies reuse this worktree's target cache.

Not cargo-mutants: its regressions are a heap's growth and a duration's, measured against a baseline, which no source
mutation cargo-mutants makes produces.
"""
import argparse
import io
import os
import re
from pathlib import Path
import subprocess
import tarfile
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--only", choices=["export", "value", "text", "timing"])
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
args.output.mkdir(parents=True, exist_ok=False)


def historical(path):
    return subprocess.check_output(["git", "show", f"1723d5b:{path}"], cwd=root, text=True)


with tempfile.TemporaryDirectory(prefix="elmdb-verification-mutant-") as directory:
    copy = Path(directory)
    archive = subprocess.check_output(["git", "archive", "HEAD"], cwd=root)
    with tarfile.open(fileobj=io.BytesIO(archive)) as tree:
        # Archive is generated above from this repository, never downloaded or supplied by a caller.
        tree.extractall(copy)
    changed = subprocess.check_output(["git", "diff", "--name-only", "HEAD"], cwd=root, text=True).splitlines()
    changed += subprocess.check_output(["git", "ls-files", "--others", "--exclude-standard"], cwd=root, text=True).splitlines()
    for path in changed:
        source = root / path
        if source.is_file():
            (copy / path).parent.mkdir(parents=True, exist_ok=True)
            (copy / path).write_bytes(source.read_bytes())
    env = {**os.environ, "CARGO_TARGET_DIR": str(root / "target"), "CARGO_INCREMENTAL": "0"}

    def run(name, command, expected_failure):
        print(f"{name}: running on disposable source", flush=True)
        result = subprocess.run(command, cwd=copy, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=600)
        (args.output / f"{name}.log").write_text(result.stdout)
        if expected_failure:
            assert result.returncode != 0 and "running 1 test" in result.stdout and "FAILED" in result.stdout, result.stdout
            label = {"export": "export", "value": "value_rebuild", "text": "text_rebuild"}[name]
            peak = re.search(rf"{label}/10000: peak_heap_bytes=(\d+) budget_bytes=(\d+)", result.stdout)
            assert peak and int(peak[1]) > int(peak[2]) and "stats.max_bytes" in result.stdout, result.stdout
        else:
            assert result.returncode == 0 and "Performance has regressed" in result.stdout, result.stdout
        print(f"{name}: regression detected", flush=True)

    heap = ["cargo", "test", "--offline", "-p", "elmdb", "--release", "--test", "maintenance_heap", "--",
            "--ignored", "--exact", "maintenance_heap_bounds", "--nocapture"]
    if args.only in (None, "export"):
        p = copy / "crates/elmdb/src/engine/maintenance.rs"
        good = p.read_text()
        old = historical("crates/elmdb/src/engine/maintenance.rs")
        start = good.index("pub(crate) fn primary_loads_of(")
        end = good.index("\nfn stats_of(", start)
        old_start = old.index("pub(crate) fn primary_loads_of(")
        old_end = old.index("\nfn stats_of(", old_start)
        p.write_text(good[:start] + old[old_start:old_end] + good[end:])
        run("export", heap, True)
        p.write_text(good)
    if args.only in (None, "value"):
        p = copy / "crates/elmdb/src/engine/derive.rs"
        good = p.read_text()
        old = historical("crates/elmdb/src/engine/derive.rs")
        start = good.index("            // Stage from primary records")
        end = good.index("\n        }\n        None =>", start)
        old_start = old.index("            t.clear_prefix(", old.index("pub fn rebuild("))
        old_end = old.index("\n        }\n        None =>", old_start)
        helpers = old[old.index("fn index_entries("):old.index("fn index_entries_into(")]
        helpers += old[old.index("fn buildable("):old.index("fn buildable_entry(")]
        p.write_text(good[:start] + old[old_start:old_end] + good[end:] + "\n" + helpers)
        text = copy / "crates/elmdb/src/engine/text/mod.rs"
        text_good = text.read_text()
        text_old = historical("crates/elmdb/src/engine/text/mod.rs")
        text.write_text(text_good + "\n" + text_old[text_old.index("pub fn entries("):text_old.index("pub fn entries_into(")])
        run("value", heap, True)
        p.write_text(good)
        text.write_text(text_good)
    if args.only in (None, "text"):
        p = copy / "crates/elmdb/src/engine/text/mod.rs"
        good = p.read_text()
        needle = "        while let Some((key, _)) = sorted.next()? {"
        assert good.count(needle) == 1
        p.write_text(good.replace(needle, """        let mut collected = Vec::new();
        while let Some(row) = sorted.next()? { collected.push(row); }
        let mut sorted = collected.into_iter();
        while let Some((key, _)) = sorted.next() {"""))
        run("text", heap, True)
        p.write_text(good)
    if args.only in (None, "timing"):
        p = copy / "crates/elmdb/benches/maintenance.rs"
        good = p.read_text()
        needle = "b.iter(|| black_box(encode(black_box(parts))))"
        assert needle in good
        p.write_text(good.replace(needle, "b.iter(|| { std::thread::sleep(Duration::from_millis(1)); black_box(encode(black_box(parts))) })"))
        run("timing", ["cargo", "bench", "--offline", "-p", "elmdb", "--bench", "maintenance", "--", "key_encode/128",
                       "--baseline", "remediation-20261027", "--sample-size", "10", "--warm-up-time", "0.2", "--measurement-time", "1"], False)
        p.write_text(good)
