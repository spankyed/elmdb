"""Show the Windows published-attribute regression firing in a disposable source copy."""
import io
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

with tempfile.TemporaryDirectory(prefix="elmdb-backup-mutation-") as temporary:
    root = Path(temporary)
    archive = subprocess.check_output(["git", "archive", "HEAD"])
    with tarfile.open(fileobj=io.BytesIO(archive)) as packed:
        packed.extractall(root)
    source = root / "crates/elmdb/src/store/backup.rs"
    code = source.read_text()
    start = code.index("    // SAFETY: source is a live terminated path.")
    end = code.index("    const REPLACE_EXISTING", start)
    source.write_text(code[:start] + code[end:])
    result = subprocess.run([
        "cargo", "test", "--offline", "-p", "elmdb",
        "backup_creates_nested_destinations_and_leaves_only_the_complete_data_file", "--", "--nocapture",
    ], cwd=root, env={**os.environ, "CARGO_TARGET_DIR": str(root / "target")}, text=True,
       stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    if not (result.returncode != 0 and "running 1 test" in result.stdout
            and "a published backup must not retain FILE_ATTRIBUTE_TEMPORARY" in result.stdout
            and "FAILED" in result.stdout):
        print(result.stdout)
        raise SystemExit("the disposable Windows attribute mutation did not fire the regression")
    print("Windows temporary-attribute regression fails on the disposable mutation as expected")
