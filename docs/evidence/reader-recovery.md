# Recovering reader slots left by dead processes

2026-10-30, `session/reader-recovery`, based on `origin/main` at `e5f6953`. The change uses heed's
`clear_stale_readers` only after `READERS_FULL`, then retries acquisition once. LMDB owns the process
liveness check. It neither evicts live readers nor changes snapshot retention or reader capacity.

The process regression fills every reader slot in a child, observes a live-reader refusal in the
parent, kills/reaps the child without running reader destructors, then reads the committed value.
The opening regression keeps the environment mapped in the parent while the saturating child dies,
then opens read-only and reads in a separate child, so first-opener table reset cannot explain recovery.
Children report explicit barriers, are killed/reaped on parent unwind, and the holding child has its
own finite lifetime. A focused check verifies exactly one retry and no retry for unrelated errors.

Validation in `scripts/linux.py`'s Linux arm64 container, two CPUs, 6 GiB RAM/no additional swap:

- `cargo fmt --all --check` and workspace/all-target clippy with warnings denied: passed.
- `cargo nextest run --workspace`: 357 passed, 11 skipped (child halves/manual measurements).
- After strengthening the opening case to use a separate opener and bounding the holding child's
  lifetime: the three reader-recovery cases passed again; production code was unchanged.
- Workspace doc tests, generated-contract check and single-LMDB dependency check: passed.
- `scripts/check-reader-recovery.py`: all three disposable mutations rejected (omit cleanup, retry
  an extra time, retry an unrelated error). The script never mutates the checkout.
- Release binding build, TypeScript checks and binding suite on Node 22.17.0: passed (33 passed,
  four skipped, zero failures). Generated declarations/loader are not changed by this PR.

The Linux runner initially refused a later invocation below its 8 GiB free-space floor. Removing only
this session's disposable incremental compilation cache restored the floor; subsequent compilation
used `CARGO_INCREMENTAL=0`. The floor and assertions were unchanged. Cross-platform acceptance is
owned by the PR's existing CI matrix; local Linux results do not establish Windows/macOS behavior.

## Windows process liveness

LMDB's Windows liveness check (`mdb_reader_pid` in lmdb-master-sys 0.2.6's `mdb.c`) opens the reader's PID
with `PROCESS_QUERY_LIMITED_INFORMATION` and then waits on the handle. That access right does not include
`SYNCHRONIZE`, so the wait fails and the failure counts as live. A reader slot is therefore reclaimed on
Windows only once `OpenProcess` reports `ERROR_INVALID_PARAMETER`: the process object is gone, which needs
every handle to it in any process closed and can lag the termination. Until then the slot stays protected,
and a read still refuses with `READERS_FULL`.

Windows CI's first run refused recovery in both process cases. Closing the parent's `Child` handle
(`902c2e3`) was not sufficient: the separate-opener case most likely passed because spawning its opener took
long enough for the process object to go, and the immediate read case still failed. The Windows test now
establishes the precondition LMDB actually checks: after kill, wait and handle close, it waits, bounded
at ten seconds, until `OpenProcess` reports `ERROR_INVALID_PARAMETER` for the PID, reporting the exit
code and any query error on failure. It never clears slots itself; production code still performs one
LMDB cleanup and one retry, without polling. Windows platform tests and the Windows mutation job passed on
`6493c96` (run 41014051098), with the wait then bounded at two seconds.

## Replacement sources

`replace_with` opened its source outside Store and bypassed both recovery and
reader registration. Its source now uses the shared helpers. The actual Linux read-only-mount case
also attempts replacement, requires IO before committing, and checks the destination sequence is
unchanged. The mount verification script independently removes each registration check on disposable
copies, so source registration cannot pass merely because normal open is still protected.
