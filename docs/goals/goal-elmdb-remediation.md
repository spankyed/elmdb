# Goal: elmdb safety, consistent commits, and trustworthy recovery

Status: active, 2026-10-26. Implementation, commits and pushes to the two remediation branches are authorized.
Those branches may be rebased onto upstream main/master or receive merges from upstream main/master. Merging the
remediation branches into any other branch, including main/master, is prohibited; releases remain unauthorized.

```text
# Goal: elmdb safety, consistent commits, and trustworthy recovery

Implement docs/goals/goal-elmdb-remediation.md. Commit and push usable checkpoints on the two remediation branches.
Only rebase onto or merge in upstream main/master; never merge the remediation branches into another branch.
Follow elmdb/CLAUDE.md and, for
AgentBuddy, its CLAUDE.md, docs/AGENTS.md and user memory. Read Scope, Current handoff, Decisions, Phases,
Acceptance and Constraints. The assessment is docs/reviews/elmdb-review-2026-10.md; preparation evidence is
docs/evidence/elmdb-remediation-preparation.md. Inspect current source before relying on this survey.

Use the two isolated session/elmdb-remediation worktrees named in Current handoff. Do not edit or build in the source
checkouts. goal-elmdb-next.md is paused before Phase 3. Finish remediation against current upstream; do not wait
for its future features or resume that goal. Leave remediation unmerged for user review.

Finished when every acceptance row is proved on the final integrated commits, current reference docs describe the
result, finding dispositions link to fixes or evidence, Outcome is written, and the goal is archived with links repaired.
No finding is closed by an unrelated passing test or by moving its requirement to a follow-up.
```

## Scope

Resolve R01–R08 from the [engineering review](../reviews/elmdb-review-2026-10.md), investigate H01–H04 to a recorded
conclusion, and preserve the behavior already implemented on upstream main/master when integrating. Future
planner, field-type, compression and search changes belong to goal-elmdb-next.md after remediation is landed.
The review remains the dated assessment; this goal owns implementation order and acceptance.

Required outcomes: safe process registration and database opening; one coherent commit outcome across every mutation
entry point; observer lifecycle that terminates predictably; complete declared-constraint auditing; bounded index-build
traversal; an evidenced recovery resource envelope; and AgentBuddy integration that handles committed errors correctly.

Exclusions: implementing the other goal's features a second time; new replication, async units, incremental views, or
bitmap formats; converting live user data; changing release metadata. Resource work serves R07/R08 rather than a general
performance rewrite. New unrelated defects are recorded with evidence, not silently absorbed into this goal.

## Current handoff

- elmdb: `session/elmdb-remediation`, `/Users/spankyed/Develop/Projects/apack/elmdb-worktrees.noindex/elmdb-remediation`,
  based on `4ea9e79f5ea9e0a32793b1a5a84c9d98b3403266`. The review's implementation baseline is `f7bc8a5`; subsequent
  source-checkout commits through `4ea9e79` changed documentation only.
- AgentBuddy: `session/elmdb-remediation`,
  `/Users/spankyed/Develop/Projects/apack/monorepo-worktrees.noindex/elmdb-remediation`, based on
  `7ac1d99fb24649a7ce3e708035bae07cde45ee78` from `master`.
- The 11 GB elmdb `target/` was copied with `cp -cR` (APFS copy-on-write), not linked. Sample outputs have distinct
  inodes and equal sizes. elmdb's tooling dependencies and binding output were copied independently; the isolated binding rebuild passed during preparation. AgentBuddy has its own `npm ci --offline` installation, and its `vendor/elmdb` and npm
  resolution point into this isolated elmdb worktree. Build outputs are seeds until source/build agreement is verified.
- The isolated binding build, `npm run typecheck`, and all 29 binding tests passed; generated declarations/loader
  are unchanged. No full AgentBuddy app build or app validation is claimed.
- Phase 0 baseline: isolated EARS package build and 14 watch tests passed; the prepared binding/type checks were
  already green. Phase 1 implements R01/R02: process claims use nonblocking file locks, and read-only opens retain
  reader registration. Final binding build/typecheck and all 33 Node tests passed; the full Rust workspace, generated/dependency checks,
  formatting, and clippy passed. Full Rust/generated/dependency final checks and the platform matrix are recorded in
  [implementation evidence](../evidence/elmdb-remediation.md). Cross-platform closure is pending, not waived.
- Regressions were shown failing against the old behavior in disposable copies. No shared checkout was mutated.
- Sequencing (user decision, 2026-10-26): goal-elmdb-next is paused before Phase 3. This goal runs independently
  against current upstream, completes its own checks, and leaves committed/pushed remediation branches for user
  review. Landing requires separate user authorization. goal-elmdb-next resumes only when the user resumes it after
  remediation has landed in both repositories and its branches incorporate those upstream commits.
- Local Phase 1 checkpoints: elmdb `2414390`; monorepo `a68ee6999b` (pin and persistence guidance).
  All 189 EARS tests passed against the final Phase 1 binding. Its initial push was rejected by automatic approval review before execution. The user subsequently explicitly
  approved both branch pushes, PR creation and required CI on 2026-10-26; the earlier block is resolved.
- Phase 2 local checkpoints: elmdb `af141adb2c7745fceca7fb5af410159fdc9fa30b`; monorepo `5edd51adf7`.
  Shared commit finalization preserves committed schema and notifies observers before reporting failed sync. EARS
  refreshes declaration caches and notifies committed admin changes before rethrowing. Full local checks and firing
  mutations are recorded in implementation evidence. Next deliverable: Phase 4 constraint auditing and bounded index construction.
  Phase 3 implements panic cleanup, callback-close refusal and bounded optimistic token/watch registration.
  Deterministic and mutation regressions pass. Alternating 20,000-row measurements reproduced 167–322 ms prior
  tracking-lock waits; current nonconflicting waits were zero and paced token/watch waits about 21 ms, with
  write maxima 22–25 ms. H03 is resolved locally with bounded fallback and documented synchronous watch cost.
  Final local Rust checks pass (221 engine tests; two measurements ignored), as do binding/type checks and 33 Node tests. Windows test quoting is fixed
  locally; fresh platform verification and final integration remain pending. Checkpoint `5146508` is pushed;
  monorepo validation exposed callback-TLS teardown panics hidden by passing Vitest totals. A `try_with` correction
  and bounded TLS-destruction regression pass all required Rust/binding checks and 198 EARS tests without fatal
  diagnostics. Commit the correction, then update the temporary monorepo pin to that commit. CLI typecheck passes;
  build prerequisite default-setup output and validate prepared permission cases plus the affected database suite
  (session 2370) before committing that integration correction. See implementation evidence.
  Revalidate upstream inputs before integrating them under D11.
- No app has been launched and no live database accessed. Do not reuse an explicit data-directory override from another
  checkout. Full AgentBuddy app/pack build and integrated acceptance are not claimed by dependency installation.

## Decisions

| ID | Decision and owner |
|---|---|
| D1 | The engineering review owns the dated findings; this goal owns their remediation acceptance and disposition. Revalidate findings on current code before implementing them. |
| D2 | Keep all edits, generated output, staging, builds and validation in the two remediation worktrees. Set `ELMDB_DIR` explicitly for every AgentBuddy setup/build environment. Never relink another checkout's `vendor/elmdb`. |
| D3 | R01's first implementation candidate is the pinned toolchain's `std::fs::File::try_lock`, held for the accepted binary's process lifetime, initialized once per binary. Confirm the complete registration design with Phase 1 tests; a successful local file-lock probe alone does not prove the binding guard. |
| D4 | No implicit `NO_LOCK` fallback. Ordinary read-only opens preserve reader registration or return an actionable error. Do not add an immutable mode without a demonstrated need and its own contract. |
| D5 | A failed sync after commit must preserve committed in-memory schema/state and notify observers. Keep `NOT_DURABLE` meaning committed but not known durable; actual commit failures do not emit commit notifications. Determine any additive error metadata from the Rust contract and generate declarations. |
| D6 | AgentBuddy refreshes declaration caches and invalidates affected views after a committed schema/admin error before rethrowing it. This must happen once per committed operation; ordinary pre-commit failures produce no notification. Do not infer commit from error text. |
| D7 | R05/R06 require an explicit listener panic and reentrant-close contract. Prefer a bounded, local correction over a general event-dispatch rewrite; publish the chosen behavior in engine.md and test it. |
| D8 | H01–H04 are hypotheses, not predetermined fixes. Close each with a deterministic test, measurement, or a documented contract; if it demonstrates a gap in the required guarantees, fix that gap and add its acceptance evidence. |
| D9 | **Remediation first, then goal-elmdb-next.** goal-elmdb-next stays paused before Phase 3. This goal completes against currently implemented upstream main/master and does not depend on that goal's future work. After separately authorized landing of remediation in both repositories, the user may resume goal-elmdb-next; it incorporates upstream and owns verification of its new features against the remediation guarantees. |
| D10 | Preserve format/version policy from format.md and AgentBuddy's current compatibility policy. Prefer API/behavior changes with coordinated in-repo callers over compatibility shims. No live-data conversions. |
| D11 | The user authorized implementation, commits and pushes on both session/elmdb-remediation branches (2026-10-26). Rebase onto or merge in upstream main/master as needed. Never merge a remediation branch into main/master or another branch; no feature-branch merges are authorized. After an authorized rebase, update only the owned remote remediation branch with --force-with-lease when necessary, never an unconditional force push. Required CI may run on the owned pushed branches; do not dispatch it as a debugging loop. Release/tag/publication permission is not granted. |
| D12 | Choose and record workload-based resource bounds before optimizing R08/H03; routine engineering parameters do not require another user decision. Each hypothesis gets a finite investigation, evidence and a conclusion. Preserve required guarantees; deferring a demonstrated required repair or changing scope still needs user authorization. |

Registration design requirements: use an independently opened handle for each independently loaded binary; a private
Rust static or N-API environment data is insufficient across copies and workers. The lock namespace must identify this
process consistently, avoid cross-process exclusion, and fail safely on permission errors. Retain the owning handle;
never unlink/recreate the lock path while a claim is live. Address PID reuse, child processes, worker teardown, binding
unload, and a rejected copy reopening. Treat diagnostic metadata separately from synchronization, including Windows
locked-file I/O. Do not introduce polling, environment mutation, or per-database locks that weaken the process guarantee.

## Phases

### Phase 0: freeze inputs and establish the baseline

**Deliverable:** source commits, worktree/link/cache verification, and relevant current checks recorded in evidence.
**Dependencies:** isolated setup.
**Done when:** current findings are mapped to source; the isolated binding build matches source without unintended
loader/type changes; binding/type tests and focused AgentBuddy EARS build/spec commands can run independently. Record
existing failures and diagnose relevant ones. Rerun the historical probes only in a disposable copy if source changes
invalidate their evidence. Do not install their defective-behavior assertions as production regressions.

### Phase 1: opening and process safety — R01/R02

**Deliverable:** an atomic duplicate-binding guard and read-only open behavior preserving LMDB locking.
**Dependencies:** Phase 0.
**Done when:** concurrent first opens from two physical binding copies admit one; the accepted binary works across
workers and later opens; duplicate copies get `ALREADY_LOADED`; no process environment is mutated; distinct processes
are independent; claim errors refuse safely; and lifecycle/path/child cases in D3 are tested on supported platforms.
A permission-restricted reader with an active writer registers normally or refuses. Update the AgentBuddy persistence
docs that currently promise lockless fallback. Follow the repository's binding/build matrix requirements.

### Phase 2: committed outcomes in Rust and AgentBuddy — R03

**Deliverable:** shared commit finalization and coordinated caller behavior, including in-memory declarations.
**Dependencies:** Phase 0; Phase 1 is not required for focused local development.
**Done when:** schema change, reset, replace, bulk load, rename and rebuild fault-injection cases distinguish commit
failure from sync failure. A failed sync leaves committed data/schema coherent and Rust watches notified. AgentBuddy's
`transact`, load, clear, replace, setSchema and declare paths rethrow committed errors after updating declarations and
notifying views exactly once; uncommitted refusals do neither. Use error codes/structured data, not messages. Exercise
raw database users, EARS, and backup/restore error handling. API reports and reference docs describe the final contract.

### Phase 3: watch lifecycle — R05/R06 and H03

**Deliverable:** unwind-safe observer state and a terminating callback-close contract; tracking contention evidence.
**Dependencies:** Phase 0; coordinate any shared finalization changes with Phase 2.
**Done when:** initial/subsequent callback panic produces the documented watch state, commits stay distinguishable from
listener failure, and later writes behave correctly. Callback close, unwatch, write, schema change and recursive delivery
are bounded subprocess cases. Resolve H03 by measuring lock wait and commit latency under long concurrent tracked reads;
fix a demonstrated unacceptable serialization cost or record the supported behavior and measurement rationale.

### Phase 4: constraint auditing and bounded construction — R04/R07

**Deliverable:** constraint inventory with independent audit checks, and cursor-based index-build traversal.
**Dependencies:** Phase 0; reconcile changes to derive/entities with currently landed upstream commits before integration.
**Done when:** unchecked invalid primary data for every declared constraint produces a specific violation, including
cycles, unique pairs, endpoints, self-links, info types, and unindexed unique values. Nested references follow the stated
contract. Rebuild cannot hide violations. Index-build visits grow approximately linearly overall, memory follows chunk
size, and interruption/resume plus insert/delete/trash transitions preserve correct completed indexes.

### Phase 5: recovery and snapshot guarantees — R08/H01/H02/H04

**Deliverable:** measured recovery bounds and an implementation meeting them; conclusions on schema/snapshot pairing,
backup publication, and independent audit verification.
**Dependencies:** Phase 4 for trust in validation; Phase 2 for committed outcomes.
**Done when:** record supported production-shaped data sizes and recovery peak memory, disk headroom and writer
occupancy. Set justified operating bounds from that evidence; if current whole-database operations cannot meet them,
implement bounded recovery, using staging where needed for atomic installation. Test cancellation, partial failures,
publication ownership, retry and restoration. Resolve H01 with deterministic interleavings, H02 with explicit concurrency
and filesystem durability tests/contracts, and H04 with independent model/mutation evidence. Fix confirmed violations
of the required guarantees; a hypothesis label alone is not closure.

### Phase 6: integrated acceptance and disposition

**Deliverable:** one tested combined result, current contracts, and final finding dispositions.
**Dependencies:** Phases 1–5 only. No dependency on goal-elmdb-next completion or its unlanded feature branches.
**Done when:** incorporate upstream main/master into the remediation branches using D11, pin exact inputs in both
repositories, and validate the combined result in isolated checkouts. Do not merge other feature branches to construct
a candidate. Test remediation with currently implemented upstream behavior; do not wait for future goal-elmdb-next
features or for remediation to be merged out.
Run elmdb's required Rust/generated/dependency/binding checks and supported-platform matrix; run AgentBuddy's focused
EARS/watch/model/backup/schema checks, full typecheck for shared-source edits, and required final strict chain. Follow
its Linux/native boundary and one-expensive-run constraints. Investigate regressions using the active goal's workload
and single-row benchmarks with recorded conditions. Verify existing query order/limits/tracking, current schema
validation, value read/export/recovery paths, and currently implemented search behavior. Record the exact upstream
feature set tested. The checks for future planner/type/compression/substring changes are owned by goal-elmdb-next,
not prerequisites for this goal's closure. If either
input advances, re-evaluate affected evidence. Commit and push validated checkpoints under D11; leave the remediation
branches unmerged for review.

Update review dispositions with source and evidence, reference docs with implemented contracts, and this goal's Outcome.
A deferred required repair needs explicit user authorization; moving it to a follow-up does not satisfy acceptance.

## Acceptance

| Requirement | Implementation / source | Evidence | Remaining work |
|---|---|---|---|
| Isolated source, build caches and binding resolution | Done locally; isolated binding/EARS builds | Preparation and implementation evidence | Final combined-source baseline in Phase 6 |
| R01/R02 opening and process safety | Implemented locally; checkpoint `2414390` | Implementation evidence; 33 Node tests, Rust regressions, firing cases | Supported-platform matrix and final integration |
| R03 committed outcomes and AgentBuddy notifications | Implemented locally; `af141ad` / `5edd51adf7` | Implementation evidence; fault injection, mutation tests, 198 EARS / eight backup tests | Remote checks and final integration |
| R05/R06 lifecycle and H03 contention | Implemented locally; Phase 3 checkpoint | Implementation evidence; bounded regressions, mutation tests, alternating measurements | Final checks, supported-platform matrix and integration |
| R04 auditing and R07 bounded construction | Missing | Review E02/E03 and source complexity | Phase 4 |
| R08 recovery envelope and H01/H02/H04 disposition | Missing | Review source assessment | Phase 5 |
| Current-upstream integration and required gates | Missing | None | Phase 6; no future-feature dependency |
| Current reference docs, review dispositions, Outcome and closure | Missing | None | Phase 6 |

## Constraints

Follow each repository's standing guidance. Implementation, commits and pushes to the two remediation branches are
authorized by D11. Only rebase onto or merge in upstream main/master. Do not merge a remediation branch into any
other branch, including main/master. The user additionally authorized PR creation and required CI on 2026-10-26. Releases, tags and publication remain unauthorized. Never share writable dependencies or build outputs, modify live
app data, or run the app without an isolated profile. Copied artifacts seed a build; they do not establish freshness.
Keep goal-elmdb-next paused; neither its completion nor user-authorized landing of remediation is a condition for
this goal's own completion. Do not automatically resume the paused work. Use idle checks and the repository's measurement
runner, or report alternating comparisons and load conditions. Do not dispatch CI as a debugging loop.

Use specific regressions and bounded validation during development, then the required converged checks. Preserve logs
and exact inputs. Diagnose a failure before repeating broad execution. A change to a finding's disposition needs evidence,
and a material scope change needs user authorization. Keep one current handoff here and detailed dated evidence elsewhere.
