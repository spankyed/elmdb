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
