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
