"""Run a command in a Linux container over a copy of this checkout, its builds kept in a Docker volume the checkout owns.

`python3 scripts/linux.py [--apt PACKAGE]... [--unconfined] -- COMMAND [ARG]...`: Valgrind, the Linux-only mutants and
anything else a Mac can't run (`docs/architecture/testing.md`). The tree is copied into the volume at each run, without
`target/`, which the volume keeps, so a second run builds only what changed. `--apt` installs packages first;
`--unconfined` lifts Docker's seccomp profile, for a tool that turns off address randomization (gungraun's runner).

Every volume this makes is labelled with its checkout's path, and one whose checkout is gone is removed at the start of
the next run, so a deleted worktree's builds don't stay in Docker for ever. A run refuses when Docker has less than
`FREE_FLOOR_GB` free, naming where to look, rather than failing halfway through on a full disk.
"""
import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import sys

IMAGE = "rust:1-bookworm"
LABEL = "dev.elmdb.checkout"
# What every checkout's runs share, each named for no checkout and so never pruned: Cargo's downloaded crates, the
# toolchains rustup installs, and the tools `cargo install` puts beside cargo. Docker fills an empty volume from the
# image's own directory, so the image's toolchain and cargo come with them
SHARED = {
    "elmdb-linux-cargo-registry": "/usr/local/cargo/registry",
    "elmdb-linux-rustup": "/usr/local/rustup",
    "elmdb-linux-cargo-bin": "/usr/local/cargo/bin",
}
# A cold build of the workspace and its benches grows a work volume by several GB
FREE_FLOOR_GB = 8
# Two cores and no swap, as the apack checkout's Linux runs have
LIMITS = ["--cpus=2", "--memory=6g", "--memory-swap=6g"]

checkout = Path(__file__).resolve().parents[1]
work = f"elmdb-linux-work-{hashlib.sha256(str(checkout).encode()).hexdigest()[:12]}"


def docker(*args, check=True):
    run = subprocess.run(["docker", *args], capture_output=True, text=True)
    if check and run.returncode != 0:
        sys.exit(f"docker {args[0]} failed: {run.stderr.strip()}")
    return run


parser = argparse.ArgumentParser(usage="python3 scripts/linux.py [--apt PACKAGE]... [--unconfined] -- COMMAND [ARG]...")
parser.add_argument("--apt", action="append", default=[], metavar="PACKAGE")
parser.add_argument("--unconfined", action="store_true")
parser.add_argument("command", nargs=argparse.REMAINDER)
args = parser.parse_args()
command = args.command[1:] if args.command[:1] == ["--"] else args.command
if not command:
    parser.error("no command to run")

if docker("info", check=False).returncode != 0:
    sys.exit("Docker is not running. Start it (OrbStack: `orb start`) and run again.")

# A volume whose checkout is gone; Docker refuses to remove one a container still has mounted
listed = docker("volume", "ls", "--filter", f"label={LABEL}", "--format", f'{{{{.Name}}}}\t{{{{.Label "{LABEL}"}}}}')
for line in listed.stdout.splitlines():
    name, _, owner = line.partition("\t")
    if name != work and owner and not Path(owner).exists() and docker("volume", "rm", name, check=False).returncode == 0:
        print(f"Removed {name}, the Linux build volume of {owner}, which no longer exists.", file=sys.stderr)

df = docker("run", "--rm", *LIMITS, IMAGE, "df", "-Pk", "/").stdout.strip().splitlines()[-1].split()
free_gb = int(df[3]) / 1024**2
if free_gb < FREE_FLOOR_GB:
    sys.exit(f"Docker has {free_gb:.1f} GB free, and a run needs {FREE_FLOOR_GB}. What holds it, volumes by size: "
             "docker system df -v. A volume no container uses (LINKS 0) can go: docker volume rm <name>.")

for volume, label in ((work, str(checkout)), *((name, None) for name in SHARED)):
    if docker("volume", "inspect", volume, check=False).returncode != 0:
        docker("volume", "create", *(["--label", f"{LABEL}={label}"] if label else []), volume)

setup = []
if args.apt:
    setup.append("apt-get update -qq >/dev/null && apt-get install -y -qq " + " ".join(args.apt) + " >/dev/null")
# The copy: everything but the builds, which stay in the volume, so a file deleted here is deleted there
setup.append("find /w -mindepth 1 -maxdepth 1 ! -name target -exec rm -rf {} +")
setup.append("(cd /src && tar cf - --exclude=./target --exclude=./node_modules --exclude=./.git .) | tar xf - -C /w")
script = " && ".join(setup) + ' && exec "$@"'
os.execvp("docker", [
    "docker", "run", "--rm", "--init", *LIMITS, *(["--security-opt", "seccomp=unconfined"] if args.unconfined else []),
    "-v", f"{checkout}:/src:ro", "-v", f"{work}:/w", *(f for name, at in SHARED.items() for f in ("-v", f"{name}:{at}")),
    "-w", "/w",
    IMAGE, "sh", "-euc", script, "linux.py", *command,
])
