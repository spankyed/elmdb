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


## Phase 3 work in progress — 2026-10-26

The isolated implementation removes an unwinding listener and its read set, continues other listeners, and keeps
the write's commit/sync outcome. Initial listener failure returns `CALLBACK_PANICKED`. A thread-local stack of
active database tracking identities makes close return `CALLBACK_ACTIVE` before taking locks or the store; nested
callbacks retain outer database identities, while another inactive database can close normally. Aborting panics
remain process-terminating. Rust owns the new error codes and the protocol has been regenerated.

Focused tracked-read tests: ten passed / one explicit contention measurement ignored. Close and schema/unwatch/
nested-delivery cases run in subprocesses with ten-second deadlines. Completion counters outside the callbacks
ensure caught assertion panics cannot produce passing tests. The first schema fixture removed an immutable short-code
prefix and was correctly refused; it was corrected before the passing run.

Disposable mutation evidence: bypassing panic containment and the callback-close guard produced three failures,
including both subprocess deadlines, with seven other cases passing. Log:
`/private/tmp/elmdb-remediation-phase3-mutation.log`. The real worktree was never mutated for this proof.
Focused passing log: `/private/tmp/elmdb-remediation-phase3-tracked.log`. Broader Rust checks and the release-mode
contention measurement are in progress; no Phase 3 checkpoint or closure is claimed yet. The measurement machine
currently has concurrent external Node workloads, so results must carry that condition rather than be called idle.

Phase 1–2 remote checkpoints: elmdb PR https://github.com/spankyed/elmdb/pull/1, CI
https://github.com/spankyed/elmdb/actions/runs/40419287426; monorepo PR
https://github.com/spankyed/apack/pull/283, strict chain
https://github.com/spankyed/apack/actions/runs/40419289605. At the latest observation both runs were active; Linux
x64 checks, Linux x64/Node 22 and Linux arm64/glibc jobs had passed. These runs test the Phase 1–2 commits, not
the uncommitted Phase 3 changes. No workflow was restarted.


### Phase 3 measurements and CI diagnosis

The full Rust workspace passed (217 engine tests; two explicit measurements ignored), as did formatting,
workspace clippy, generated protocol and single-LMDB checks. Binding rebuild/checks are still in progress.

`cargo test --offline --release -p elmdb measure_tracking_contention -- --ignored --nocapture` passed.
Conditions: local arm64 macOS, syncs disabled, two tracked readers each performing eight full unindexed title
scans, and sixteen single-row writes; external Node workloads were active. At 2,000 rows: untracked-read median
1,306 us, tracked-read median 1,715 us, write maximum 16,149 us, sampled tracking-lock maximum wait 15,657 us.
At 20,000 rows: untracked-read median 13,592 us, tracked-read median 19,497 us, write maximum 352,162 us.
Direct lock samples missed the long wait at 20,000 rows; writes also acquire tracking state internally. This is
evidence of end-to-end contention, not a precise attribution of all 352 ms to one lock acquisition. Log:
`/private/tmp/elmdb-remediation-phase3-contention.log`. H03 remains open: investigate reducing serialized tracked
read time while preserving snapshot/registration ordering; do not treat this measurement as acceptable closure.

The completed Windows job in Phase 1–2 CI passed its Rust suites, then failed the worker-lifetime Node test with
`MODULE_NOT_FOUND`: nested template-string parsing stripped Windows path backslashes before loading the binding.
Log: `/private/tmp/elmdb-remediation-phase2-windows.log`, job 121909379551. The test source now JSON-encodes the
whole worker program, preserving path literals at both evaluation levels. Other matrix jobs continue; no workflow
rerun was dispatched. A fresh Windows run is required on the next converged checkpoint.


### Bounded optimistic registration — in progress

Following the measured contention, token and watch queries now run outside the tracking mutex on their first two
attempts. Before filing dependencies, they validate that the commit sequence has not advanced. A conflict retries
once; a second conflict runs under the mutex, guaranteeing a bounded number of executions under continuous writes.
Validation and filing share the mutex used by commit notification, so a commit either precedes the validated read
or sees its dependencies. Token reads register cancellation tickets; `untrack` cancels in-flight registration, and
unwind cleanup removes tickets. In-flight reads keep write logging enabled.

Four deterministic tests cover a commit during token execution, a commit during watch execution, cancellation by
untrack/unwatch, fallback after repeated conflicts, and unwind cleanup. All pass. Disabling sequence validation
in a disposable copy made the three interleaving/fallback cases fail; log
`/private/tmp/elmdb-remediation-phase3-registration-mutation.log`. Current passing log:
`/private/tmp/elmdb-remediation-phase3-registration.log`. The matching release-mode workload measurement, fresh
workspace checks and rebuilt binding checks are running; comparative performance and Phase 3 closure remain pending.

The earlier lifecycle binding build, typecheck and all 33 Node tests passed before optimistic registration was
added (`phase3-build.log`, `phase3-types.log`, `phase3-node.log`, same temporary-log prefix as above). Fresh logs use
`phase3-optimistic-`. A clippy loop-style failure on the first optimistic implementation was corrected to `while let`
before restarting the stopped Rust check chain.

Phase 1–2 matrix run 40419287426 is now terminal: all seven non-Windows jobs passed, Windows failed on the recorded
worker-test quoting error. The monorepo strict-chain run remains active. A transient TLS polling timeout was
followed by a successful observation of that same run; it was not restarted.


The first matching after measurement completed: at 2,000 rows, write median 34 us / maximum 238 us; at 20,000
rows, write median 38 us / maximum 110 us, compared with the earlier 352,162 us maximum. At 20,000 rows,
untracked read median was 13,453 us and tracked read median 19,287 us / maximum 47,619 us. Log:
`/private/tmp/elmdb-remediation-phase3-contention-after.log`. This is a promising sequential comparison under
external load, not yet an alternating or idle controlled comparison. Readers may retry, so their tail cost remains
part of the operating contract. Measure repeated/write-heavy and watch workloads before closing H03.

Refreshed workspace checks passed: 220 engine tests, two explicit measurements ignored; CLI one, binding two and
xtask two passed; formatting, workspace clippy, generated and dependency checks passed. The later unwind-cleanup
test then passed in the focused four-case registration suite. The final binding rebuild is still running (exec
session 74605); do not restart it based on an observation timeout.


The refreshed optimistic binding build, TypeScript checks and all 33 Node tests passed. Logs:
`/private/tmp/elmdb-remediation-phase3-optimistic-build.log`, `-types.log`, and `-node.log` (same prefix).
The strict-chain producer job passed; its integration/native consumers are now executing.

The contention runner now alternates the prior locked algorithm and current token algorithm in the same release
binary, with the same rows and query. Its additional watch-registration and 128 paced-write cases exercise overlap
and retry/fallback costs; these are finite bursts, not a steady-state throughput benchmark. Each writer in a paced
case waits 250 us between commits. The locked comparison is test-only and reproduces the previous mutex boundary;
there is no production switch or alternative API. Current run: exec session 22945, log
`/private/tmp/elmdb-remediation-phase3-contention-alternating.log`. Do not start a duplicate measurement.

Provisional evaluation criterion for the reference 20,000-row token workload: eliminate scan-duration mutex hold
in the nonconflicting case; keep the finite write burst's p95 below 10 ms and maximum below 50 ms under the
recorded local conditions. These are workload evaluation bounds, not a public latency SLA. Repeated conflicts may
serialize one final read, deliberately bounding retry count at three. Synchronous watch notification still makes a
write pay for affected watches and listeners; this unavoidable execution time is evaluated separately from waiting
on unrelated tracked reads. Confirm results before disposing H03.


### Phase 4 source inventory prepared while the Phase 3 measurement compiles

Current `derive::violations` was re-read before implementation. This is an inventory of gaps, not a finding disposition.

| Constraint | Current audit source | Required correction/proof |
|---|---|---|
| Required fields, field value types, refused additional fields | Primary entity fields, `values::check_field` | Preserve and cover every value type; nested lists require explicit reference traversal |
| Missing reference targets | Only top-level `ValueType::Ref` | Recurse through list element types; schema.md promises missing-id audit |
| Unique entity values | Scans derived index keys | Derive equality groups from primary values, including long strings, maps, bytes and timestamps; respect single-field list and compound rules |
| Missing relation ends | Primary relation rows | Preserve, including trashed entities |
| Cardinality | Primary relation rows, distinct far-end sets | Preserve parallel-edge semantics; all four cardinalities |
| Self links, endpoint type pairs, required/typed info | Enforced by writes; absent from audit | Audit primary relation rows independently of derived edges |
| Unique relation pairs | Enforced by writes; absent from audit | Group primary `(kind, source, target)` pairs |
| Acyclic relation kinds | Enforced by traversal of derived edges; absent from audit | Build primary adjacency and audit in linear graph traversal; do not trust damaged derived edges |

`entities::unique_keys` explicitly includes values that `entries_of` omits. Audit must use its equality semantics
(`values::same`, including integer/float equality and unordered map fields) without depending on stored indexes.
Rebuild leaves unbuildable value indexes unmarked, so a derived-key-only audit can hide precisely these violations.
Independent fixtures/model assertions must establish the intended result rather than merely compare two calls to
the same derivation helper. Removed schema declarations retain primary data by contract; do not automatically
report every retained undeclared type/kind as corruption.

R07 source confirms both live/trash ranges are fully scanned and sorted in every `build_chunk`. A repair needs
cursor bounds after the last entity sequence, merging bounded live/trash candidates, with count/visit evidence and
interrupt/resume plus trash/mutation cases. The current `Read::visit` supports prefix bounds only, so its range
contract and tracking implementation must be considered before adding a cursor helper.


### Phase 3 local disposition — 2026-10-26

The alternating release measurement passed in 4.50 s. Same binary, local arm64 macOS, syncs off, two readers x
eight full unindexed scans at 2,000 and 20,000 rows. The order was locked / token / watch-paced / token-paced /
token / locked; paced cases used 128 writes with a 250 us inter-write delay, others sixteen writes. Machine load
was observed separately: rustc plus macOS indexing/UI processes; no idle-run claim is made. Log:
`/private/tmp/elmdb-remediation-phase3-contention-alternating.log`.

At 20,000 rows:

| Case | Tracking-lock maximum wait (us) | Write p95 / maximum (us) | Read median / maximum (us) |
|---|---:|---:|---:|
| Prior locked, first | 322294 | 45 / 150 | 18023 / 19810 |
| Token, first | 0 | 58 / 173 | 17885 / 45296 |
| Watch, paced | 21379 | 85 / 21507 | 21034 / 77699 |
| Token, paced | 20754 | 64 / 25125 | 17931 / 73491 |
| Token, second | 0 | 45 / 122 | 18002 / 42506 |
| Prior locked, second | 166887 | 59 / 158853 | 18246 / 20387 |

The explicit lock probe precedes each write and can absorb contention before the write timer starts. Therefore a
small write time alone is not proof of cheap serialization; both columns matter. Even the sum of the revised paced
cases' separate maximum lock wait and maximum write is below 50 ms (an upper bound, not a paired observation).
The reference write p95/max bounds were met; read tail cost increased through retries and deliberate fallback,
while median scan costs stayed near 18–21 ms. These are finite-burst measurements, not steady-state guarantees.

H03 conclusion: confirmed a tracking-mutex serialization problem and corrected it with bounded optimistic token
and watch reads. The nonconflicting path releases the mutex during query execution; three-attempt bound and
registration/notification ordering are covered deterministically, and mutation tests fail when validation is removed.
Under continued conflicts a final serialized scan is supported behavior; this runner measured its finite-burst cost.
Synchronous affected-watch execution and user callback time remain part of a committing thread's latency, with no
public latency SLA. No general asynchronous dispatch redesign is required for this finding.

R05/R06 are implemented and locally covered by initial/subsequent panic, later writes, other listeners, close/timed
close, schema change, unwatch, recursive writes and nested database callback cases. Regressions run with subprocess
deadlines where a deadlock is possible; outside-callback completion counters prevent containment from hiding assertion
failures. Required platform/final-integration evidence is still pending. This local disposition does not close the goal.


Final Phase 3 checkpoint checks passed: formatting; workspace clippy with warnings denied; workspace tests
(engine 221 passed / two measurements ignored, CLI one, binding two, xtask two); generated protocol and single-LMDB
checks. Logs: `/private/tmp/elmdb-remediation-phase3-final-clippy.log` and `-final-rust.log`. The final source-only
change since the passing binding build is the test measurement expansion; production code is unchanged. All
local Phase 3 sessions are terminal. Next implementation phase is primary-data constraint auditing and bounded
index construction. Remote platform evidence on this checkpoint and final integrated acceptance remain required.


### Phase 3 checkpoint and integration follow-up

Phase 3 checkpoint `5146508d7372d284ab43651693e91062edb704e6` was committed and pushed. Its platform run is
https://github.com/spankyed/elmdb/actions/runs/40421138412. This checkpoint is not yet accepted in the monorepo.
The attempted pin ran 198 EARS tests with passing API/type checks, but inspection found fatal TLS-destruction
panics during each worker's teardown despite Vitest reporting success. Treat that run as failed integration, not
clean evidence. Log: `/private/tmp/elmdb-remediation-phase3-ears.log`. No pin commit/push was made.

Cause: N-API cleanup closes databases after callback TLS has been destroyed. The new `in_callback` lookup used a
panicking TLS accessor. The follow-up uses `try_with` and treats unavailable state as inactive: no live callback
scope can remain on a thread whose TLS destructors are executing. A bounded subprocess initializes database-owning
TLS before callback TLS, forcing the destruction order; the corrected implementation passes. Log:
`/private/tmp/elmdb-remediation-phase3-tls.log`. Disposable mutation proof is running in session 1846; required
Rust/binding checks for the correction are running in session 7913, logs with prefix
`/private/tmp/elmdb-remediation-phase3-tls-`. Keep those sessions until terminal; no replacement run.

The original monorepo strict-chain run has a completed failed integration-2 job (121913624373); other consumers
continue. Log: `/private/tmp/elmdb-remediation-phase2-strict-integration2.log`. Failure is the old CLI test that
chmods both data and lock files read-only and expects a successful open, contrary to R02. The isolated CLI test now
separates readable nonwritable data (writable lock retained) from lock-access refusal with actionable IO diagnostics.
Its focused validation waits for the corrected binding. These test edits and the temporary `5146508` vendor pin
remain uncommitted; update the pin to the TLS-fix checkpoint before accepting it.


TLS regression firing proof completed: the pre-fix accessor aborts the child with SIGTRAP and fails the parent
(exit 101); corrected source passes. The disposable harness initially retained an earlier mutation removing the
close guard, so that initial run did not exercise the lookup. Restoring the current database call site made the
intended mutation fail; no production source was changed to manufacture the result. Final failing log:
`/private/tmp/elmdb-remediation-phase3-tls-mutation.log`. Session 1846 and corrected proof session 5743 are terminal.
The correction's full Rust checks passed; session 7913 continues into binding rebuild/type/Node checks. Monorepo
permission tests and the temporary pin remain uncommitted pending corrected-binding validation.


### TLS correction validated locally — 2026-10-26

The correction's required checks passed: formatting, workspace clippy, all workspace tests, generated protocol,
single-LMDB dependency, rebuilt release binding, typecheck and all 33 Node tests. Session 7913 is terminal.
The rebuilt binding then passed all 198 EARS tests with **no** panic/abort/TLS diagnostics (explicit log inspection):
`/private/tmp/elmdb-remediation-phase3-tls-ears.log`. This replaces the invalid earlier green Vitest total.

A separate natural/terminated Node worker probe did not reproduce the precise TLS destruction order and was
removed; the deterministic bounded Rust regression and the existing EARS worker integration provide the firing
and corrected evidence. CLI typecheck passed. Focused CLI permission cases initially failed before assertions
because the isolated worktree lacked `packages/default-setup/dist`; a single prerequisite build followed by the
focused cases and affected database suite is now running in session 2370. Logs use prefix
`/private/tmp/elmdb-remediation-phase3-` with suffixes `default-setup.log`, `cli-permissions.log`, `cli-db.log`.
No app launch or live-data access is involved.


### TLS checkpoint and start of constraint remediation

TLS correction committed/pushed as `27c76ae646c19a44c6fdd6715ca8d652a8080a84`; its platform matrix is
https://github.com/spankyed/elmdb/actions/runs/40421747228. The monorepo now provisionally pins this correction.
EARS 198 tests passed without fatal diagnostics; CLI typecheck passed. The default-setup prerequisite built once,
and two permission tests passed. The full CLI database suite then passed 70 and failed a second fixture still
chmodding the lock file read-only during dry runs. That fixture now protects only data.mdb too; the new lock-access
refusal test continues proving the fail-safe behavior. All three permission cases and the full affected suite are
running in session 74761, final logs `phase3-cli-permissions-final.log` and `phase3-cli-db-final.log` under the
existing temporary-log prefix. Do not restart the strict chain to diagnose this already isolated failure.

Phase 4 begins locally with recursive missing-reference auditing. The new nested-list fixture has one present and
two missing targets plus an opaque `any` string; it asserts exactly two element-path violations both before and
after rebuild. Focused test passed. Copying the new test onto the pre-fix audit in the disposable source produced
zero violations instead of two (exit 101), proving the regression fires. Logs:
`/private/tmp/elmdb-remediation-phase4-references.log` and `-references-mutation.log`. These Phase 4 changes are
uncommitted and are not part of the rebuilt/pinned TLS checkpoint. Remaining primary uniqueness/relation auditing
and bounded builder traversal are still required; this is not R04 closure.


Final CLI validation passed: three permission cases and all 71 database integration tests, using the corrected
TLS binding. Logs: `/private/tmp/elmdb-remediation-phase3-cli-permissions-final.log` and `-cli-db-final.log`.
Monorepo checkpoint `547279e4d6d3f231d822e2b9621deb76e920895b` commits both fixture corrections and pins
`27c76ae646c19a44c6fdd6715ca8d652a8080a84`; it was pushed to the owned remediation branch. No production CLI
behavior changed. Session 74761 is terminal. Existing remote strict-chain consumers and the latest engine platform
matrix still require terminal observation; no broad workflow rerun was dispatched.


## Phase 4 relation auditing and Windows lifetime investigation — 2026-10-26

Primary relation auditing now checks self-links, endpoint type pairs, required/typed info, nested info references,
unique pairs and acyclic kinds. Graphs come from primary rows, not derived edges. Iterative three-color DFS visits
each edge once with expected constant-time hash lookups; deterministic root sorting costs O(V log V), and graph
memory is O(V + E). Cardinality now visits each endpoint group once rather than rescanning all groups for every
schema kind. Existing distinct-far-end semantics are preserved.

The independent fixture writes nine unchecked primary rows, with no derived edges. It proves specific violations
for all new relation checks, then proves rebuild leaves the violation list identical. The new case passes; copied
onto the pre-fix audit it fails on the missing cycle report (only the old missing-end report appears). Logs:
`/private/tmp/elmdb-remediation-phase4-relations.log` and `-relations-mutation.log`. A direct graph case checks
20,000 nodes, an acyclic chain and parallel forward edges, then one back edge: exactly one cycle report. Log:
`/private/tmp/elmdb-remediation-phase4-cycles.log`. All 23 derived tests and focused clippy pass (`-derived-final.log`,
`-clippy-final.log`). These changes remain uncommitted; primary-value uniqueness and R07 are not implemented yet.

The latest completed Windows job 121917617450 (run 40421747228) now gets past path quoting but rejects the
accepted image after its first worker exits: the main thread's first open receives `ALREADY_LOADED`. Log:
`/private/tmp/elmdb-remediation-phase3-windows.log`. This demonstrates that retaining a claim handle is insufficient
if Windows unloads the DLL and a later load reinitializes its Rust static. R01 is still open across platforms.

The local Windows-only correction pins the accepted image after acquiring its claim, using
[GetModuleHandleExW FROM_ADDRESS + PIN](https://learn.microsoft.com/en-us/windows/win32/api/libloaderapi/nf-libloaderapi-getmodulehandleexw).
Microsoft documents that PIN keeps the module mapped until process termination. The address identifies this
executing image rather than a basename. A pin error drops the just-acquired handle and refuses opening with IO.
Only the accepted image is pinned; independent rejected copies still cannot acquire the lock. This keeps the static
and handle alive together rather than adding a second ownership-metadata protocol. Local claim tests (two) and
node-crate clippy pass; neither executes the cfg(windows) path. No Windows pass is claimed.

The prescribed Windows VM was probed read-only: SSH timed out after ten seconds. Azure instance-view lookup
failed with AADSTS530035 (security-defaults authentication block), so no current power state was established.
No login, network-rule change, VM start/stop or new workflow was attempted. Focused Windows validation awaits
restored access or the next converged platform gate; the Windows-only pin remains uncommitted. Other goal work
can proceed independently. Latest remote strict-chain consumers are still active; integration-2's diagnosed
permission-fixture failure is corrected locally and in monorepo checkpoint `547279e4d6`.


## Phase 4 primary uniqueness and bounded traversal — 2026-10-26

Uniqueness auditing no longer reads stored index entries. It groups primary entity values per type/index, using
the write contract's key extraction (single-field list elements; complete non-list compound tuples). Numeric
representations and map field order are canonicalized for ordinary ordered grouping, so long strings, bytes,
timestamps and maps remain auditable without an index key. Duplicate-key MessagePack maps are permitted by the
current value decoder, while `values::same` on them is not an equivalence relation; these take an exact-comparison
fallback rather than silently changing write semantics. Ordinary grouping avoids quadratic pairwise comparison;
with A ambiguous tuples and N ordinary tuples the fallback is O(A*N + A²). This unusual-input cost is explicit,
not a claim of universal O(N log N). Primary value storage during audit remains part of R08's resource investigation.

The independent fixture covers long strings, reordered maps with equivalent numeric representations, duplicate-key
maps, bytes, timestamps, integer/float equality, signed zero, null and list/compound distinctions. Each corrupts
primary fields directly, proves specific unique violations before rebuild, then proves they remain after rebuild.
The test passes; pre-fix derived-index auditing fails on the first long-string case. Logs:
`/private/tmp/elmdb-remediation-phase4-unique.log` and `-unique-mutation.log`. Focused clippy passes.

Index build chunks now seek both live/trash cursors after the saved sequence, collect at most limit+1 rows per
range, and sort only those bounded candidates. Temporary candidate storage is at most 2*(limit+1); visits are
linear at fixed chunk size, with O(C log C) sorting per chunk. At C=64, actual cursor entries were 1,991 / 8,231 /
33,191 for 1,024 / 4,096 / 16,384 mixed live/trash rows. The test verifies every sequence/id once in merged order.
Removing the per-range limit in a disposable copy fails the bound at 1,024 rows (8,704 visits). Logs:
`/private/tmp/elmdb-remediation-phase4-cursors.log` and `-cursors-mutation.log`.

A real index fixture resumes manual chunks around inserts, deletion, field edits and live/trash movement both
below and above the cursor, then checks every derived key. A second interrupted build closes/reopens and completes
through the background builder; check is clean and the index marker is built. Log:
`/private/tmp/elmdb-remediation-phase4-resume.log`. All focused cases pass.

The combined required workspace/generated/dependency/binding checks are running once in session 39330, logs
`/private/tmp/elmdb-remediation-phase4-workspace-clippy.log`, `-workspace.log`, `-build.log`, `-types.log`,
`-node.log`. These Phase 4 changes and Windows-only module pin remain uncommitted. R04's full declared-constraint
inventory and H04's independent verification still need final coverage review; resource and platform closure remain
pending. Do not infer the goal is complete from these focused cases.


## Phase 4 inventory review and H04 — 2026-10-26

Constraint auditing is now isolated in `engine/derive/audit.rs`; reconstruction and bounded build traversal stay
in derive.rs. The extraction initially missed the explicit value_indexes import; corrected before passing suites.
All 26 then-current derived tests and both direct graph/cursor tests passed after extraction. The earlier combined
check session 39330 completed with all 33 Node tests passing, but predates this module extraction and is not the
final source gate.

Inventory review considered short-code prefixes and retained roles. A trial current-prefix audit made the existing
rename-into-populated-type regression fail: admin rename deliberately retains old short codes. That check and its
trial test were removed, preserving the demonstrated contract. Prefix validation is a creation rule, not a global
invariant of historical primary data. Short-code uniqueness is audited from primary values. Missing legacy codes
and removed-role data remain allowed, consistent with schema.md's retained-declaration-removal contract.

A new unchecked field fixture asserts exactly twelve specific violations: missing required field, refused extra
field, nonnullable null, and wrong values for string, number, integer, boolean, bytes, enum, ref target type, list
element and object. A populated any field stays opaque. It passes (`phase4-field-inventory.log`). A separate fixture
checks all four relation cardinalities, parallel edges counting only once per far end, and identical reports after
rebuild; it passes (`phase4-cardinality-inventory.log`). Logs use the existing `/private/tmp/elmdb-remediation-` prefix.

H04 conclusion: check/rebuild share reconstruction, so check alone cannot prove that shared derivation correct.
The independent fixture creates exactly three known entities, rebuilds, checks, then asserts query count three and
the exact three IDs. Mutating shared count derivation to n+1 in a disposable copy leaves check clean but makes the
independent count assertion fail (four versus three). Log: `phase4-independent-mutation.log`. The correct source
passes in the derived suite. An initial assertion used count rather than the protocol's n field; it was corrected
before the meaningful mutation run. Together with unchecked primary fixtures and their firing proofs, this closes
H04's investigation locally while preserving model/query assertions as independent evidence, not treating shared
check as its own oracle. Final integrated verification still applies.

Current converged checks are session 53202 with log prefix `phase4-final-`: workspace clippy, 229 engine tests
(two measurements ignored), CLI/binding/xtask suites, generated/dependency checks passed; binding rebuild continues.
The later two inventory tests passed separately and focused lint logs to `phase4-inventory-clippy.log`. No production source changed after this final pipeline started. Do not start
a replacement binding build. The Windows module-pin path remains unproved on Windows.


Final Phase 4 local checks completed: session 53202 is terminal, rebuilt binding/typecheck and all 33 Node tests
passed (`phase4-final-build.log`, `-types.log`, `-node.log`). Workspace tests passed 229 engine cases with two
measurements ignored, plus CLI one, binding two and xtask two; the later field/cardinality inventory cases passed
separately. Focused lint passed after those test-only additions. All current staged production changes were covered
by the final build; afterward only stale comments and prose wrapping were cleaned up. R04/R07 and H04 have local
implementation/evidence; supported-platform checks and final integration remain outstanding. Phase 5 now owns
recovery resource measurement, schema/snapshot pairing and backup publication. The audit/builder and Windows
module-lifetime changes are separate logical checkpoints; the latter still requires real Windows verification.


## Phase 4 pushed checkpoints — 2026-10-26

Audit/builder checkpoint `42423dbd121c80d7eaa4cdfe52b509e52195e2c3` and separate Windows module-lifetime
checkpoint `6b839bd5e0309450570c256568cfb49064725f32` were reviewed, committed and pushed. The platform matrix
on their combined source is https://github.com/spankyed/elmdb/actions/runs/40424293465; Windows is not yet verified.
Monorepo validation against the rebuilt binding passed 198 EARS tests, eight host backup tests and EARS typecheck.
The passing logs were explicitly checked for fatal/panic/TLS teardown diagnostics; none appeared. Logs:
`/private/tmp/elmdb-remediation-phase4-ears.log`, `-backup.log`, `-ears-types.log`. Session 13615 is terminal.

The original strict chain (40419289605, Phase 2 source) is terminal: producers, integration-1, native-1, native-2 and
app-and-rest passed; integration-2 failed on the diagnosed CLI permission assumptions. Those fixtures are corrected
in `547279e4d6` and all 71 affected database tests passed locally. This is not a green final strict chain. No broad
retry was dispatched; final strict acceptance belongs on converged integrated commits after Phase 5.

Monorepo pin checkpoint `7c59b97310f754f909abc6ce49466c4b47858db6` was committed/pushed after that validation.


## Phase 5 schema/snapshot work in progress — 2026-10-26

H01 is confirmed locally. Ordinary reads previously cloned the schema before acquiring an LMDB snapshot; tracked
read retries and watch reruns could retain that clone across schema changes. Schema storage is now shared through
Arc<RwLock<Arc<Schema>>>. Ordinary reads/check/index status hold a schema read guard across snapshot execution;
watch reads acquire the live schema before tracking locks and release it before invoking listeners. A Unit retains
its write schema and a separate live schema source for post-commit observer delivery. This preserves lock ordering
and permits callback schema changes without continuing subsequent watch queries under the old declaration.

Primary export now captures declared schema, engine schema and the LMDB snapshot under the schema guard, then
releases that guard before export callbacks. Its immutable snapshot/schema remain paired while callbacks can
change live schema. Dump writes its header and primary loads through this same pair rather than reading schema
and data in separate calls. These changes protect changes through this Database's shared schema source; cross-process
schema mutation behavior still needs an explicit contract/review before closing H01.

Bounded deterministic tests pass: paused threaded read between schema acquisition and snapshot; watch callbacks
remove/restore a type during initial, batch and Unit delivery; and export callbacks change live schema while the
captured snapshot retains its old declaration. All 14 tracked tests pass (one explicit measurement ignored).
Logs: `/private/tmp/elmdb-remediation-phase5-pairing-final.log`, `-watch-schema.log`. Disposable mutations replacing
the read guard with a clone, and freezing the watch delivery schema, each fail their regression (exit 101):
`phase5-read-schema-mutation.log`, `phase5-watch-schema-mutation.log`. These are actual failures, not timeout
assumptions.

The actual dump/load regression passed and focused clippy passed (`phase5-pairing-dump.log`,
`phase5-pairing-clippy-final.log`). An earlier `dump::tests` filter selected zero tests, so it supplies no export
evidence; it was replaced with the actual `a_dump_and_a_load` filter. The corrected regression is included in the
preserved log. No H01 closure, new binding build, async-binding verification or full Phase 5 check is claimed yet.

The committed Phase 4 Windows module pin now passes the Windows job in run 40424293465, including the worker
lifetime reproducer. At the latest observation Linux checks, Linux glibc both architectures and arm64 musl also
passed; Linux x64 musl was active and both macOS jobs queued. Continue observing that same run to terminal state.
Backup publication (H02) and measured recovery bounds (R08) still require implementation/evidence; no resource
envelope has been claimed from these schema tests.


## Phase 5 backup publication and resource measurement — work in progress

Backup publication now reserves the canonical destination in the same process registry as Store::open for the
entire operation, creates a private tempfile in that directory, copies through its owned descriptor, syncs and closes
it, then replaces data.mdb. RAII removes only that invocation's temporary path on failure; the fixed
data.mdb.partial name is untouched. Unix syncs the destination plus parents of newly created directories; Windows
uses [MoveFileExW REPLACE_EXISTING + WRITE_THROUGH](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw).
No cross-volume copy flag is used. tempfile is now a runtime dependency rather than only a test dependency.

Pre-publication failures retain the earlier complete backup. If post-publication directory syncing fails, IO
explicitly reports that the complete copy is visible and a retry is safe. Seven focused backup tests pass, including
private-file ownership with an unrelated legacy partial sentinel, failure cleanup/retry, bounded publisher/open
concurrency handshakes, nested-directory creation and post-publication sync failure. Clippy passes after factoring
the test-hook type. Logs: `/private/tmp/elmdb-remediation-phase5-backup-cases.log`, `-backup-clippy.log`. The old
copy implementation fails the ownership/failure regression in the disposable copy (`phase5-backup-mutation.log`,
exit 101). Destination ownership across other processes and filesystem-specific power-loss guarantees need an
explicit contract before H02 closure; no simulated power-cut proof is claimed.

`scripts/measure-recovery.mjs` creates only owned synthetic workload directories and refuses to adopt existing
ones. Separate process commands measure seed/check/rebuild/replace/backup, RSS including native allocations,
elapsed time and logical disk usage. Replace/rebuild elapsed time is explicitly a conservative writer-occupancy
upper bound, not an instrumented lock-held duration. The 1,000-row/1,024-byte smoke seed and check passed, using
the last built Phase 4 binding (`6b839bd` source), not unbuilt Phase 5 code. Logs:
`/private/tmp/elmdb-remediation-recovery-seed-smoke.log`, `-check-smoke.log`.

Initial R08 evaluation workload chosen before optimization: 100,000 entities with 1,024-byte message bodies, two
relations per entity (apart from the first two), unique slug and state value indexes plus a title/body text index.
Evaluate against 2 GiB peak process RSS, 30 s writer-occupancy upper bound for replace/rebuild, and logical disk
headroom measured per operation. These are investigation criteria, not a final advertised support limit. Small,
reference and larger scales will be recorded before a final envelope is chosen; dump/load require their own CLI
measurements. Seed syncs are disabled; maintenance uses default durability. Filesystem cache and machine load
are recorded, not called idle or controlled by assumption. No live database or app profile is involved.


Reference baseline completed on the built Phase 4 binding (6b839bd), fresh Node processes, local macOS filesystem
cache uncontrolled. Synthetic 100,000 entities / 1,024-byte bodies / 199,997 relations, value and text indexes:

| Operation | Elapsed / writer upper bound (s) | Peak RSS (KiB) | Notes |
|---|---:|---:|---|
| seed | 27.260 / n/a | 223808 | Syncs off; source file 566,820,864 bytes |
| check | 11.411 / 0 | 1622608 | No differences or violations |
| rebuild | 11.059 / 11.059 | 1349296 | Source grew to 813,334,528 bytes |
| replace | 20.783 / 20.783 | 1163648 | New private destination; total logical footprint 1,224,336,207 bytes |
| backup | 0.870 / 0 | 582432 | Total logical footprint 1,763,795,791 bytes |

Logs have prefix `/private/tmp/elmdb-remediation-recovery-reference-` and suffixes `seed.log`, `check.log`,
`rebuild.log`, `replace-final.log`, `backup.log`. The first replacement harness incorrectly kept the source open
with different LMDB options and was refused with ALREADY_OPEN before replacing. The harness now leaves the source
closed, matching the API contract; its failed empty destination is retained and included in later logical totals.
No engine change was made to accommodate the harness. This baseline fits the initial reference criteria but is
not the complete envelope: larger/large-value and dump/load measurements are still outstanding, and Phase 5
safety changes have not yet been rebuilt into the binding.


The next scale is running once in session 60851: seed then check, 200,000 entities, same 1,024-byte/text/value/
relation shape. Logs: `/private/tmp/elmdb-remediation-recovery-large-seed.log`, `-large-check.log`. Host RAM was
read as 34,359,738,368 bytes before starting; only this synthetic resource run owns expensive local execution.
Retain the handle until terminal and inspect results before deciding on larger-scale changes. The runner and
all Phase 5 source changes remain uncommitted. H02 has seven passing local cases and clippy; filesystem-specific
platform acceptance and H01's remaining cross-process/async contract review are still pending.


## Phase 5 recovery investigation continuation — 2026-10-26

The earlier 200,000-row run completed: 399,997 relations, 1,024-byte bodies and the same indexes.
Seed took 64.615 s with 331,520 KiB peak RSS and a 1,152,843,776-byte source file. Baseline check took
23.473 s and 3,438,112 KiB peak RSS, exceeding the preselected 2 GiB investigation budget. Logs:
`/private/tmp/elmdb-remediation-recovery-large-seed.log`, `-large-check.log`.

First, check streamed stored rows against an expected map, removing the second complete actual-data map.
The 200,000-row check then took 24.974 s and 2,429,296 KiB RSS, still over budget (`-large-check-stream.log`).
The current implementation instead spills expected derived rows through an 8 MiB capacity-accounted buffer
plus the largest record, with global ordinals preserving last-insertion-wins semantics, fan-in 16 merging,
owned temporary directories and bounded open files. It streams the stored side and preserves report ordering.
This bounds sorting storage, not the complete audit: primary graph/constraint state and report output remain
workload-dependent. Scratch I/O failure returns an error rather than a partial successful report.

Four sorter tests cover multilevel merges, duplicates, empty/oversized rows, cleanup on failure and truncated
frames. The original duplicate mutation passed because its first and last values coincided; that fixture was
corrected to encode global emission order. Correct code passes and the first-wins mutation now fails. Logs:
`/private/tmp/elmdb-remediation-phase5-sort-winner.log`, `-sort-mutation.log`. Earlier broad checks passed:
242 engine tests (two measurements ignored), workspace CLI/binding/xtask tests, formatting, clippy,
generated/dependency checks and rebuilt binding/typecheck/34 Node tests. The strengthened fixture passes
separately. These are local checkpoint checks, not final integrated acceptance.

The first external-sort measurement took 28.913 s and 2,093,600 KiB RSS at 200,000 rows. It sampled
679,439,987 bytes of peak scratch and 1,832,317,410 bytes peak workload logical disk; sampling every
100 ms is a lower bound and logical sizes differ from allocated filesystem space. Scratch cleaned up.
This was only 3,552 KiB under budget, insufficient margin for a support claim. Log:
`/private/tmp/elmdb-remediation-recovery-large-check-spill.log`. Text derivation now emits document and
position rows directly rather than retaining a complete per-index map; necessary term/word aggregation
remains. All 17 text tests, 28 derived tests and clippy pass. The rebuilt-binding measurement is running
once in session 70929; inspect `-phase5-text-stream-binding.log` and `-recovery-large-check-text-stream.log`
before starting another expensive operation.

The CLI baseline completed in the former session 39547 (that handle no longer exists). At 100,000 rows,
dump took 2.10 s with 371,228,672 bytes maximum RSS; load took 77.46 s with 2,062,188,544 bytes maximum
RSS. macOS `/usr/bin/time -l` reports these memory values in bytes, unlike Node's KiB resourceUsage.
Logs: `/private/tmp/elmdb-remediation-recovery-{dump,load}-baseline{,-time}.log`.
The preserved executable is `/private/tmp/elmdb-remediation-pre-bounded-load`. Its explicit source archive
is `/private/tmp/elmdb-remediation-pre-load-source.tar.gz`, SHA-256
`4164a0bc32a114aa716d848b2788e4f0dff22f968ae3910fef1a30f5ebe46da6`.
Load still retains all decoded loads and rebuild still collects all derived rows. These results do not
close R08 or establish a final supported envelope. Bounded load/publication, cross-process schema pairing,
large-value/vector profiles and final combined-source validation remain outstanding.

Platform run [40424293465](https://github.com/spankyed/elmdb/actions/runs/40424293465), at `6b839bd`,
is terminal success: all eight jobs, including Windows, passed. This verifies the committed Windows
module-pin correction; it does not verify the uncommitted Phase 5 changes. Monorepo final strict CI is
still required. The isolated monorepo remains clean at `7c59b97310`.


The text-streaming measurement is terminal success: 200,000 rows, check clean, elapsed 30.256 s,
1,600,848 KiB peak RSS, sampled scratch peak 659,907,632 bytes and workload logical peak
1,812,760,575 bytes. The source remained 1,152,843,776 bytes; scratch cleaned up. Log:
`/private/tmp/elmdb-remediation-recovery-large-check-text-stream.log`. This gives approximately
473 MiB RSS margin below the selected 2 GiB budget for this shape. Relative to the baseline,
check uses less RSS but more elapsed time and temporary disk. Check is read-only: its 30.256 s
is not writer occupancy. This is one uncontrolled-cache run, not a percentile or a universal
bound. Required checkpoint gates are running once in session 43978, logs with prefix
`/private/tmp/elmdb-remediation-phase5-checkpoint-`.


Checkpoint gates completed successfully in session 43978: formatting, workspace clippy, workspace tests
(242 engine tests passed, two measurements ignored), generated protocol and single-LMDB checks, binding
typecheck and all 34 Node tests. The binding was rebuilt immediately before the measured text-streaming
run with unchanged production source. Logs use the checkpoint prefix above. Reviewed source checkpoints:
backup publication `c32a87a`, local snapshot pairing `7a4df3c`; the next audit checkpoint follows them.
No final integrated or Phase 5 cross-platform closure is claimed by these host checks.


## Phase 5 bounded dump loading — 2026-10-26, local work

The previous checkpoint is pushed through `e15b027`; CI run 40429674942 is active on that exact head.
The next loader change is local and not covered by that run. It applies at most 1,000 decoded loads or
8 MiB of encoded frame lengths per batch (plus the largest frame), into an owned `.elmdb-load-*`
directory inside the destination. It validates the footer/count and rejects trailing bytes, rebuilds,
closes, then publishes the complete data file without clobbering an existing destination. Unix uses
an atomic hard link followed by file/directory sync; Windows uses MoveFileExW without REPLACE_EXISTING,
with WRITE_THROUGH. Post-publication sync errors explicitly leave a visible complete database.
Same-process opens and publishers are excluded by a held canonical destination reservation. Other-process
destination ownership still belongs to the caller; a race creating data.mdb is refused without deleting it.

The existing three dump/order/embedding regressions pass. Two additional cases pass: 1,100 rows spanning
batches retain a byte-identical roundtrip, preserve unrelated files, and refuse a foreign publication;
late bad count/truncation/trailing bytes remove all private staging and permit retry. Workspace clippy
passes. Logs: `/private/tmp/elmdb-remediation-phase5-staged-load-{tests,cases,clippy}.log`.
Cancellation, mutation proof and broader final checks remain outstanding. The reference CLI build and
one load measurement are active in session 28079; do not restart or run competing expensive work.
Logs: `-phase5-staged-load-build.log`, `-recovery-load-staged.log`, `-recovery-load-staged-time.log`.


The staged reference load is terminal success: 36.06 s elapsed, 1,370,095,616 bytes maximum RSS,
100,000 entities, 600,000 fields, 199,997 relations and one sequence. Compared with the preserved
baseline's 77.46 s / 2,062,188,544 bytes RSS, both elapsed time and peak memory decreased on this
single uncontrolled-cache run. The new destination occupies 835,920 KiB according to du -sk;
that is allocated filesystem space, not the runner's logical-byte metric. The writer is private to
staging throughout load/rebuild; no existing destination is held open or modified.

Publication mutation is demonstrated in a disposable archive/target clone at
`/private/tmp/elmdb-remediation-load-mutation`: replacing Unix hard_link with overwrite-capable rename
makes the race regression fail (exit 101, unexpectedly successful load), as intended. The worktree was
not mutated. Log: `/private/tmp/elmdb-remediation-phase5-load-publication-mutation.log`.
All six dump tests now pass, including unwind cleanup before publication and an injected post-publication
sync failure leaving a complete, independently checked 1,100-row database. An unwinding interruption
cleans private staging; abrupt process termination may leave an unpublished private directory, never a
partially published database. Workspace clippy passes. Logs: `-phase5-staged-load-final-{cases,clippy}.log`.

One larger rebuild/replace measurement chain is active in session 88559, using the previously built
checkpoint binding (e15b027-equivalent production source). The staged loader edits do not change either
operation. Logs: `/private/tmp/elmdb-remediation-recovery-large-{rebuild,replace}.log`. Only this chain
owns expensive local execution. The dump-loader checkpoint is not committed and still needs all required
gates before commit. CI 40429674942 currently has Linux checks/glibc and Windows green; remaining jobs
are still active, so do not call the matrix complete.


## Phase 5 rebuild and replacement resource repair — local, 2026-10-26

At 200,000 entities / 399,997 relations, the checkpoint binding measured rebuild at 28.145 s /
2,656,272 KiB RSS, and replacement at 42.680 s / 2,301,536 KiB RSS. Both exceed the 2 GiB
investigation memory budget; larger replacement also exceeds the reference workload's 30 s writer
upper-bound criterion. Logs: `/private/tmp/elmdb-remediation-recovery-large-{rebuild,replace}.log`.

Rebuild now deletes batches of at most 1,000 keys without copying stored values or revisiting deleted
rows, preserving normal transaction undo/tracking through del. Full derivation uses the owned external
sorter and streams entries into the same atomic write. Replacement retains one source snapshot and copies
only primary prefix ranges directly into the destination transaction; it no longer retains a complete
primary-data vector or traverses source derived rows. Map-growth retries use that same immutable snapshot.
The destination still changes in one atomic unit, preserving its schema and format/metadata semantics.

All 28 derived-data tests and eight maintenance tests passed after the production changes, and clippy
passed (`phase5-bounded-rebuild-tests.log`, `-stream-replace-tests.log`, `-stream-recovery-clippy.log`).
Rebuilt-binding measurements then produced:

| 200,000-row operation | Elapsed / writer upper bound (s) | RSS (KiB) | Sampled scratch peak (bytes) |
|---|---:|---:|---:|
| rebuild | 28.118 / 28.118 | 1876128 | 661001966 |
| replace | 47.693 / 47.693 | 1856864 | 662408434 |

Logs: `-recovery-large-rebuild-spill.log`, `-recovery-large-replace-stream.log`. Source data.mdb grew
from 1,666,416,640 to 2,164,047,872 bytes during the repeated rebuild; the new replacement data file
added 956,456,896 logical bytes. These are sequential uncontrolled-cache measurements, not paired
latency statistics; file history differs. Peak sampled workload logical sizes were 3,176,962,585 and
3,970,016,079 bytes respectively, including earlier retained synthetic destinations. Scratch cleaned up.
Memory meets the chosen 2 GiB budget for this shape, but larger replacement latency is not hidden:
47.693 s is above the reference 30 s criterion. Final workload/operating bounds and larger-value/vector
profiles remain required; these results alone do not close R08.

A focused failure regression injects scratch-creation IO after rebuild has deleted derived keys, and
after replacement has cleared/copied primary data. Both operations return IO, preserve primary data,
sequence, query answers and clean derived state; replacement retry succeeds. The regression passes.
A disposable mutation swallowing the first scratch failure instead returns success and fails the test
(exit 101). Logs: `/private/tmp/elmdb-remediation-phase5-scratch-rollback{,-mutation}.log`.
Required combined-source gates are active once in session 74611; prefix
`/private/tmp/elmdb-remediation-phase5-load-checkpoint-`. Do not start competing expensive work.


## Phase 5 additional profiles and checkpoint gates — 2026-10-26

Combined-source required gates are terminal success in session 74611: formatting, workspace clippy,
workspace tests (246 engine tests passed, two measurements ignored), generated protocol/single-LMDB checks,
rebuilt binding/typecheck and all 34 Node tests. The two recovery source checkpoints are `97e2675`
(private bounded loader) and `bce836a` (bounded deletion/rebuild, streamed replacement). The exercised
vector workload extension is `9c18fdc`; node --check and the 20-row seed/check smoke run pass.
The prior checkpoint matrix [40429674942](https://github.com/spankyed/elmdb/actions/runs/40429674942)
is terminal success across all eight jobs at e15b027. The newer recovery source still needs platform CI.

Additional production-shaped synthetic profiles retain the same slug/state/text indexes and two backward
relations per entity. Both have 2,000 entities and 3,997 relations. The vector profile uses 1,024-byte bodies
and 2,000 deterministic 1,536-dimensional finite embeddings, model synthetic-recovery; this tests storage
and recovery, not semantic quality or an external embedding service. The large-value profile has 65,536-byte
bodies and no vector index. Values repeat the runner's stated natural-language phrase; vocabulary diversity
and graph shape are limited, so these are explicit shapes, not arbitrary-data capacity claims.

| Profile | Operation | Elapsed (s) | RSS (KiB) | Sampled scratch peak (bytes) |
|---|---|---:|---:|---:|
| vector | seed | 1.517 | 197872 | n/a |
| vector | check | 0.242 | 102192 | 0 |
| vector | rebuild | 0.844 | 156224 | 5747364 |
| vector | replace | 1.104 | 183280 | 9686933 |
| vector | backup | 0.035 | 119072 | 0 |
| large-values | seed | 18.765 | 413792 | n/a |
| large-values | check | 8.615 | 299408 | 98975636 |
| large-values | rebuild | 8.571 | 359968 | 80121459 |
| large-values | replace | 8.735 | 476048 | 50587386 |
| large-values | backup | 0.218 | 278368 | 0 |

Each operation used a fresh process, uncontrolled filesystem cache and the built current recovery source.
Seed syncs are disabled; maintenance uses defaults. Rebuild/replace elapsed time is a writer-occupancy
upper bound. Logical sampled peak across the retained vector workload was 252,036,238 bytes; across
large values it was 751,731,636 bytes, including earlier artifacts. Scratch cleaned up after every operation.
Logs: `/private/tmp/elmdb-remediation-recovery-{vector,large-values}-{seed,check,rebuild,replace,backup}.log`.

Separate read-only validation opens all four produced backup/replacement artifacts: check is clean,
independent expected primary counts are 2,000 entities/3,997 relations, and an exact slug lookup returns
Note-1999. Both vector artifacts have zero pending embeddings and a working nearest-vector query.
Log: `/private/tmp/elmdb-remediation-recovery-profile-restoration.log`; session 61811 is terminal success.
No benchmark time/RSS result includes that separate validation process.

One final current-source CLI build/dump/load chain is active in session 88967: 200,000-row, vector and
large-value profiles. Logs use `/private/tmp/elmdb-remediation-phase5-final-recovery-cli-build.log` and
`-recovery-{large,vector,large-values}-final-{dump,load}{,-time}.log`. Retain the handle to terminal state;
do not edit production source or start competing expensive work while this measurement chain runs.
Final restored-artifact validation and operating-envelope reference documentation remain required after it.
Cross-process schema pairing, final upstream/app integration, strict CI and finding dispositions are still open.
