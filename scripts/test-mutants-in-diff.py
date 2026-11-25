"""Tests for scripts/mutants-in-diff.py's orphan copy sweep and narrow filter passthrough."""
import importlib.util
import os
from pathlib import Path
import tempfile

try:
    import fcntl
except ImportError:
    fcntl = None

SPEC = importlib.util.spec_from_file_location("mutants_in_diff", Path(__file__).with_name("mutants-in-diff.py"))
assert SPEC and SPEC.loader
mid = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(mid)

NOW = 1_800_000_000.0
OLD = NOW - mid.IN_USE_SECS - 600
RECENT = NOW - 300

CRATES = [
    ("elmdb", "crates/elmdb", {"diagnostics": []}),
    ("elmdb-cli", "crates/elmdb-cli", {}),
    ("elmdb-node", "crates/elmdb-node", {"diagnostics": []}),
]

# Default crate file selection without narrow filters
assert mid.select_crate_files(CRATES, None, []) == {
    "elmdb": ["crates/elmdb/**"],
    "elmdb-cli": ["crates/elmdb-cli/**"],
    "elmdb-node": ["crates/elmdb-node/**"],
}

# Narrow --file routes only to the matching crate and leaves others empty
assert mid.select_crate_files(CRATES, None, ["./crates/elmdb/src/engine/ranks.rs"]) == {
    "elmdb": ["crates/elmdb/src/engine/ranks.rs"],
    "elmdb-cli": [],
    "elmdb-node": [],
}

# Narrow --file combined with --windows-only intersects with the Windows-only scope
assert mid.select_crate_files(
    CRATES,
    ["crates/elmdb/src/store/wake.rs", "crates/elmdb/src/engine/ranks.rs"],
    ["crates/elmdb/src/store/*.rs"],
) == {
    "elmdb": ["crates/elmdb/src/store/wake.rs"],
    "elmdb-cli": [],
    "elmdb-node": [],
}

# An unmatched --file refuses rather than silently mutating nothing
try:
    mid.select_crate_files(CRATES, None, ["docs/README.md"])
    raise AssertionError("expected ValueError for unmatched --file")
except ValueError as error:
    assert "does not match any workspace crate" in str(error)

# Default command keeps SUITE_ARGS; --test-filter replaces it with nextest -E
default_cmd = mid.build_mutants_command("2", Path("out/elmdb"), ["--in-diff", "branch.diff"], [], "elmdb", ["--features", "diagnostics"])
assert default_cmd[-4:] == ["--", "--", "--skip", "model::machine::the_engine"], default_cmd

filtered_cmd = mid.build_mutants_command(
    "2", Path("out/elmdb"), ["--in-diff", "branch.diff"], ["--re", "ranks\\.rs:42:"],
    "elmdb", ["--features", "diagnostics"], test_filter="binary(engine_correctness)",
)
assert filtered_cmd[-3:] == ["--", "-E", "binary(engine_correctness)"], filtered_cmd
assert "--re" in filtered_cmd and "ranks\\.rs:42:" in filtered_cmd

# parse_args refuses --re combined with --windows-only
try:
    mid.parse_args(["origin/main", "2", "--windows-only", "--re", "wake\\.rs:"])
    raise AssertionError("expected SystemExit when combining --windows-only and --re")
except SystemExit as error:
    assert error.code == 2

# Orphaned cargo-mutants-* copy sweep
if fcntl is not None:
    with tempfile.TemporaryDirectory() as scratch:
        tmp = Path(scratch)

        # 1. Idle, unlocked cargo-mutants-* copy is removed
        orphaned = tmp / "cargo-mutants-elmdb-old.tmp"
        orphaned_profile = orphaned / "target/mutants"
        (orphaned_profile / "deps").mkdir(parents=True)
        (orphaned_profile / ".cargo-lock").write_bytes(b"")
        for p in (orphaned_profile / "deps", orphaned_profile, orphaned):
            os.utime(p, (OLD, OLD))

        # 2. Old top-level dir whose nested target/mutants/deps was written recently is kept
        active_nested = tmp / "cargo-mutants-elmdb-active.tmp"
        active_profile = active_nested / "target/mutants"
        (active_profile / "deps").mkdir(parents=True)
        (active_profile / ".cargo-lock").write_bytes(b"")
        os.utime(active_nested, (OLD, OLD))
        os.utime(active_profile, (OLD, OLD))
        os.utime(active_profile / "deps", (RECENT, RECENT))

        # 3. Idle copy whose .cargo-lock is held by an active Cargo process is kept
        locked = tmp / "cargo-mutants-elmdb-locked.tmp"
        locked_profile = locked / "target/mutants"
        (locked_profile / "deps").mkdir(parents=True)
        lock_file = locked_profile / ".cargo-lock"
        lock_file.write_bytes(b"")
        for p in (locked_profile / "deps", locked_profile, locked):
            os.utime(p, (OLD, OLD))

        # 4. Unrelated dir and symlink are ignored
        unrelated = tmp / "other-old-dir"
        unrelated.mkdir()
        os.utime(unrelated, (OLD, OLD))
        symlink = tmp / "cargo-mutants-symlink.tmp"
        symlink.symlink_to(unrelated)

        held_fd = os.open(lock_file, os.O_RDONLY)
        try:
            fcntl.flock(held_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            removed = mid.sweep_orphaned_copies(tmp, now=NOW)
        finally:
            os.close(held_fd)

        assert removed == [orphaned], removed
        assert not orphaned.exists()
        assert active_nested.exists()
        assert locked.exists()
        assert unrelated.exists()
        assert symlink.is_symlink()

print("ok")
