# Goal: elmdb safety, consistent commits, and trustworthy recovery

Status: complete, 2026-10-27. Implemented and validated on the two remediation branches, committed and pushed for
review in elmdb #1 and apack #283. The branches remain unmerged; releases remain unauthorized. goal-elmdb-next stays
paused until the user separately authorizes landing and resumption.

**Original execution prompt (historical):**

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

Resolve R01–R08 from the [engineering review](../../reviews/elmdb-review-2026-10.md), investigate H01–H04 to a recorded
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

Acceptance is complete. The isolated worktrees remain available for review:

| Input | Validated revision / location |
|---|---|
| elmdb source | `9aea4c18093dc0ec49e848079d0a46ccd3d3a02a`; `/Users/spankyed/Develop/Projects/apack/elmdb-worktrees.noindex/elmdb-remediation` |
| apack | `20de66e19ff6ff6607b7a753b94c0f6175612348`, pinning that exact elmdb revision; `/Users/spankyed/Develop/Projects/apack/monorepo-worktrees.noindex/elmdb-remediation` |
| Current upstream | elmdb main `3afdf3700db041d08bcec9f0c7e13d66275ebe1a`; apack master `5574f84ceaee30392e3430b8a4981f0ba55dd9d9`; both ancestral, freshly revalidated at acceptance |
| Engine CI | [40443984290](https://github.com/spankyed/elmdb/actions/runs/40443984290), all eight jobs passed |
| App strict CI | [40444016830](https://github.com/spankyed/apack/actions/runs/40444016830), all six strict jobs passed |
| Review | [elmdb #1](https://github.com/spankyed/elmdb/pull/1), [apack #283](https://github.com/spankyed/apack/pull/283) |

The closure commit changes documentation only; apack retains the exact validated source pin. Source checkouts and
live data were untouched. Dependencies and build outputs stayed isolated, with the original 11 GB target seed
APFS-cloned rather than linked. No expensive local execution remains active. The tested feature set is landed v1/live
queries; future planner, type, compression and search work remains with the paused next goal. Detailed dated work,
failures, corrections, operating bounds and receipts are in [implementation evidence](../../evidence/elmdb-remediation.md).

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

| Requirement | Implementation / source | Acceptance evidence | Remaining work |
|---|---|---|---|
| Isolated source, build caches and binding resolution | Completed in the two named worktrees; exact native provenance recorded | Preparation/implementation evidence; final source pin and strict receipt | None |
| R01/R02 opening and process safety | `2414390`, `6b839bd`, `9aea4c1` | Ownership/permission/growth regressions and firing mutations; final eight-job matrix including Windows Rust/Node | None |
| R03 committed outcomes and apack notifications | `af141ad`, `24be744`; app `5edd51adf7` / `9ae643be01` | Every mutation entry point, fault injection and firing proofs; 199 EARS/eight backup cases, final strict chain | None |
| R05/R06 lifecycle and H03 contention | `5146508`, `27c76ae` | Bounded panic/close/TLS regressions, registration mutations, alternating contention measurements; final platform/strict gates | None |
| R04 auditing and R07 bounded construction | `42423db` | Primary inventories, independent model/mutation evidence, cursor counts, interruption/resume/transitions; final platform/strict gates | None |
| R08 recovery envelope and H01/H02/H04 disposition | `c32a87a`, `7a4df3c`, `97e2675`, `bce836a`, `744ebf8`, `9aea4c1`; current reference contracts | Reference/larger/vector/large-value measurements, golden restoration, SIGKILL/retry, publication faults and schema races; final platform/strict gates | None within the stated operating envelope; measured limits remain documented |
| Current-upstream integration and required gates | Exact inputs above; upstream freshly revalidated as ancestral | Host fmt/clippy/workspace/generated/single-LMDB/binding gates, full shipped benchmark, eight-job engine matrix and six-job app strict chain | None; future features are outside this goal |
| Current reference docs, review dispositions, Outcome and closure | Reference contracts reconciled; dated dispositions appended; Outcome below; goal archived and backlinks repaired | Documentation-only closure with source equivalence and link validation | None |

## Outcome

Completed 2026-10-27. R01–R08 are repaired within their specified contracts and measured operating envelope;
H01–H04 have evidence-backed conclusions in the [review dispositions](../../reviews/elmdb-review-2026-10.md).
The architecture is retained: Rust owns behavior and generated contracts, primary data reconstructs derived state,
and writes preserve atomic data/index changes. Opening, committed-error, lifecycle, audit, chunking, snapshot and
recovery guarantees now have focused regressions and demonstrated firing cases.

The integrated inputs are elmdb `9aea4c1` and apack `20de66e19f`, with the exact full hashes above. All eight engine
CI jobs and all six app strict-chain jobs pass. Host validation includes 254 engine tests/two ignored measurement
cases, 35 Node tests, workspace lint/generated/dependency gates, 199 EARS/model/watch tests, eight backup tests,
71 corrected CLI database cases and full root typecheck. The strict chain additionally validates the complete required
producer/integration/native/app checks on disposable macOS runners.

Final shipped EARS measurements complete all 24 cases on the recorded app-sized graph. Create/short-code medians
are 102.744/59.086 ms; the reset-related write regression was diagnosed and repaired without weakening schema,
logical sequence or observer guarantees. The final matrix also exposed and then verified the Windows read-only
mapping repair. Earlier failed/superseded runs remain in the evidence; they are not counted as passing acceptance.

The 100k-row reference rebuild/replacement meet the selected 2 GiB RSS / 30 s budget. Larger 200k-row replacement
takes 47.693 s, requiring a longer maintenance window; vector/large-value profiles and restoration/interruption
checks pass. These are explicit shapes, not universal bounds. Long snapshots, remaining graph/inventory state,
compound-index expansion, scratch/destination disk planning and filesystem publication limits remain documented
operating contracts. Use representative owned copies to measure a materially different workload.

Both branches and PRs are ready for user review. No merge or release was performed. goal-elmdb-next remains paused;
its future features and the separate lmdb-js research plan retain their own scope and acceptance.

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
