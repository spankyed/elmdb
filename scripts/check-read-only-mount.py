"""Check EROFS refusal, including a firing mutation, in private Linux mount namespaces."""
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile


def executable(root, env):
    built = subprocess.run(
        ["cargo", "test", "--offline", "-p", "elmdb", "--lib", "--no-run", "--message-format=json"],
        cwd=root, env=env, text=True, stdout=subprocess.PIPE, check=True,
    )
    binaries = [row["executable"] for line in built.stdout.splitlines()
                if (row := json.loads(line)).get("reason") == "compiler-artifact"
                and row.get("target", {}).get("kind") == ["lib"]
                and row.get("profile", {}).get("test") and row.get("executable")]
    assert len(binaries) == 1, binaries
    return binaries[0]


def mounted(binary):
    return subprocess.run([
        "sudo", "-n", "unshare", "--mount", "--propagation", "private", "env", "ELMDB_TEST_READ_ONLY_MOUNT=1",
        binary, "--exact", "store::tests::read_only_mount_requires_reader_registration", "--ignored", "--nocapture",
    ], text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)


positive = mounted(executable(Path.cwd(), os.environ))
print(positive.stdout)
assert positive.returncode == 0 and "ELMDB_READ_ONLY_MOUNT_REFUSED" in positive.stdout
with tempfile.TemporaryDirectory(prefix="elmdb-erofs-mutation-") as temporary:
    root = Path(temporary)
    with tarfile.open(fileobj=io.BytesIO(subprocess.check_output(["git", "archive", "HEAD"]))) as archive:
        archive.extractall(root)
    source = root / "crates/elmdb/src/store/mod.rs"
    code = source.read_text()
    assert code.count("require_registered_reader(&env, &canonical)?;") == 1
    source.write_text(code.replace("require_registered_reader(&env, &canonical)?;", "// deliberately omit the post-open lock-table check"))
    negative = mounted(executable(root, {**os.environ, "CARGO_TARGET_DIR": str(root / "target")}))
    if not (negative.returncode != 0 and "running 1 test" in negative.stdout
            and "a read-only mount must not open without a lock table" in negative.stdout and "FAILED" in negative.stdout):
        print(negative.stdout)
        raise SystemExit("the EROFS mutation did not fire the selected regression")
    print("Actual EROFS regression fails on the disposable mutation as expected")
