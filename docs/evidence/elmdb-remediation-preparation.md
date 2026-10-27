# elmdb remediation preparation

Date: 2026-10-26. Preparation only; no remediation implementation applied.

## Inputs and revalidation

elmdb HEAD: `e74f9dd981f51fc803186d7675d880745c5310a9` on `session/elmdb-next`.
AgentBuddy master: `7ac1d99fb24649a7ce3e708035bae07cde45ee78`.
The elmdb diff from reviewed implementation `f7bc8a584841f5b3bac2621de7e32088748f46f9` to HEAD contains no changes
under crates/, Cargo.toml or Cargo.lock. Review R01–R08 therefore retain their implementation evidence. The eight
reviewed entry-point files were also compared byte-for-byte with the new isolated elmdb checkout.

Source checkouts were clean when inspected. No implementation, staging index, build output or vendor link in them was
changed by preparation. New worktrees use separate branch refs in each repository:

- elmdb: `/Users/spankyed/Develop/Projects/apack/elmdb-worktrees.noindex/elmdb-remediation`, branch
  `session/elmdb-remediation`, base `e74f9dd`.
- AgentBuddy: `/Users/spankyed/Develop/Projects/apack/monorepo-worktrees.noindex/elmdb-remediation`, branch
  `session/elmdb-remediation`, base `7ac1d99fb2`.

The user explicitly requested separate branches/worktrees and copying the roughly 11 GB target directory to avoid
interfering with existing builds. target/ is elmdb's Rust output; AgentBuddy has no root target/ directory.

## Binding registration investigation

The current guard mutates `ELMDB_BINDING` and has a documented first-open race. A per-binary static does not synchronize
independent binaries. Node's environment data is scoped to an environment; it is not a process-wide worker registry.
[Node-API environment lifecycle](https://nodejs.org/api/n-api.html#environment-life-cycle-apis).

The existing pinned Rust toolchain supplies `std::fs::File::try_lock` (available since Rust 1.89). Its documented
exclusive lock excludes independently opened handles in this process as well as other processes. It currently uses
flock on Unix and LockFileEx on Windows. This avoids a new locking dependency and unsafe environment mutation.
[Rust File locking](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock).

Candidate: hold one nonblocking exclusive file lock for the winning binary's process lifetime, with per-binary one-time
initialization. Each independently loaded binary independently opens the same per-process lock path. A losing claim
returns ALREADY_LOADED; actual I/O failures refuse with an actionable code. Do not unlink a live claim or turn failed
registration into a permissive fallback. The complete namespace, metadata, lifecycle and platform behavior remain
Phase 1 implementation requirements, not proven by this spike.

Alternatives considered: environment data is both unsafe to mutate and racy; JavaScript/N-API globals do not span
workers; separate named OS objects require more platform-specific handling than the standard library candidate.
Retain the OS-specific option only if platform validation disproves the standard-library approach.

A one-run mechanism probe on macOS 26.3.1 (25D771280a), Rust 1.99.1, created a fresh scratch file and opened two independent
read/write handles. First try_lock succeeded; the second returned WouldBlock; dropping the first allowed the second to
lock. The scratch file was removed. Command: `rustc /private/tmp/elmdb-registration-probe.rs -o
/private/tmp/elmdb-registration-probe`, then that executable. Source:

```rust
use std::fs::OpenOptions;
fn main() {
    let path = std::env::temp_dir().join(format!("elmdb-registration-probe-{}", std::process::id()));
    let a = OpenOptions::new().read(true).write(true).create_new(true).open(&path).unwrap();
    let b = OpenOptions::new().read(true).write(true).open(&path).unwrap();
    a.try_lock().unwrap();
    assert!(matches!(b.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
    drop(a);
    b.try_lock().unwrap();
    drop(b);
    std::fs::remove_file(path).unwrap();
}
```

This is a mechanism check, not a dual-addon concurrency test or a Windows/Linux result. The implementation phase must
prove two physical addon copies, workers, child processes, retries, errors and teardown behavior.

## AgentBuddy integration trace

At the pinned AgentBuddy source:

- `packages/apack-ears/src/database.ts`, transact: a NOT_DURABLE unit commit invokes committed listeners with all
  tracked tokens invalidated, then rethrows. This already recognizes that the mutation happened.
- The same file's load calls bulkLoad and invokes committed listeners only after success; a committed error skips it.
- `packages/apack-ears/src/engine.ts`, admin.clear and replaceWith: committedEverything runs only after success.
- admin.setSchema and declare: both refresh declaredNames(database.schema) and invoke committedEverything only after
  success. Even after fixing Rust's schema finalization, these JavaScript caches and notifications need reconciliation
  on a committed error.
- `packages/apack-host/src/backup/index.ts`: databaseBackupStore can call EARS admin.replaceWith or a raw database's
  replaceWith. Its multi-database restore/rollback path must be tested against the final committed-error contract.
- `packages/apack-ears/tests/engine/watch.spec.ts` already has a durable-unit failure case using a proxy database.
  Extend the same externally observable pattern for admin/schema/load paths; do not replace native failure injection.
- `packages/apack-ears/CLAUDE.md` currently promises lockless read-only access on permission failure. Update this with
  R02's actual refusal behavior. Keep schema/error contracts generated from Rust when changing exported data.

The expected application edit set is EARS database.ts, engine.ts, focused watch/admin/schema tests, persistence docs,
possibly host backup tests, generated API reports if exports change, and vendor/elmdb.rev at the accepted checkpoint.
No pack feature, UI, compiler runtime, live application data, or version metadata change is currently justified.

## Build isolation and setup

Machine/filesystem observations, 2026-10-26: macOS 26.3.1, APFS; df reported roughly 24 GiB free before setup and 22 GiB
following dependency installation. du reported elmdb target/ as 11 GB, elmdb tooling dependencies as 111 MB, and source
AgentBuddy dependencies as 2.5 GB. These are setup observations, not performance comparisons.

Actions performed:

1. Explicit git worktree add created both branches at the exact bases above, following the repository's documented
   explicit-worktree alternative. No uncommitted work was transferred.
2. `cp -cR` copied elmdb target/ and tooling dependencies into its worktree; `cp -c` copied the binding as a bootstrap
   artifact. APFS copies have independent file ownership while sharing unchanged extents. No hardlinks or build-output
   symlinks were used. Sample libelmdb.rlib and libelmdb_node.dylib had distinct inodes and equal sizes.
3. With ELMDB_DIR pointing to the isolated elmdb checkout, ran AgentBuddy scripts/link-elmdb.mjs in the new application
   worktree. Both vendor/elmdb and npm resolution from packages/apack-ears point into the isolated checkout.
4. `npm ci --offline --no-audit --no-fund` in the new AgentBuddy worktree passed: 1,348 packages installed. npm emitted
   deprecation notices and an engine warning for eslint-visitor-keys@5.0.1 on Node 23.11.1. No dependency versions were
   changed. node_modules is owned by that worktree.
5. `npm run build` in the isolated elmdb worktree passed, compiling both elmdb-node and elmdb in release mode.
   Generated index.js/index.d.ts remained unchanged. The new binary is built from the isolated source, not merely copied.

No app or database workload was launched. Full AgentBuddy packages/app build and final integrated suites remain part of
the implementation baseline and acceptance. Use worktree-default profiles; explicitly isolate any manual app launch.
Do not point AgentBuddy builds at the default elmdb checkout: the file dependency is writable through its symlink.

## Baseline verification

The isolated elmdb binding was rebuilt before verification. `npm run typecheck` passed; `npm test` passed all 29 tests
with zero skipped or failed. Output is preserved locally at `/private/tmp/elmdb-remediation-node-baseline.log`.
No loader/declaration diff was produced. These checks establish the prepared binding baseline; they do not close any
review finding or substitute for final Rust, application, or platform acceptance. Both remediation worktrees were clean
before the two preparation documents were installed; elmdb's source checkout remained clean at e74f9dd.

## Current execution and sequencing

Implementation was subsequently authorized; Phase 0/1 results are in
[implementation evidence](elmdb-remediation.md). The preparation results above remain dated evidence.

On 2026-10-26 the user chose serial execution: goal-elmdb-next stays paused before Phase 3 while remediation completes
against current upstream main/master. Remediation may commit and push its own branches and bring in main/master;
it must not merge out. The user reviews and separately authorizes landing. Only afterward may the user resume
goal-elmdb-next, whose branches incorporate upstream and whose acceptance owns its future features' compatibility.
Remediation completion does not depend on those future features or on its branches being landed.

The goal records implementation and acceptance:
[completed remediation goal](../archive/goals/goal-elmdb-remediation.md). The original assessment remains
[elmdb-review-2026-10.md](../reviews/elmdb-review-2026-10.md).

Implementation and final acceptance completed on 2026-10-27; the archived goal records the exact validated pair,
terminal CI results and Outcome. These preparation observations remain the dated starting point.
