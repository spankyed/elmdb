"""Prove Linux kernel-claim exclusion and unsupported-rename fallback assertions fire on disposable regressions.

Not cargo-mutants: both mutations are Linux-only code, run on Linux CI, and each swaps one expression for another (an
unbound claim, the fallback's link for its error) where cargo-mutants replaces a function's whole answer; the claim's is
in elmdb-node, which `.cargo/mutants.toml` doesn't test.
"""
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

root = Path.cwd()
with tempfile.TemporaryDirectory(prefix="elmdb-claim-publish-mutations-") as temporary:
    copy = Path(temporary)
    with tarfile.open(fileobj=io.BytesIO(subprocess.check_output(["git", "archive", "HEAD"]))) as archive:
        archive.extractall(copy)
    cases = [
        ("crates/elmdb-node/src/process.rs", "elmdb-node",
         "process::tests::independent_kernel_claims_conflict_and_are_reclaimed_on_close",
         "Claim::bind_addr(&claim_address()?)", "Claim::unbound()", "unwrap_err"),
        ("crates/elmdb/src/store/backup.rs", "elmdb",
         "store::backup::publication_tests::unsupported_exclusive_rename_falls_back_without_overwriting",
         "std::fs::hard_link(source, target)\n        }\n        result => result,",
         "Err(error)\n        }\n        result => result,", "unwrap"),
    ]
    for path, package, test, before, after, marker in cases:
        source = copy / path
        good = source.read_text()
        assert good.count(before) == 1, path
        source.write_text(good.replace(before, after))
        ran = subprocess.run(["cargo", "test", "--offline", "-p", package, "--lib", test, "--", "--exact", "--nocapture"],
                             cwd=copy, env={**os.environ, "CARGO_TARGET_DIR": str(root / "target")}, text=True,
                             stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not (ran.returncode and "running 1 test" in ran.stdout and marker in ran.stdout and "FAILED" in ran.stdout):
            print(ran.stdout)
            raise SystemExit(f"the runtime mutation did not fire: {test}")
        print(f"Disposable runtime mutation fired: {test}", flush=True)
        source.write_text(good)
