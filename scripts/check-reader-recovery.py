"""Prove reader-recovery regressions fail in disposable source copies, never in the checkout."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

root = Path(__file__).resolve().parents[1]
relative = Path("crates/elmdb/src/store/mod.rs")
original = (root / relative).read_text()
cases = [
    ("no cleanup", "            env.clear_stale_readers()?;", "            // Deliberately leave dead slots registered.",
     "a_read_refuses_live_reader_exhaustion_then_recovers_the_killed_readers"),
    ("extra retry", "            begin()\n        }", "            begin().or_else(|_| begin())\n        }",
     "reader_recovery_retries_only_once_and_only_readers_full"),
    ("unrelated error retried", "        result => result,", "        Err(_) => begin(),\n        result => result,",
     "reader_recovery_retries_only_once_and_only_readers_full"),
]
for label, before, after, test in cases:
    assert original.count(before) == 1, (label, "mutation site drifted")
    with tempfile.TemporaryDirectory(prefix="elmdb-reader-mutation-") as temporary:
        copy = Path(temporary) / "source"
        shutil.copytree(root, copy, ignore=shutil.ignore_patterns(".git", "target", "node_modules"))
        (copy / relative).write_text(original.replace(before, after))
        name = "store::tests::" + test
        result = subprocess.run(
            ["cargo", "test", "--offline", "-p", "elmdb", "--lib", name, "--", "--exact", "--nocapture"],
            cwd=copy, env={**os.environ, "CARGO_TARGET_DIR": str(root / "target")},
            text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=240,
        )
        assert result.returncode != 0 and "running 1 test" in result.stdout and f"{name} ... FAILED" in result.stdout, result.stdout
        print(f"Reader recovery assertion rejects: {label}", flush=True)
