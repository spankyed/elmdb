# elmdb remediation evidence

## Phase 0 and Phase 1 — 2026-10-26

Inputs: elmdb remediation at `ae88ec85fb629f71dffc778196814b8c2c098aef`, monorepo remediation at
`5574f84ceaee30392e3430b8a4981f0ba55dd9d9`. All edits/builds ran in the isolated worktrees named by the goal.
Machine: macOS 26.3.1 arm64, Rust 1.99.1, Node 23.11.1. No performance comparison is claimed.

### Implementation

R01: `crates/elmdb-node/src/process.rs` takes `File::try_lock` on an independently opened claim file, initialized once
per physical binary. The static owner retains the handle across database close and worker teardown. Unix names include
real UID and PID in `/tmp`, independent of a worker's TMPDIR. Windows uses its native process temporary directory and
PID. Stale unlocked files can be reused; live claim files are never unlinked/replaced. A rejected copy returns
ALREADY_LOADED; an I/O failure returns IO before any database is created. No environment mutation or poll loop is used.
libc supplies only Unix getuid; its call's safety invariant is documented. No new dependency version was downloaded.

R02: removed the permission-denied NO_LOCK fallback. A read-only permission failure explains reader registration and
which file access to grant, or to use a writable copy. Tests cover read-only files and a child reader beside an open
writer whose existing lock handle remains usable. Elevated users may legitimately retain write access; assertions check
that opening is permitted only when reader registration remains possible.

Updated engine opening documentation and the EARS persistence guidance. No exported protocol or loader/declaration
changes are expected. The EARS implementation remains unchanged in this phase.

### Local verification

- Phase 0: isolated `npm run build:package -w @apackdev/ears` passed. Focused EARS watch baseline passed 14 tests.
  Logs: `/private/tmp/elmdb-remediation-ears-build.log`, `/private/tmp/elmdb-remediation-ears-watch-baseline.log`.
  The watch baseline ran against the prepared binding before the final Phase 1 rebuild; it is baseline evidence only.
- `cargo test --offline -p elmdb-node process::tests`: two passed, including independent handles and I/O refusal.
  Final log: `/private/tmp/elmdb-remediation-phase1-claim-final.log`.
- `node --test crates/elmdb-node/test/process.test.mjs`: six passed. Cases cover a rejected second image, independent
  child processes, unchanged environment, simultaneous worker first opens, distinct worker TMPDIR settings, a claim
  surviving its first worker, repeated rejection, and failure before database creation. The same cases are included in
  the final full Node suite.
- `cargo test --offline -p elmdb a_read_only_`: four passed after the final diagnostic change.
  Log: `/private/tmp/elmdb-remediation-phase1-readonly-final.log`.
- `cargo clippy --offline --workspace --all-targets -- -D warnings`: passed on the final user-scoped claim source.
  Log: `/private/tmp/elmdb-remediation-phase1-clippy-final.log`.
- `npm run build`: final user-scoped binding rebuild passed. `npm run typecheck` passed, and `npm test` passed all 33
  tests with none failed/skipped. Logs: `/private/tmp/elmdb-remediation-phase1-uid-build.log`,
  `/private/tmp/elmdb-remediation-phase1-types.log`, `/private/tmp/elmdb-remediation-phase1-node.log`.
- Final `cargo fmt --all --check`, `cargo test --offline --workspace`, `cargo xtask check-generated` and
  `cargo xtask check-one-lmdb` passed. Workspace results: elmdb 212 passed/one measurement ignored, CLI one passed,
  binding Rust tests two passed, xtask two passed. Logs: `/private/tmp/elmdb-remediation-phase1-rust-final.log`,
  `/private/tmp/elmdb-remediation-phase1-generated-final.log`, `/private/tmp/elmdb-remediation-phase1-one-lmdb-final.log`.
  Generated loader/declarations are unchanged.

### Demonstrated firing cases

Only disposable copies were used. The environment-safety regression was run against a copied baseline binding and
failed with actual `mutated`, expected `unchanged` (exit 1). Log:
`/private/tmp/elmdb-remediation-process-mutation.log`.

The active-writer reader-registration regression was installed in the earlier disposable review source, retaining the
old NO_LOCK fallback. It failed in the child with `a reader without lock-file access must not open beside a writer`
(exit 101). Log: `/private/tmp/elmdb-remediation-readonly-mutation.log`. Both regressions pass on the correction.

### Remaining verification

R01/R02 are implemented and locally verified, not yet closed across supported platforms. The binding matrix must
verify the kernel-lock behavior on Linux glibc/musl and Windows as well as both macOS architectures. There was no
simulated cross-platform result. Phase 2 and all later findings remain outstanding. Final remediation acceptance uses the currently implemented upstream main/master inputs under D11; no feature-branch
merge or remediation merge-out is allowed. It does not wait for future goal-elmdb-next features.

### Checkpoint and remote verification

Local Phase 1 checkpoint: `2414390` (`fix: preserve process exclusion and reader registration`). The requested
`git push -u origin session/elmdb-remediation` to https://github.com/spankyed/elmdb was rejected by automatic approval
review before execution: the external destination and exact branch were not established as explicitly authorized by
trusted user content. A user approval request naming both repository destinations, both remediation branches, and
required CI is pending. No alternate transport, push, workflow dispatch, merge or release was attempted.
Local implementation continues independently; platform-matrix verification remains pending.

The isolated monorepo then ran the complete EARS package suite against the final Phase 1 binding:
`ELMDB_DIR=<isolated-elmdb> npm test -w @apackdev/ears` passed 12 files / 189 tests, including the model and watch
properties. Log: `/private/tmp/elmdb-remediation-phase1-ears.log`. Monorepo checkpoint `a68ee6999b` updates its elmdb pin
to `241439064550cbee34d44510a0dc15a697558952` and corrects the persistence guidance. It remains local while push approval
is pending. There were no EARS implementation changes or generated API changes in that checkpoint.


## Sequencing decision — 2026-10-26

The original remediation Phase 6 required the completed goal-elmdb-next feature set, while that work was paused for
remediation. This created a circular dependency. The user selected remediation-first execution: complete remediation
against currently implemented upstream, commit/push the branches for review, and do not merge them. Landing is a
separate user-authorized step. goal-elmdb-next stays paused before Phase 3 until the user resumes it after landing.

Future-feature integration assertions are retained in goal-elmdb-next's acceptance: planner ordering/limits/tracking,
field-type auditing, compressed read/export/recovery paths, and trigram/regex correctness. This transfers ownership
of those later checks; it does not waive current remediation regressions, platform checks, or recovery acceptance.


## Phase 2 checkpoint — 2026-10-26

elmdb `af141adb2c7745fceca7fb5af410159fdc9fa30b` centralizes commit finalization. Schema memory and
index markers agree with the committed transaction; watches are notified before a failed sync is returned as
`NOT_DURABLE`. A pre-commit error leaves sequence, data, schema and notifications unchanged. Fault injection
covers batch, unit, reset, replace, load, rename, rebuild and schema mutation.

Monorepo `5edd51adf7` pins that engine and updates load, clear, replace, setSchema and declare handling.
Committed errors refresh declarations and notify before rethrowing; pre-commit refusals do neither. A direct
connection listener test proves exactly one notification. The backup importer test proves rollback after a
replacement commits and then reports failed sync. API reports are unchanged.

Local checks passed on the isolated macOS worktrees:

- Rust workspace: engine 214 passed / one measurement ignored; CLI one, binding two, xtask two passed.
  Formatting, clippy with warnings denied, generated protocol and single-LMDB checks passed.
- Rebuilt release binding, TypeScript checks and all 33 Node tests passed.
- Full monorepo typecheck passed; final focused EARS typecheck passed after the last test addition.
  EARS: 12 files / 198 tests passed. Host backup: eight tests passed against the rebuilt binding.
  EARS API check: two reports current.

Logs have prefix `/private/tmp/elmdb-remediation-phase2-`: `rust-final.log`, `clippy-final.log`,
`generated.log`, `one-lmdb.log`, `build.log`, `types.log`, `node.log`, `typecheck.log`,
`ears-types-final.log`, `ears-final.log`, `backup-final.log`, and `api.log`.

Firing evidence used disposable copies only. Moving the Rust sync-error return ahead of notification made
`committed_operations_notify_before_reporting_a_failed_sync` fail (watch count one rather than two); see
`rust-mutation.log`. Omitting EARS committed-error notification made five watch cases fail; see
`ears-mutation.log`. The initial disposable resolver harness error was diagnosed and corrected, with its log
retained separately as `mutation-resolver-error.log`. No working source was mutated for these checks.

The user explicitly approved both repository branch pushes and required CI, then PR creation, on 2026-10-26.
This resolves the earlier automatic approval rejection; no merge, release or tag is authorized. Remote CI and
platform results will be recorded separately. These are Phase 1–2 checkpoints, not final remediation closure;
Phases 3–6 and integrated acceptance remain outstanding.
