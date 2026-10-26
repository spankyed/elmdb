# Goal: elmdb safety, consistent commits, and trustworthy recovery

Status: prepared, 2026-10-26. Implementation has not started. Preparation is authorized; this document records the
proposed implementation scope and does not itself start implementation or grant commit, push, merge, or release permission.

```text
# Goal: elmdb safety, consistent commits, and trustworthy recovery

Implement docs/goals/goal-elmdb-remediation.md when implementation is authorized. Follow elmdb/CLAUDE.md and, for
AgentBuddy, its CLAUDE.md, docs/AGENTS.md and user memory. Read Scope, Current handoff, Decisions, Phases,
Acceptance and Constraints. The assessment is docs/reviews/elmdb-review-2026-10.md; preparation evidence is
docs/evidence/elmdb-remediation-preparation.md. Inspect current source before relying on this survey.

Use the two isolated session/elmdb-remediation worktrees named in Current handoff. Do not edit or build in the source
checkouts. The active goal-elmdb-next.md is separate work; reconcile pinned committed results in isolation.

Finished when every acceptance row is proved on the final integrated commits, current reference docs describe the
result, finding dispositions link to fixes or evidence, Outcome is written, and the goal is archived with links repaired.
No finding is closed by an unrelated passing test or by moving its requirement to a follow-up.
```

## Scope

Resolve R01–R08 from the [engineering review](../reviews/elmdb-review-2026-10.md), investigate H01–H04 to a recorded
conclusion, and preserve the active goal's query, watch, type, compression, and search guarantees when integrating.
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
- Eight reviewed implementation files match the source checkout. R01–R08 remain relevant; no fix is claimed. The
  detailed revalidation, mechanism probe, integration trace, and setup results belong in the preparation evidence.
- Next deliverable after implementation authorization: Phase 0's current-source baseline, then Phase 1's guard and open
  behavior. Before edits, re-read the latest active goal's handoff and compare its committed inputs with this branch.
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
| D9 | The active goal retains its own features and roadmap designs. This goal contributes the review's acceptance requirements and verifies the final combined result without rewriting that goal or its code concurrently. |
| D10 | Preserve format/version policy from format.md and AgentBuddy's current compatibility policy. Prefer API/behavior changes with coordinated in-repo callers over compatibility shims. No live-data conversions. |

Registration design requirements: use an independently opened handle for each independently loaded binary; a private
Rust static or N-API environment data is insufficient across copies and workers. The lock namespace must identify this
process consistently, avoid cross-process exclusion, and fail safely on permission errors. Retain the owning handle;
never unlink/recreate the lock path while a claim is live. Address PID reuse, child processes, worker teardown, binding
unload, and a rejected copy reopening. Treat diagnostic metadata separately from synchronization, including Windows
locked-file I/O. Do not introduce polling, environment mutation, or per-database locks that weaken the process guarantee.

## Phases

### Phase 0: freeze inputs and establish the baseline

**Deliverable:** source commits, worktree/link/cache verification, and relevant current checks recorded in evidence.
**Dependencies:** implementation authorization and isolated setup.
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
**Dependencies:** Phase 0; reconcile changes to derive/entities with current active-goal commits before integration.
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
**Dependencies:** Phases 1–5 and the committed active-goal implementation being combined.
**Done when:** pin exact inputs in both repositories and validate their combination in isolated integration checkouts.
Run elmdb's required Rust/generated/dependency/binding checks and supported-platform matrix; run AgentBuddy's focused
EARS/watch/model/backup/schema checks, full typecheck for shared-source edits, and required final strict chain. Follow
its Linux/native boundary and one-expensive-run constraints. Investigate regressions using the active goal's workload
and single-row benchmarks with recorded conditions. Confirm planner order/limits/tracking, typed schema validation,
compressed read/export/recovery paths, and substring/regex candidate correctness on the final feature set. If either
input advances, re-evaluate affected evidence before integration. Merge/push only when separately authorized.

Update review dispositions with source and evidence, reference docs with implemented contracts, and this goal's Outcome.
A deferred required repair needs explicit user authorization; moving it to a follow-up does not satisfy acceptance.

## Acceptance

| Requirement | Implementation / source | Evidence | Remaining work |
|---|---|---|---|
| Isolated source, build caches and binding resolution | Prepared; branches and paths above | Preparation evidence | Confirm current-source baseline before implementation |
| R01/R02 opening and process safety | Missing | Registration mechanism spike only | Phase 1 |
| R03 committed outcomes and AgentBuddy notifications | Missing | Review E04/E06; integration trace | Phase 2 |
| R05/R06 lifecycle and H03 contention | Missing | Review E01/E05; static trace | Phase 3 |
| R04 auditing and R07 bounded construction | Missing | Review E02/E03 and source complexity | Phase 4 |
| R08 recovery envelope and H01/H02/H04 disposition | Missing | Review source assessment | Phase 5 |
| Final feature integration and required gates | Missing | None | Phase 6 |
| Current reference docs, review dispositions, Outcome and closure | Missing | None | Phase 6 |

## Constraints

Follow each repository's standing guidance. Preparation does not authorize implementation, commits, pushes, PRs or
merges; later user instructions govern those actions. Never share writable dependencies or build outputs, modify live
app data, or run the app without an isolated profile. Copied artifacts seed a build; they do not establish freshness.
Keep goal-next's expensive measurements separate from remediation runs; use idle checks and the repository's measurement
runner, or report alternating comparisons and load conditions. Do not dispatch CI as a debugging loop.

Use specific regressions and bounded validation during development, then the required converged checks. Preserve logs
and exact inputs. Diagnose a failure before repeating broad execution. A change to a finding's disposition needs evidence,
and a material scope change needs user authorization. Keep one current handoff here and detailed dated evidence elsewhere.
