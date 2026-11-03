"""List the upstream LMDB commits nobody here has read, and fail when there are any.

`python3 scripts/lmdb-upstream.py`: the commits on OpenLDAP's `mdb.master` past `scripts/lmdb-upstream-reviewed.txt`,
marking those heed's LMDB (the `lmdb-master-sys` in Cargo.lock) lacks. heed's LMDB trails upstream (`docs/architecture/
format.md`, LMDB), and a fix elmdb needs goes to Meilisearch's fork, so someone has to read what lands. To clear a failure:
read each commit, decide whether it reaches elmdb, and put upstream's head in the file. `GH_TOKEN` reads heed's repository.
"""
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile

UPSTREAM = "https://git.openldap.org/openldap/openldap.git"
FORK = "https://github.com/meilisearch/lmdb.git"
ROOT = Path(__file__).resolve().parent.parent


def text(*args, cwd=None):
    return subprocess.run(args, cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def heeds_lmdb():
    """The fork commit the locked `lmdb-master-sys` was built from, through the heed commit its crate records"""
    lock = (ROOT / "Cargo.lock").read_text()
    version = re.search(r'name = "lmdb-master-sys"\nversion = "([^"]+)"', lock)[1]
    subprocess.run(["cargo", "fetch", "--locked"], cwd=ROOT, check=True, capture_output=True)
    metadata = json.loads(text("cargo", "metadata", "--format-version=1", "--locked", cwd=ROOT))
    manifest = next(Path(p["manifest_path"]) for p in metadata["packages"]
                    if p["name"] == "lmdb-master-sys" and p["version"] == version)
    heed = json.loads((manifest.parent / ".cargo_vcs_info.json").read_text())["git"]["sha1"]
    return text("gh", "api", f"repos/meilisearch/heed/contents/lmdb-master-sys/lmdb?ref={heed}", "--jq", ".sha")


reviewed = (ROOT / "scripts/lmdb-upstream-reviewed.txt").read_text().strip()
pinned = heeds_lmdb()
with tempfile.TemporaryDirectory() as scratch:
    text("git", "init", "--bare", "-q", scratch)
    text("git", "fetch", "-q", "--filter=blob:none", UPSTREAM, "mdb.master:refs/upstream", cwd=scratch)
    text("git", "fetch", "-q", "--filter=blob:none", FORK, pinned, cwd=scratch)
    unread = text("git", "log", "--format=%h %ad %s", "--date=short", f"{reviewed}..refs/upstream", cwd=scratch).splitlines()
    missing = set(text("git", "log", "--format=%h", f"{pinned}..refs/upstream", cwd=scratch).split())
    head = text("git", "rev-parse", "refs/upstream", cwd=scratch)
print(f"heed's LMDB is {pinned[:10]}; upstream is {head[:10]}; read up to {reviewed[:10]}")
if not unread:
    print("No upstream commit is unread.")
    sys.exit(0)
print(f"{len(unread)} unread upstream commits, those marked * not in heed's LMDB:")
for line in unread:
    print(f"  {'*' if line.split()[0] in missing else ' '} {line}")
sys.exit("Read them, and put upstream's head in scripts/lmdb-upstream-reviewed.txt")
