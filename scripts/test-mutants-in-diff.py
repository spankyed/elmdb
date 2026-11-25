"""Tests for scripts/mutants-in-diff.py's orphan copy sweep and narrow filter passthrough."""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

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

CHANGED_RUST = [
    "crates/elmdb/src/lib.rs",
    "crates/elmdb/src/engine/ranks.rs",
    "crates/elmdb/src/store/wake.rs",
    "crates/elmdb-cli/src/main.rs",
    "crates/elmdb-node/src/lib.rs",
]


def set_mtimes(paths, mtime):
    for path in paths:
        os.utime(path, (mtime, mtime))


class SelectCrateFilesTests(unittest.TestCase):
    def test_defaults_select_only_touched_crates(self):
        self.assertEqual(
            mid.select_crate_files(CRATES, ["crates/elmdb/src/engine/ranks.rs"], None, []),
            {"elmdb": ["crates/elmdb/**"], "elmdb-cli": [], "elmdb-node": []},
        )
        self.assertEqual(
            mid.select_crate_files(CRATES, [], None, []),
            {"elmdb": [], "elmdb-cli": [], "elmdb-node": []},
        )

    def test_narrow_file_routes_only_to_matching_crate(self):
        self.assertEqual(
            mid.select_crate_files(CRATES, CHANGED_RUST, None, ["./crates/elmdb/src/engine/ranks.rs"]),
            {"elmdb": ["crates/elmdb/src/engine/ranks.rs"], "elmdb-cli": [], "elmdb-node": []},
        )

    def test_windows_backslash_and_deduplication(self):
        self.assertEqual(
            mid.select_crate_files(
                CRATES,
                CHANGED_RUST,
                None,
                ["crates\\elmdb\\src\\engine\\ranks.rs", "./crates/elmdb/src/engine/ranks.rs"],
            ),
            {"elmdb": ["crates/elmdb/src/engine/ranks.rs"], "elmdb-cli": [], "elmdb-node": []},
        )

    def test_directory_targets_expand_to_matching_changed_files(self):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            (root / "crates/elmdb/src/store").mkdir(parents=True)
            self.assertEqual(
                mid.select_crate_files(
                    CRATES,
                    CHANGED_RUST,
                    None,
                    ["crates/elmdb/src/store", "crates/elmdb-cli/"],
                    root=root,
                ),
                {
                    "elmdb": ["crates/elmdb/src/store/wake.rs"],
                    "elmdb-cli": ["crates/elmdb-cli/src/main.rs"],
                    "elmdb-node": [],
                },
            )
            self.assertEqual(
                mid.select_crate_files(CRATES, CHANGED_RUST, None, ["crates/elmdb"], root=root),
                {
                    "elmdb": [
                        "crates/elmdb/src/lib.rs",
                        "crates/elmdb/src/engine/ranks.rs",
                        "crates/elmdb/src/store/wake.rs",
                    ],
                    "elmdb-cli": [],
                    "elmdb-node": [],
                },
            )

    def test_cross_crate_globs_route_each_file_to_its_owning_crate(self):
        self.assertEqual(
            mid.select_crate_files(CRATES, CHANGED_RUST, None, ["crates/*/src/lib.rs"]),
            {
                "elmdb": ["crates/elmdb/src/lib.rs"],
                "elmdb-cli": [],
                "elmdb-node": ["crates/elmdb-node/src/lib.rs"],
            },
        )
        self.assertEqual(
            mid.select_crate_files(CRATES, CHANGED_RUST, None, ["crates/"]),
            {
                "elmdb": [
                    "crates/elmdb/src/lib.rs",
                    "crates/elmdb/src/engine/ranks.rs",
                    "crates/elmdb/src/store/wake.rs",
                ],
                "elmdb-cli": ["crates/elmdb-cli/src/main.rs"],
                "elmdb-node": ["crates/elmdb-node/src/lib.rs"],
            },
        )

    def test_windows_only_glob_semantics(self):
        scope = [
            "crates/elmdb/src/lib.rs",
            "crates/elmdb/src/store/wake.rs",
        ]
        # `**/` matches zero or more directories; `*` stays within a single path segment
        self.assertEqual(
            mid.select_crate_files(CRATES, CHANGED_RUST, scope, ["crates/elmdb/src/**/*.rs"]),
            {"elmdb": scope, "elmdb-cli": [], "elmdb-node": []},
        )
        self.assertEqual(
            mid.select_crate_files(CRATES, CHANGED_RUST, scope, ["crates/elmdb/src/*.rs"]),
            {"elmdb": ["crates/elmdb/src/lib.rs"], "elmdb-cli": [], "elmdb-node": []},
        )
        self.assertEqual(
            mid.select_crate_files(CRATES, CHANGED_RUST, scope, ["crates/elmdb/src/store/*.rs"]),
            {"elmdb": ["crates/elmdb/src/store/wake.rs"], "elmdb-cli": [], "elmdb-node": []},
        )
        self.assertEqual(
            mid.select_crate_files(CRATES, CHANGED_RUST, scope, ["crates/elmdb"]),
            {"elmdb": scope, "elmdb-cli": [], "elmdb-node": []},
        )
        # Valid changed file with no Windows-only code yields empty work rather than raising
        self.assertEqual(
            mid.select_crate_files(CRATES, CHANGED_RUST, scope, ["crates/elmdb/src/engine/ranks.rs"]),
            {"elmdb": [], "elmdb-cli": [], "elmdb-node": []},
        )

    def test_longest_crate_path_wins(self):
        nested = [("outer", "crates/outer", {}), ("inner", "crates/outer/inner", {})]
        changed = ["crates/outer/inner/src/lib.rs"]
        self.assertEqual(
            mid.select_crate_files(nested, changed, None, ["crates/outer/inner/src/lib.rs"]),
            {"outer": [], "inner": ["crates/outer/inner/src/lib.rs"]},
        )

    def test_refuses_untouched_unmatched_or_invalid_paths(self):
        for bad in (
            "crates/elmdb/src/untouched.rs",
            "docs/README.md",
            "/crates/elmdb/src/lib.rs",
            "../crates/elmdb/src/lib.rs",
            ".",
        ):
            with self.subTest(bad=bad):
                with self.assertRaises(ValueError):
                    mid.select_crate_files(CRATES, CHANGED_RUST, None, [bad])


class CommandAndArgsTests(unittest.TestCase):
    def test_build_mutants_command_suite_args_and_test_filter(self):
        default_cmd = mid.build_mutants_command(
            "2", Path("out/elmdb"), ["--in-diff", "branch.diff"], [], "elmdb", ["--features", "diagnostics"]
        )
        self.assertEqual(default_cmd[-4:], ["--", "--", "--skip", "model::machine::the_engine"])

        filtered_cmd = mid.build_mutants_command(
            "2", Path("out/elmdb"), ["--in-diff", "branch.diff"], ["--re", "ranks\\.rs:42:"],
            "elmdb", ["--features", "diagnostics"], test_filter="binary(rank_default)",
        )
        self.assertEqual(filtered_cmd[-3:], ["--", "-E", "binary(rank_default)"])
        self.assertIn("--re", filtered_cmd)
        self.assertIn("ranks\\.rs:42:", filtered_cmd)

    def test_validate_test_filter_scope_refuses_multi_crate_selection(self):
        mid.validate_test_filter_scope({"elmdb": ["crates/elmdb/**"], "elmdb-cli": []}, "binary(rank_default)")
        mid.validate_test_filter_scope({"elmdb": [], "elmdb-cli": []}, "binary(rank_default)")
        mid.validate_test_filter_scope(
            {"elmdb": ["crates/elmdb/**"], "elmdb-cli": ["crates/elmdb-cli/**"]},
            None,
        )
        with self.assertRaises(ValueError) as ctx:
            mid.validate_test_filter_scope(
                {"elmdb": ["crates/elmdb/**"], "elmdb-cli": ["crates/elmdb-cli/**"]},
                "binary(rank_default)",
            )
        self.assertIn("elmdb, elmdb-cli", str(ctx.exception))
        self.assertIn("--file", str(ctx.exception))

    def test_parse_args_happy_paths_and_summary(self):
        full = mid.parse_args(["origin/main"])
        self.assertEqual(mid.format_scope_summary(full), "the change")

        sharded = mid.parse_args(["origin/main", "2", "--shard", "1/4"])
        self.assertEqual(mid.format_scope_summary(sharded), "the change, shard 1/4")

        narrow = mid.parse_args([
            "origin/main", "2",
            "--file", "crates/elmdb/src/engine/ranks.rs",
            "--file", "crates/elmdb/src/store/wake.rs",
            "--re", "ranks\\.rs:42:",
            "--re", "wake\\.rs:10:",
            "--test-filter", "binary(rank_default)",
        ])
        self.assertEqual(narrow.files, ["crates/elmdb/src/engine/ranks.rs", "crates/elmdb/src/store/wake.rs"])
        self.assertEqual(narrow.patterns, ["ranks\\.rs:42:", "wake\\.rs:10:"])
        self.assertEqual(narrow.test_filter, "binary(rank_default)")
        self.assertEqual(
            mid.format_scope_summary(narrow),
            "the change, files: crates/elmdb/src/engine/ranks.rs, crates/elmdb/src/store/wake.rs, "
            "re: ranks\\.rs:42:, wake\\.rs:10:, test-filter: binary(rank_default)",
        )

    def test_parse_args_refuses_invalid_combinations_and_empty_values(self):
        cases = [
            ["origin/main", "2", "--windows-only", "--re", "wake\\.rs:"],
            ["origin/main", "2", "--file", ""],
            ["origin/main", "2", "--file", "   "],
            ["origin/main", "2", "--re", ""],
            ["origin/main", "2", "--test-filter", ""],
            ["origin/main", "2", "--test-filter", "   "],
        ]
        for argv in cases:
            with self.subTest(argv=argv):
                stderr = io.StringIO()
                with contextlib.redirect_stderr(stderr):
                    with self.assertRaises(SystemExit) as ctx:
                        mid.parse_args(argv)
                self.assertEqual(ctx.exception.code, 2)


class ProcessDetectionTests(unittest.TestCase):
    def test_ps_parser_detects_macos_and_linux_cargo_mutants(self):
        self.assertTrue(mid._ps_has_cargo_mutants("  101 /Users/dev/.cargo/bin/cargo-mutants\n", self_pid=999))
        self.assertTrue(mid._ps_has_cargo_mutants("  202 cargo-mutants\n", self_pid=999))
        self.assertFalse(mid._ps_has_cargo_mutants("  303 /usr/bin/python3\n  304 cargo\n", self_pid=999))
        self.assertFalse(mid._ps_has_cargo_mutants("  999 cargo-mutants\n", self_pid=999))

    def test_cargo_mutants_running_fails_closed_when_ps_errors(self):
        with mock.patch.object(mid.subprocess, "run", side_effect=OSError("no ps")):
            self.assertTrue(mid.cargo_mutants_running())
        failed = subprocess.CompletedProcess(["ps"], 1, stdout="", stderr="err")
        with mock.patch.object(mid.subprocess, "run", return_value=failed):
            self.assertTrue(mid.cargo_mutants_running())


@unittest.skipIf(fcntl is None, "Unix flock sweep is skipped on platforms without fcntl")
class OrphanSweepTests(unittest.TestCase):
    def test_sweep_skips_entirely_when_cargo_mutants_is_running(self):
        with tempfile.TemporaryDirectory() as scratch:
            tmp = Path(scratch)
            idle = tmp / "cargo-mutants-elmdb-old.tmp"
            idle.mkdir()
            set_mtimes((idle,), OLD)

            removed = mid.sweep_orphaned_copies(tmp, now=NOW, is_running=lambda: True)
            self.assertEqual(removed, [])
            self.assertTrue(idle.exists())

    def test_sweep_respects_activity_locks_symlinks_and_permissions(self):
        with tempfile.TemporaryDirectory() as scratch:
            tmp = Path(scratch)

            # 1. Idle, unlocked cargo-mutants-* copy is removed
            orphaned = tmp / "cargo-mutants-elmdb-old.tmp"
            orphaned_profile = orphaned / "target/mutants"
            (orphaned_profile / "deps").mkdir(parents=True)
            (orphaned_profile / ".cargo-lock").write_bytes(b"")
            set_mtimes((orphaned_profile / "deps", orphaned_profile, orphaned / "target", orphaned), OLD)

            # 2. Idle copy with no target/ directory at all (crashed mid-copy) is removed
            no_target = tmp / "cargo-mutants-elmdb-partial.tmp"
            no_target.mkdir()
            set_mtimes((no_target,), OLD)

            # 3. Idle copy containing read-only files and directories is still removed cleanly
            readonly = tmp / "cargo-mutants-elmdb-readonly.tmp"
            ro_dir = readonly / "fixture"
            ro_dir.mkdir(parents=True)
            ro_file = ro_dir / "locked.txt"
            ro_file.write_text("data", encoding="utf-8")
            set_mtimes((ro_file, ro_dir, readonly), OLD)
            os.chmod(ro_file, 0o444)
            os.chmod(ro_dir, 0o555)

            # 4. Exact IN_USE_SECS boundary is kept; one second past is removed
            at_boundary = tmp / "cargo-mutants-elmdb-boundary.tmp"
            at_boundary.mkdir()
            set_mtimes((at_boundary,), NOW - mid.IN_USE_SECS)

            just_past = tmp / "cargo-mutants-elmdb-past.tmp"
            just_past.mkdir()
            set_mtimes((just_past,), NOW - mid.IN_USE_SECS - 1)

            # 5. Old top-level dir whose per-crate incremental subdir was written recently is kept
            active_inc = tmp / "cargo-mutants-elmdb-incremental.tmp"
            active_profile = active_inc / "target/mutants"
            crate_inc = active_profile / "incremental/elmdb-deadbeef"
            crate_inc.mkdir(parents=True)
            (active_profile / ".cargo-lock").write_bytes(b"")
            set_mtimes(
                (active_profile / "incremental", active_profile, active_inc / "target", active_inc),
                OLD,
            )
            set_mtimes((crate_inc,), RECENT)

            # 6. Idle copy whose depth-2 target-triple .cargo-lock is held is kept
            locked = tmp / "cargo-mutants-elmdb-locked.tmp"
            locked_profile = locked / "target/aarch64-apple-darwin/mutants"
            (locked_profile / "deps").mkdir(parents=True)
            lock_file = locked_profile / ".cargo-lock"
            lock_file.write_bytes(b"")
            set_mtimes(
                (locked_profile / "deps", locked_profile, locked / "target/aarch64-apple-darwin", locked / "target", locked),
                OLD,
            )

            # 7. Unrelated dir and symlink are ignored
            unrelated = tmp / "other-old-dir"
            unrelated.mkdir()
            set_mtimes((unrelated,), OLD)
            symlink = tmp / "cargo-mutants-symlink.tmp"
            symlink.symlink_to(unrelated)

            held_fd = os.open(lock_file, os.O_RDONLY)
            try:
                fcntl.flock(held_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                removed = mid.sweep_orphaned_copies(tmp, now=NOW, is_running=lambda: False)
            finally:
                os.close(held_fd)

            self.assertEqual(set(removed), {orphaned, no_target, readonly, just_past})
            for gone in (orphaned, no_target, readonly, just_past):
                self.assertFalse(gone.exists(), f"expected {gone} to be swept")
            for kept in (at_boundary, active_inc, locked, unrelated):
                self.assertTrue(kept.exists(), f"expected {kept} to be kept")
            self.assertTrue(symlink.is_symlink())


if __name__ == "__main__":
    unittest.main()
