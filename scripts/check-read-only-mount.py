"""Check real Linux EROFS read-only mount behavior, including a firing disposable-copy mutation."""
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile


def executable(root: Path, env: dict[str, str]) -> str:
    built = subprocess.run(
        ["cargo", "test", "--offline", "-p", "elmdb", "--test", "read_only_mount", "--no-run", "--message-format=json"],
        cwd=root,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        check=True,
    )
    binaries = [
        row["executable"]
        for line in built.stdout.splitlines()
        if (row := json.loads(line)).get("reason") == "compiler-artifact"
        and row.get("target", {}).get("name") == "read_only_mount"
        and row.get("profile", {}).get("test")
        and row.get("executable")
    ]
    assert len(binaries) == 1, binaries
    return binaries[0]


def mounted(binary: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            "sudo",
            "-n",
            "unshare",
            "--mount",
            "--propagation",
            "private",
            "env",
            "ELMDB_TEST_READ_ONLY_MOUNT=1",
            binary,
            "--exact",
            "read_only_mount_opens_without_writing_locks_and_blocks_writes",
            "--ignored",
            "--nocapture",
        ],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )


positive = mounted(executable(Path.cwd(), dict(os.environ)))
print(positive.stdout)
assert positive.returncode == 0 and "ELMDB_READ_ONLY_MOUNT_OK" in positive.stdout

# Disposable-copy negative control: force `read_only` open to call writable `lock_file` without
# EROFS fallback, and prove the real Linux read-only bind-mount test fails loudly!
with tempfile.TemporaryDirectory(prefix="elmdb-erofs-mutation-") as temporary:
    root = Path(temporary)
    with tarfile.open(fileobj=io.BytesIO(subprocess.check_output(["git", "archive", "HEAD"]))) as archive:
        archive.extractall(root)
    source = root / "crates/elmdb-storage/src/durability.rs"
    code = source.read_text()
    target = """            let lock_res = if self.read_only {
                self.vfs.lock_file_shared_if_exists(&self.sidecar_lock_path)
            } else {
                self.vfs.lock_file(&self.sidecar_lock_path).map(Some)
            };
            match lock_res {
                Ok(guard) => guard,
                Err(StorageError::Io(ref e))
                    if self.read_only
                        && matches!(
                            e.kind(),
                            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
                        ) =>
                {
                    None
                }
                Err(err) => return Err(err),
            }"""
    replacement = """            // Deliberate negative-control mutation: force writable sidecar lock creation on read_only opens.
            self.vfs.lock_file(&self.sidecar_lock_path).map(Some)?"""
    assert code.count(target) == 1, "expected exact sidecar lock block in durability.rs"
    source.write_text(code.replace(target, replacement))
    negative = mounted(executable(root, {**os.environ, "CARGO_TARGET_DIR": str(root / "target")}))
    if not (
        negative.returncode != 0
        and "running 1 test" in negative.stdout
        and "read_only open must succeed on real EROFS mount" in negative.stdout
        and "FAILED" in negative.stdout
    ):
        print(negative.stdout)
        raise SystemExit("the EROFS negative-control mutation did not fire its regression")
    print("Actual EROFS regression rejects writable sidecar lock creation on read_only opens")
