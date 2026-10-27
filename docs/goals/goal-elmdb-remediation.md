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

- All edits/builds remain in isolated `session/elmdb-remediation` worktrees:
  elmdb `/Users/spankyed/Develop/Projects/apack/elmdb-worktrees.noindex/elmdb-remediation`, based on
  `4ea9e79f5ea9e0a32793b1a5a84c9d98b3403266`; monorepo
  `/Users/spankyed/Develop/Projects/apack/monorepo-worktrees.noindex/elmdb-remediation`, based on
  master `7ac1d99fb24649a7ce3e708035bae07cde45ee78`. Set ELMDB_DIR to isolated elmdb for app commands.
  The 11 GB target seed was APFS-cloned, never linked; dependencies/build outputs are isolated. Source checkouts
  remain untouched. No app launched or live database accessed.
- User authorized implementation, commits, pushes, draft PRs and required CI on these branches. No merges outward,
  tags or releases. PRs: [elmdb #1](https://github.com/spankyed/elmdb/pull/1) and
  [apack #283](https://github.com/spankyed/apack/pull/283). Earlier push rejection was resolved by explicit approval.
  goal-elmdb-next stays paused before Phase 3; do not wait for its future work. Revalidate upstream under D11
  before final integration.
- Phases 1–4 are committed/pushed: R01/R02 `2414390`, R03 `af141ad`, lifecycle/tracking `5146508`,
  TLS teardown `27c76ae`, audit/bounded construction `42423db`, Windows module lifetime `6b839bd`.
  Platform run [40424293465](https://github.com/spankyed/elmdb/actions/runs/40424293465) is terminal success
  across all eight jobs at 6b839bd, including Windows. These results do not verify later Phase 5 code.
- Monorepo is clean at `7c59b97310f754f909abc6ce49466c4b47858db6`, pinning 6b839bd. Committed-error caller
  handling and CLI permission fixtures are committed. Latest app checks passed 198 EARS tests, eight backup tests
  and EARS types with no fatal/TLS diagnostics. All 71 CLI database tests passed after fixture correction.
  Original strict chain 40419289605 failed integration-2's old permission assumptions; other executed jobs passed.
  A final green strict chain remains required. VM SSH timed out and Azure lookup hit an authentication block;
  no network/resource/authentication changes were made. Windows behavior is now verified by CI instead.
- Phase 5 local checkpoint source: backup publication `c32a87a`, local schema/snapshot pairing `7a4df3c`,
  audit scratch sorting/text streaming `614170c`. Required host checks passed together in session 43978:
  formatting, workspace clippy/tests (242 engine tests, two measurements ignored), generated/single-LMDB checks,
  rebuilt binding/typecheck and all 34 Node tests. Disposable mutations prove backup ownership, read-gap,
  watch schema refresh and duplicate sort winner regressions fire. H01 cross-process schema semantics remain
  unresolved; local guards alone do not protect against another process changing declarations.
- H02 now uses owned private files, held same-process destination reservation, atomic publication and Unix directory
  sync/Windows write-through. Seven focused tests pass. Other-process destination ownership is explicitly the
  caller's obligation; no power-cut guarantee is inferred from sync calls. Phase 5 platform validation is pending.
- R08 remains open. Owned synthetic measurements are in implementation evidence and `scripts/measure-recovery.mjs`.
  At 100,000 one-KiB entities/199,997 relations, baseline rebuild/replace upper bounds were ~11/~21 seconds.
  At 200,000 entities, baseline check peaked at 3,438,112 KiB RSS; current check is clean at 1,600,848 KiB /
  30.256 seconds, with 659,907,632 bytes sampled peak scratch and complete cleanup. This fits the selected 2 GiB
  investigation budget for this shape, not a universal bound. CLI baseline dump/load at 100,000 rows took
  2.10/77.46 seconds; load retained all decoded loads and peaked at 2,062,188,544 bytes RSS.
- No execution session remains running. Next: bounded load/staging publication and remaining recovery measurements
  (larger rebuild/replace, large values/vector shape), cross-process schema pairing, then converged source checks,
  app pin/strict CI, current reference contracts, finding dispositions and closure. Do not close acceptance early.
  Preserved CLI baseline executable and exact source archive, mutation logs and measurement conditions are in
  [implementation evidence](../evidence/elmdb-remediation.md). Keep one expensive local owner at a time.

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
| R04 auditing and R07 bounded construction | Implemented locally; Phase 4 checkpoint | Primary fixtures, firing mutations, cursor counts and restart tests | Supported-platform checks and final integration |
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
