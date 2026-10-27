"""Exercise the binding as an unmapped numeric UID with no home or /proc, in a private mount namespace."""
import json
import os
from pathlib import Path
import pwd
import shutil
import subprocess
import sys
import tempfile


def isolated(binary, node):
    # This branch runs only under unshare --mount --propagation private. Hiding /proc cannot affect the runner.
    subprocess.run(["mount", "-t", "tmpfs", "-o", "mode=0555", "tmpfs", "/proc"], check=True)
    uid = 61234
    try:
        pwd.getpwuid(uid)
    except KeyError:
        pass
    else:
        raise AssertionError("the numeric UID fixture unexpectedly has an account")
    with tempfile.TemporaryDirectory(prefix="elmdb-no-account-") as temporary:
        root = Path(temporary)
        os.chown(root, uid, uid)
        copy = root / "second.node"
        shutil.copyfile(binary, copy)
        source = f"""
          const assert = require('node:assert/strict');
          const fs = require('node:fs');
          assert.equal(fs.existsSync('/proc/self'), false);
          const first = require({json.dumps(binary)});
          const second = require({json.dumps(str(copy))});
          first.Database.open({json.dumps(str(root / 'first'))}, {{syncs: false}}).close();
          assert.throws(() => second.Database.open({json.dumps(str(root / 'second'))}, {{syncs: false}}),
            {{code: 'ALREADY_LOADED'}});
          assert.equal(fs.existsSync({json.dumps(str(root / 'second'))}), false);
          console.log('ELMDB_NO_ACCOUNT_HOME_OR_PROC_CLAIMED');
        """

        def unprivileged():
            os.setgroups([])
            os.setgid(uid)
            os.setuid(uid)

        ran = subprocess.run([node, "-e", source], env={**os.environ, "HOME": "/nonexistent", "TMPDIR": "/nonexistent"},
                             preexec_fn=unprivileged, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                             timeout=30)
        print(ran.stdout, end="", flush=True)
        assert ran.returncode == 0 and "ELMDB_NO_ACCOUNT_HOME_OR_PROC_CLAIMED" in ran.stdout


if len(sys.argv) > 1 and sys.argv[1] == "--isolated":
    isolated(sys.argv[2], sys.argv[3])
else:
    binary = next(Path("crates/elmdb-node").resolve().glob("*.node"))
    node = str(Path(shutil.which("node")).resolve())
    subprocess.run(["sudo", "-n", "unshare", "--mount", "--propagation", "private", sys.executable,
                    str(Path(__file__).resolve()), "--isolated", str(binary), node], check=True, timeout=45)
