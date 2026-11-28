"""Mutate what a change touches, each crate against its own suite, and fail on a survivor.

`python3 scripts/mutants-in-diff.py BASE [JOBS] [--shard K/N] [--windows-only] [--resume] [--file GLOB ...] [--re PATTERN ...] [--test-filter EXPR]`:
cargo-mutants over `git diff BASE...HEAD`, what HEAD changed since it left BASE, once per workspace crate, testing each
crate against its own suite (plus downstream integration/DST test packages for internal v2 layered crates). Exits
nonzero when a mutant is missed, or when cargo-mutants itself fails (a bad diff, a baseline that doesn't pass): a
survivor is a test missing, or an equivalent mutant a `// mutant-skip:` names with why. A timeout is listed
and doesn't fail the run, since load alone can cause one. In GitHub Actions each is annotated on its line and written
to the step summary.

The configuration every run here reads is the one `cargo xtask mutant-exclusions` writes: `.cargo/mutants.toml` with an
exclusion for each `// mutant-skip:` comment, which only the source holds.

It also exits nonzero when a mutant failed to build for want of a file or a manifest that reads, and not for what
the mutant changed: cargo-mutants lists such a mutant as unviable, as it does one whose replacement doesn't compile, but
it means the copy of the tree the mutant was built in was damaged, and the mutant was never tested.

`--resume` leaves out the mutants an earlier run of the same change caught, so a run that was stopped, or one of
several shares run again, tests only what is left. What was caught is kept in `target/mutants-in-diff/<crate>/caught.txt`
beside the diff it was caught in, and forgotten when the diff is another.

`--file`, `--re` and `--test-filter` narrow a local iteration without bypassing the 20 GB disk floor or exclusions:
`--file` limits mutation to matching Rust paths or globs in the diff (refusing filters that match no changed file),
`--re` filters mutant names (not combined with `--windows-only`), and `--test-filter` hands a cargo-nextest `-E`
expression to cargo-mutants in place of the default suite filter (requiring a single crate in scope). Outside CI, when
no `cargo-mutants` process is running, unlocked `cargo-mutants-*` temp copies idle past an hour are swept before the
disk check.

`--shard K/N` tests the Kth of N shares of the mutants, so N runners split them; each runs the unmutated baseline,
which is what its timeouts are measured from. `--windows-only` mutates only the code that compiles on Windows alone,
which a Linux run can't build: the rest of the change is Linux's to mutate.

A mutant of code the runner doesn't compile, behind a `#[cfg(...)]` for another platform, builds unchanged and passes
every test, so it would be reported missed whatever the tests: a Linux run leaves out code compiled only on another
platform, and a Windows run takes only code compiled on Windows alone. macOS-only code is read on a Mac
(`docs/architecture/testing.md`, Mutation testing).
"""
import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import sys
import tempfile
import time

try:
    import fcntl
except ImportError:
    fcntl = None

# Exit codes cargo-mutants answers for a run that tested what it was given: nothing missed, a mutant missed, a timeout
TESTED = {0, 2, 3}
# Optional per-crate extra nextest arguments when no explicit --test-filter is provided
SUITE_ARGS = {}
# Internal v2 layered crates whose mutants are tested against BOTH their own crate suite AND downstream DST/E2E suites
LAYERED_TEST_PACKAGES = {
    "elmdb-storage": ("elmdb-sim", "elmdb-lmdb", "elmdb"),
    "elmdb-lmdb": ("elmdb",),
    "elmdb-wire": ("elmdb",),
    "elmdb-engine": ("elmdb",),
    "elmdb-projections": ("elmdb-query", "elmdb"),
    "elmdb-vec": ("elmdb",),
    "elmdb-query": ("elmdb",),
    "elmdb-invalidation": ("elmdb",),
}
# Each job's copy builds the workspace from cold in the system's temporary directory: below the floor `cargo xtask
# pre-push` holds a run to (`xtask/src/build_dir.rs`, FLOOR) the disk would fill part way through, and the copies with
# it, and whatever else the machine runs. Nothing is run when nothing is in scope, so nothing is asked of the disk then.
# A CI runner's disk is its run's alone, which a full disk ends and nothing more, and holds less than the floor
FLOOR = 20 << 30
# A cargo-mutants copy written to within an hour may still belong to a running job and is never swept
IN_USE_SECS = 60 * 60
# What a build's log says when a file of the tree it was built in is missing or doesn't read
DAMAGED = re.compile(r"could not parse .* \(manifest\)|failed to (read|load) |No such file or directory")

# What each runner's platform answers for the cfg predicates that name one; anything else (test, a feature) is
# unknown, and code behind an unknown predicate is never taken for another platform's. Plain mutation builds never
# pass `--cfg loom` or `--cfg shuttle`, so those cfgs are False on both platforms (cancelling out in --windows-only).
PLATFORMS = {
    "linux": {
        "unix": True,
        "windows": False,
        "target_os": "linux",
        "target_family": "unix",
        "loom": False,
        "shuttle": False,
    },
    "windows": {
        "unix": False,
        "windows": True,
        "target_os": "windows",
        "target_family": "windows",
        "loom": False,
        "shuttle": False,
    },
    "shuttle-linux": {
        "unix": True,
        "windows": False,
        "target_os": "linux",
        "target_family": "unix",
        "loom": False,
        "shuttle": True,
    },
}


def holds(predicate, platform):
    """Whether a cfg predicate holds on `platform`: True, False, or None when it names something else"""
    tokens = re.findall(r'"(?:\\.|[^"\\])*"|[A-Za-z_][A-Za-z0-9_]*|[(),=]', predicate)
    if not tokens:
        return None
    at = 0

    def parse():
        nonlocal at
        if at >= len(tokens):
            return None
        name = tokens[at]
        at += 1
        if at < len(tokens) and tokens[at] == "(":
            at += 1
            values = []
            while at < len(tokens) and tokens[at] != ")":
                values.append(parse())
                if at < len(tokens) and tokens[at] == ",":
                    at += 1
            if at < len(tokens) and tokens[at] == ")":
                at += 1
            if name == "all":
                return False if False in values else (True if all(v is True for v in values) else None)
            if name == "any":
                return True if True in values else (False if all(v is False for v in values) else None)
            if name == "not":
                return None if not values or values[0] is None else not values[0]
            return None
        if at < len(tokens) and tokens[at] == "=":
            if at + 1 >= len(tokens):
                return None
            value = tokens[at + 1].strip('"')
            at += 2
            known = PLATFORMS[platform].get(name)
            return None if known is None else known == value
        known = PLATFORMS[platform].get(name)
        return known if isinstance(known, bool) else None

    return parse()


def _raw_string_start(line, pos):
    """If a Rust raw string (`r#"`, `br#"`, `cr#"`) starts at `pos`, return `(hash_count, after_quote)`."""
    if line[pos] != "r":
        return None
    if pos > 0:
        prev = line[pos - 1]
        if prev in "bc" and (pos == 1 or not (line[pos - 2].isalnum() or line[pos - 2] == "_")):
            pass
        elif prev.isalnum() or prev == "_":
            return None
    idx = pos + 1
    while idx < len(line) and line[idx] == "#":
        idx += 1
    if idx < len(line) and line[idx] == '"':
        return idx - (pos + 1), idx + 1
    return None


def _strip_line_comment(line):
    """Strip a `//` comment from `line` without cutting through string or char literals."""
    in_string = False
    escaped = False
    raw_hashes = None
    i = 0
    n = len(line)
    while i < n:
        ch = line[i]
        if raw_hashes is not None:
            if ch == '"' and line.startswith("#" * raw_hashes, i + 1):
                i += 1 + raw_hashes
                raw_hashes = None
                continue
        elif in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
        else:
            raw = _raw_string_start(line, i)
            if raw is not None:
                raw_hashes, i = raw
                continue
            if ch == "'" and i + 2 < n and line[i + 1] == '"' and line[i + 2] == "'":
                i += 3
                continue
            if ch == '"':
                in_string = True
            elif ch == "/" and i + 1 < n and line[i + 1] == "/":
                return line[:i]
        i += 1
    return line


def _find_cfg_attrs(lines):
    """Yield `(is_inner, predicate, start_line, end_line, after_text)` for each `#![cfg(...)]` and `#[cfg(...)]`.

    Parses balanced parentheses across lines and ignores comments and string literals so multiple attributes on one
    line (`#[cfg(windows)] #[allow(dead_code)] fn f() {}`), multiline `#[cfg(...)]` blocks, and string literals
    containing `#[cfg(...)]` are each handled accurately.
    """
    n = len(lines)
    start_re = re.compile(r"#(!?)\[\s*cfg\s*\(")
    in_string = False
    raw_hashes = None
    block_comment = 0
    line_idx = 0
    while line_idx < n:
        line = lines[line_idx]
        pos = 0
        line_len = len(line)
        escaped = False
        while pos < line_len:
            ch = line[pos]
            if block_comment > 0:
                if ch == "/" and pos + 1 < line_len and line[pos + 1] == "*":
                    block_comment += 1
                    pos += 2
                elif ch == "*" and pos + 1 < line_len and line[pos + 1] == "/":
                    block_comment -= 1
                    pos += 2
                else:
                    pos += 1
                continue
            if raw_hashes is not None:
                if ch == '"' and line.startswith("#" * raw_hashes, pos + 1):
                    pos += 1 + raw_hashes
                    raw_hashes = None
                else:
                    pos += 1
                continue
            if in_string:
                if escaped:
                    escaped = False
                elif ch == "\\":
                    escaped = True
                elif ch == '"':
                    in_string = False
                pos += 1
                continue
            if ch == "/" and pos + 1 < line_len and line[pos + 1] == "/":
                break
            if ch == "/" and pos + 1 < line_len and line[pos + 1] == "*":
                block_comment = 1
                pos += 2
                continue
            raw = _raw_string_start(line, pos)
            if raw is not None:
                raw_hashes, pos = raw
                continue
            if ch == "'" and pos + 2 < line_len and line[pos + 1] == '"' and line[pos + 2] == "'":
                pos += 3
                continue
            if ch == '"':
                in_string = True
                pos += 1
                continue
            if ch == "#":
                match = start_re.match(line, pos)
                if match:
                    is_inner = match.group(1) == "!"
                    depth = 1
                    pred_in_string = False
                    pred_escaped = False
                    pred_parts = []
                    cur_line = line_idx
                    cur_code = _strip_line_comment(lines[cur_line])
                    idx = match.end()
                    found_end = None
                    while cur_line < n:
                        chunk_start = idx
                        while idx < len(cur_code):
                            c = cur_code[idx]
                            if pred_in_string:
                                if pred_escaped:
                                    pred_escaped = False
                                elif c == "\\":
                                    pred_escaped = True
                                elif c == '"':
                                    pred_in_string = False
                            else:
                                if c == '"':
                                    pred_in_string = True
                                elif c == "(":
                                    depth += 1
                                elif c == ")":
                                    depth -= 1
                                    if depth == 0:
                                        pred_parts.append(cur_code[chunk_start:idx])
                                        tail = cur_code[idx + 1:]
                                        close_match = re.match(r"\s*\]", tail)
                                        if close_match:
                                            after_idx = idx + 1 + close_match.end()
                                            found_end = (cur_line, after_idx, cur_code[after_idx:])
                                        else:
                                            found_end = (cur_line, idx + 1, "")
                                        break
                            idx += 1
                        if found_end is not None:
                            break
                        pred_parts.append(cur_code[chunk_start:])
                        cur_line += 1
                        if cur_line < n:
                            cur_code = _strip_line_comment(lines[cur_line])
                            idx = 0
                    if found_end is None:
                        return
                    end_line, after_idx, after_text = found_end
                    predicate = " ".join(pred_parts)
                    yield is_inner, predicate, line_idx, end_line, after_text
                    if end_line > line_idx:
                        line_idx = end_line
                        line = lines[line_idx]
                        line_len = len(line)
                    pos = after_idx
                    continue
            pos += 1
        line_idx += 1


def _strip_leading_attrs(code, attr_depth):
    """Strip any leading `#[...]` attributes (including multiline ones) from `code`, returning `(rest, attr_depth)`."""
    i = 0
    n = len(code)
    while True:
        if attr_depth > 0:
            while i < n and attr_depth > 0:
                if code[i] == "[":
                    attr_depth += 1
                elif code[i] == "]":
                    attr_depth -= 1
                i += 1
            if attr_depth > 0:
                return "", attr_depth
        while i < n and code[i].isspace():
            i += 1
        if i + 1 < n and code[i] == "#" and code[i + 1] == "[":
            attr_depth = 1
            i += 2
            continue
        return code[i:], attr_depth


def compiled_out_text(text, platform):
    """The 1-based line numbers of Rust source `text` behind a `#[cfg(...)]` that doesn't hold on `platform`."""
    lines = text.splitlines()
    out = set()
    attrs = list(_find_cfg_attrs(lines))
    if any(is_inner and holds(predicate, platform) is False for is_inner, predicate, _, _, _ in attrs):
        return set(range(1, len(lines) + 1))
    for is_inner, predicate, start_line, end_line, after_text in attrs:
        if is_inner or holds(predicate, platform) is not False:
            continue
        # The item the attribute is on: to its closing brace, to the `;` or `,` that ends a statement or a field first,
        # or to the brace closing what holds it
        depth, parens, opened, end = 0, 0, False, None
        attr_depth = 0
        rest = [after_text] + lines[end_line + 1:]
        for offset, raw in enumerate(rest):
            code = re.sub(r'"(\\.|[^"\\])*"', '""', _strip_line_comment(raw))
            if not opened and depth == 0 and parens == 0:
                code, attr_depth = _strip_leading_attrs(code, attr_depth)
                if not code:
                    continue
            for char in code:
                if char in "([":
                    parens += 1
                elif char in ")]":
                    parens -= 1
                elif char == "{":
                    depth, opened = depth + 1, True
                elif char == "}":
                    depth -= 1
                elif char in ";," and depth == 0 and parens == 0 and not opened:
                    end = offset
                    break
                if (opened and depth == 0) or depth < 0:
                    end = offset
                    break
            if end is not None:
                break
        last_line = end_line + (end if end is not None else len(rest) - 1)
        out.update(range(start_line + 1, last_line + 2))
    return out


def compiled_out(root, path, platform):
    """The lines of a source file behind a `#[cfg(...)]` that doesn't hold on `platform`"""
    return compiled_out_text((root / path).read_text(encoding="utf-8"), platform)


def lines_pattern(path, numbers):
    """A regex over cargo-mutants' names for the mutants on these lines of `path`"""
    return f"^{re.escape(path)}:({'|'.join(str(n) for n in sorted(numbers))}):"


def _profile_dirs(target):
    """Profile directories under a Cargo `target` directory that carry a `.cargo-lock` (up to depth 2)."""
    found = []
    if not target.is_dir() or target.is_symlink():
        return found

    def visit(directory, depth):
        lock = directory / ".cargo-lock"
        if not lock.is_symlink() and lock.is_file():
            found.append(directory)
        if depth == 0:
            return
        try:
            children = list(directory.iterdir())
        except OSError:
            return
        for child in children:
            if child.is_dir() and not child.is_symlink():
                visit(child, depth - 1)

    visit(target, 2)
    return found


def _newest_in_dir(directory, newest):
    """Newest mtime of `directory` and its direct children (shallow, without following symlinks)."""
    try:
        newest = max(newest, directory.lstat().st_mtime)
        children = list(directory.iterdir())
    except OSError:
        return newest
    for child in children:
        try:
            newest = max(newest, child.lstat().st_mtime)
        except OSError:
            pass
    return newest


def _copy_activity_mtime(copy_dir):
    """Newest modification time across `copy_dir`, its `target/` layout, and per-crate incremental/fingerprint dirs."""
    newest = copy_dir.lstat().st_mtime
    target = copy_dir / "target"
    if target.is_dir() and not target.is_symlink():
        newest = _newest_in_dir(target, newest)
        for profile in _profile_dirs(target):
            try:
                newest = max(newest, profile.lstat().st_mtime)
            except OSError:
                pass
            for sub in ("deps", "build"):
                try:
                    newest = max(newest, (profile / sub).lstat().st_mtime)
                except OSError:
                    pass
            # Rebuilding an existing crate mutates entries inside `<crate>-<hash>/` without updating its parent dir's mtime
            for sub in (".fingerprint", "incremental"):
                newest = _newest_in_dir(profile / sub, newest)
    return newest


def _rmtree_force(entry):
    """Remove `entry`, relaxing read-only permissions inside it if a permission error blocks deletion."""
    try:
        shutil.rmtree(entry)
        return
    except PermissionError:
        pass
    stack = [entry]
    while stack:
        current = stack.pop()
        try:
            if not current.is_symlink():
                os.chmod(current, current.lstat().st_mode | 0o700)
        except OSError:
            pass
        try:
            children = list(current.iterdir())
        except OSError:
            continue
        for child in children:
            if child.is_symlink():
                continue
            try:
                os.chmod(child, child.lstat().st_mode | 0o700)
            except OSError:
                pass
            if child.is_dir():
                stack.append(child)
    shutil.rmtree(entry)


def _ps_has_cargo_mutants(ps_output, self_pid=None):
    """Whether `ps -A -o pid=,comm=` output lists a running `cargo-mutants` process."""
    for line in ps_output.splitlines():
        parts = line.strip().split(None, 1)
        if len(parts) != 2:
            continue
        pid_text, comm = parts
        if self_pid is not None and pid_text.isdigit() and int(pid_text) == self_pid:
            continue
        if PurePosixPath(comm.strip()).name == "cargo-mutants":
            return True
    return False


def cargo_mutants_running():
    """Whether any `cargo-mutants` process is running on this host (fails closed as True if `ps` fails)."""
    try:
        probe = subprocess.run(
            ["ps", "-A", "-o", "pid=,comm="],
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return True
    if probe.returncode != 0:
        return True
    return _ps_has_cargo_mutants(probe.stdout, self_pid=os.getpid())


def sweep_orphaned_copies(tmp_root, now=None, min_age_secs=IN_USE_SECS, is_running=cargo_mutants_running):
    """Remove orphaned `cargo-mutants-*` temp copies idle past `min_age_secs` whose `.cargo-lock`s are unheld.

    Skips sweeping entirely while any `cargo-mutants` process is running (Cargo releases `.cargo-lock` once tests
    start, and a machine sleep can push a live run past `min_age_secs`). Holds every nested `.cargo-lock` exclusively
    across removal on Unix as defense-in-depth. Returns the paths removed.
    """
    if fcntl is None or is_running():
        return []
    now = time.time() if now is None else now
    removed = []
    open_flags = os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0)
    try:
        entries = list(Path(tmp_root).iterdir())
    except OSError:
        return removed
    for entry in entries:
        if not entry.name.startswith("cargo-mutants-"):
            continue
        if entry.is_symlink() or not entry.is_dir():
            continue
        try:
            activity = _copy_activity_mtime(entry)
        except OSError:
            continue
        if now - activity <= min_age_secs:
            continue
        locks = [profile / ".cargo-lock" for profile in _profile_dirs(entry / "target")]
        fds = []
        busy = False
        try:
            for lock in locks:
                try:
                    # BSD flock accepts O_RDONLY on both Linux and macOS without creating or modifying the lock file
                    fd = os.open(lock, open_flags)
                except FileNotFoundError:
                    continue
                except OSError:
                    busy = True
                    break
                fds.append(fd)
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except OSError:
                    busy = True
                    break
            if busy:
                continue
            _rmtree_force(entry)
            removed.append(entry)
        except OSError as error:
            print(f"Warning: could not sweep orphaned {entry}: {error}", file=sys.stderr)
            continue
        finally:
            for fd in fds:
                try:
                    os.close(fd)
                except OSError:
                    pass
    return removed


def _glob_matches(path, pattern):
    """Match a POSIX relative `path` against a globset-style `pattern` (`*` stays within one segment, `**/` crosses any)."""
    parts = ["^"]
    at = 0
    length = len(pattern)
    while at < length:
        if pattern.startswith("**/", at):
            parts.append("(?:[^/]+/)*")
            at += 3
        elif pattern.startswith("**", at):
            parts.append(".*")
            at += 2
        elif pattern[at] == "*":
            parts.append("[^/]*")
            at += 1
        elif pattern[at] == "?":
            parts.append("[^/]")
            at += 1
        elif pattern[at] == "[":
            end = at + 1
            if end < length and pattern[end] in "!^":
                end += 1
            if end < length and pattern[end] == "]":
                end += 1
            end = pattern.find("]", end)
            if end == -1:
                parts.append(re.escape(pattern[at]))
                at += 1
            else:
                body = pattern[at + 1:end]
                if body.startswith("!"):
                    body = "^" + body[1:]
                parts.append(f"[{body}]")
                at = end + 1
        else:
            parts.append(re.escape(pattern[at]))
            at += 1
    parts.append("$")
    return re.match("".join(parts), path) is not None


def _owner_crate(file_path, crates_by_specificity):
    """Return the owning crate name for repo-relative `file_path`, matching the longest crate path first."""
    for name, path, _ in crates_by_specificity:
        if file_path.startswith(f"{path}/"):
            return name
    return None


def select_crate_files(crates, changed_rust, scope, file_filters, root=None):
    """Map each workspace crate name to the `--file` arguments to hand to cargo-mutants.

    Without `file_filters`, selects every crate that has changed `.rs` files in `changed_rust` (or in `scope` under
    `--windows-only`). With `file_filters`, matches each path or glob (including cross-crate globs) against
    `changed_rust`, refusing any filter that matches no changed workspace Rust file, and routes matched files to their
    owning crate (intersected with `scope` under `--windows-only`).
    """
    by_crate = {name: [] for name, _, _ in crates}
    # Match longest crate path first so nested crates win over an enclosing one
    crates_by_specificity = sorted(crates, key=lambda item: len(item[1]), reverse=True)
    crate_paths = {path for _, path, _ in crates}

    if not file_filters:
        if scope is not None:
            for file in scope:
                owner = _owner_crate(file, crates_by_specificity)
                if owner is not None and file not in by_crate[owner]:
                    by_crate[owner].append(file)
            return by_crate
        touched_crates = {
            owner
            for file in changed_rust
            if (owner := _owner_crate(file, crates_by_specificity)) is not None
        }
        return {name: ([f"{path}/**"] if name in touched_crates else []) for name, path, _ in crates}

    workspace_rust = [
        (file, owner)
        for file in changed_rust
        if (owner := _owner_crate(file, crates_by_specificity)) is not None
    ]
    scope_set = None if scope is None else set(scope)
    for raw in file_filters:
        raw_posix = raw.replace("\\", "/")
        had_trailing_slash = raw_posix.endswith("/")
        pure = PurePosixPath(raw_posix)
        pattern = pure.as_posix()
        if not pattern or pattern == "." or pure.is_absolute() or ".." in pure.parts:
            raise ValueError(f"--file {raw!r} must be a repo-relative path or glob inside the workspace")
        is_glob = any(ch in pattern for ch in "*?[")
        if pattern in crate_paths or had_trailing_slash or (not is_glob and root is not None and (Path(root) / pattern).is_dir()):
            pattern = f"{pattern}/**"
        matched = [(file, owner) for file, owner in workspace_rust if _glob_matches(file, pattern)]
        if not matched:
            raise ValueError(f"--file {raw!r} matches no workspace Rust file changed in the diff")
        for file, owner in matched:
            if scope_set is not None and file not in scope_set:
                continue
            if file not in by_crate[owner]:
                by_crate[owner].append(file)
    return by_crate


def validate_test_filter_scope(crate_files, test_filter):
    """Refuse `--test-filter` when multiple crates are in scope, since one nextest expression cannot fit every crate."""
    if test_filter is None:
        return
    active_crates = [name for name, files in crate_files.items() if files]
    if len(active_crates) > 1:
        joined = ", ".join(active_crates)
        raise ValueError(
            f"--test-filter applies to a single crate, but this selection spans multiple crates ({joined}): "
            "pass --file to narrow to one crate"
        )


def build_mutants_command(jobs, out_dir, selection, chosen, crate_name, with_features, test_filter=None):
    """Assemble the `cargo mutants` argv for one crate."""
    suite_args = ["--", "-E", test_filter] if test_filter is not None else SUITE_ARGS.get(crate_name, [])
    test_packages = ["--test-package", crate_name]
    for extra_pkg in LAYERED_TEST_PACKAGES.get(crate_name, ()):
        test_packages.extend(["--test-package", extra_pkg])
    return [
        "cargo", "mutants", "-j", jobs, "-o", str(out_dir),
        *selection, *chosen,
        *test_packages, *with_features, *suite_args,
    ]


def format_scope_summary(args):
    """Describe what a run mutated for console output and the GitHub Actions step summary."""
    parts = ["Windows-only code" if args.windows_only else "the change"]
    if args.shard:
        parts.append(f"shard {args.shard}")
    if args.files:
        parts.append(f"files: {', '.join(args.files)}")
    if args.patterns:
        parts.append(f"re: {', '.join(args.patterns)}")
    if args.test_filter is not None:
        parts.append(f"test-filter: {args.test_filter}")
    return ", ".join(parts)


def lines_of(path):
    return path.read_text().splitlines() if path.exists() else []


def parse_args(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("base")
    parser.add_argument("jobs", nargs="?", default="3")
    parser.add_argument("--shard", help="K/N: the Kth of N shares of the mutants")
    parser.add_argument("--windows-only", action="store_true")
    parser.add_argument("--resume", action="store_true", help="leave out what an earlier run of the same change caught")
    parser.add_argument("--file", dest="files", action="append", default=[],
                        help="restrict mutation to matching repo-relative path(s) or glob(s) in the diff")
    parser.add_argument("--re", dest="patterns", action="append", default=[],
                        help="pass --re PATTERN to cargo-mutants (cannot be combined with --windows-only)")
    parser.add_argument("--test-filter", help="cargo-nextest -E filter expression to run instead of the default suite filter")
    args = parser.parse_args(argv)
    if args.windows_only and args.patterns:
        parser.error("--re cannot be combined with --windows-only (cargo-mutants ORs multiple --re filters)")
    if any(not item.strip() for item in args.files):
        parser.error("--file must not be empty")
    if any(not item.strip() for item in args.patterns):
        parser.error("--re must not be empty")
    if args.test_filter is not None and not args.test_filter.strip():
        parser.error("--test-filter must not be empty")
    return args


def main(argv=None):
    args = parse_args(argv)

    root = Path.cwd()
    out = root / "target/mutants-in-diff"
    out.mkdir(parents=True, exist_ok=True)
    diff = out / "branch.diff"
    change = subprocess.check_output(["git", "diff", f"{args.base}...HEAD"])
    # What an earlier run caught holds only for the diff it was caught in
    same_change = diff.exists() and diff.read_bytes() == change
    if not same_change:
        # Forgotten before the diff is written: a run of the new diff that is stopped before it has kept anything would
        # otherwise find the diff its own on a resume, and the last diff's catches beside it
        for kept in out.glob("*/caught.txt"):
            kept.unlink()
        for results in out.glob("*/mutants.out"):
            shutil.rmtree(results, ignore_errors=True)
    diff.write_bytes(change)
    changed = subprocess.check_output(["git", "diff", "--name-only", f"{args.base}...HEAD"], text=True).split()

    rust = [path for path in changed if path.endswith(".rs") and (root / path).exists()]
    if args.windows_only:
        # Only what compiles on Windows and not on Linux
        only_windows = {path: compiled_out(root, path, "linux") - compiled_out(root, path, "windows") for path in rust}
        only_windows = {path: numbers for path, numbers in only_windows.items() if numbers}
        scope = sorted(only_windows)
        platform_args = [arg for path, numbers in only_windows.items() for arg in ("--re", lines_pattern(path, numbers))]
    else:
        scope = None
        elsewhere = {path: numbers for path in rust if (numbers := compiled_out(root, path, "linux"))}
        platform_args = [arg for path, numbers in elsewhere.items() for arg in ("--exclude-re", lines_pattern(path, numbers))]

    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1"]))
    crates = sorted((p["name"], Path(p["manifest_path"]).parent.relative_to(root).as_posix(), p["features"])
                    for p in metadata["packages"])
    assert crates, "the workspace names no crate"

    try:
        crate_files = select_crate_files(crates, rust, scope, args.files, root=root)
        validate_test_filter_scope(crate_files, args.test_filter)
    except ValueError as error:
        sys.exit(str(error))
    has_work = any(crate_files.values())

    if has_work and not os.environ.get("CI"):
        tmp_dir = tempfile.gettempdir()
        swept = sweep_orphaned_copies(Path(tmp_dir))
        if swept:
            print(f"Swept {len(swept)} orphaned cargo-mutants temp director{'y' if len(swept) == 1 else 'ies'} in {tmp_dir}")
        free = shutil.disk_usage(tmp_dir).free
        if free < FLOOR:
            sys.exit(f"{tmp_dir} has {free / (1 << 30):.1f} GB free, less than the {FLOOR >> 30} GB a mutation "
                     "run's copies build in: free space first, `cargo xtask sweep` among the ways")

    # cargo-mutants builds each job's copy of the tree in a directory of its own; a CARGO_TARGET_DIR from the caller would
    # put every copy's build in one, where a job can run another's test binary and report a caught mutant as missed
    env = {key: value for key, value in os.environ.items() if key != "CARGO_TARGET_DIR"}
    # The committed configuration with the exclusions the source's `// mutant-skip:` comments make, which are in no file
    # cargo-mutants reads by itself; the task prints where it wrote it. A change with nothing in scope runs nothing below,
    # and doesn't build the task to find that out
    configuration = [] if not has_work else [
        "--config", subprocess.check_output(["cargo", "xtask", "mutant-exclusions"], text=True).strip()]
    sharding = ["--shard", args.shard] if args.shard else []
    re_args = [arg for pattern in args.patterns for arg in ("--re", pattern)]

    missed, timeouts, damaged = [], [], []
    for name, _, features in crates:
        files = crate_files[name]
        if not files:
            continue
        # A crate's optional diagnostics/failpoints code is compiled only with its feature enabled, so
        # mutants of those crates build and test with the feature(s) turned on.
        feat_args = []
        if "diagnostics" in features:
            feat_args.append(f"{name}/diagnostics")
        if "failpoints" in features:
            feat_args.append("failpoints")
        with_features = ["--features", ",".join(feat_args)] if feat_args else []
        results = out / name / "mutants.out"
        # Every mutant a run of this diff caught, the last run's among them, which a run that was stopped never added
        kept = out / name / "caught.txt"
        caught = set(lines_of(kept) + lines_of(results / "caught.txt")) if same_change else set()
        selection = [*configuration, "--in-diff", str(diff), *(arg for file in files for arg in ("--file", file))]
        chosen = [*sharding, *platform_args, *re_args]
        if args.resume and caught:
            # The mutants this run would test, less those caught: named one by one, since a share of what is left is not
            # what is left of a share
            listed = subprocess.check_output(["cargo", "mutants", "--list", *selection, *chosen], env=env, text=True)
            left = [mutant for mutant in listed.splitlines() if mutant not in caught]
            print(f"{name}: {len(left)} mutants left of {len(listed.splitlines())}")
            if not left:
                continue
            chosen = [arg for mutant in left for arg in ("--re", f"^{re.escape(mutant)}$")]
        # A change that holds no mutant of the crate has cargo-mutants write no results, and those an earlier run left
        # would be read as this one's: what that run caught is already taken, above
        shutil.rmtree(results, ignore_errors=True)
        cmd = build_mutants_command(args.jobs, out / name, selection, chosen, name, with_features, args.test_filter)
        run = subprocess.run(cmd, env=env)
        kept.parent.mkdir(parents=True, exist_ok=True)
        kept.write_text("".join(f"{mutant}\n" for mutant in sorted(caught | set(lines_of(results / "caught.txt")))))
        if run.returncode not in TESTED:
            sys.exit(f"cargo-mutants failed for {name} with exit code {run.returncode}")
        for found, file in ((missed, "missed.txt"), (timeouts, "timeout.txt")):
            found += lines_of(results / file)
        if (results / "outcomes.json").exists():
            for outcome in json.loads((results / "outcomes.json").read_text())["outcomes"]:
                log = results / outcome.get("log_path", "")
                if outcome["summary"] == "Unviable" and log.is_file() and DAMAGED.search(log.read_text(errors="replace")):
                    damaged.append(outcome["scenario"]["Mutant"]["name"])

    what = format_scope_summary(args)
    if not has_work and args.windows_only:
        print("No Windows-only code matched the --file filter" if args.files
              else "The change holds no Windows-only code to mutate")
    print(f"{len(missed)} missed, {len(timeouts)} timeouts ({what})")
    for line in missed:
        print(f"MISSED  {line}")
    for line in timeouts:
        print(f"TIMEOUT {line}")
    for line in damaged:
        print(f"UNTESTED {line}")
    if damaged:
        print(f"{len(damaged)} mutants were never tested: the tree they were built in lacked a file. Run again with --resume")

    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        for line in missed:
            file, number, _ = line.split(":", 2)
            print(f"::error file={file},line={number},title=Missed mutant::{line}")
        with open(summary, "a", encoding="utf-8") as written:
            written.write(f"## Mutants in {what}\n\n{len(missed)} missed, {len(timeouts)} timeouts\n\n")
            for heading, lines in (("Missed", missed), ("Timeouts", timeouts)):
                if lines:
                    written.write(f"### {heading}\n\n" + "".join(f"- `{line}`\n" for line in lines) + "\n")

    # A survivor fails the run: kill it with a test, or name it in a `// mutant-skip:` with why it is equivalent
    sys.exit(1 if missed or damaged else 0)


if __name__ == "__main__":
    main()
