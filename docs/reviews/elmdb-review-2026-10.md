# elmdb principal engineering review — October 2026

Date: 2026-10-26. Status: assessment complete; recommendations proposed, fixes not implemented by this review.

## Scope and baseline

This review assesses elmdb's current implementation and the intended shape after
[goal-elmdb-next.md](../goals/goal-elmdb-next.md). It covers storage safety, durability, transactions, watches,
query semantics and complexity, indexes, recovery, the Rust and JavaScript APIs, and maintainability. AgentBuddy's
integration was read to distinguish engine guarantees from behavior supplied by its caller.

The implementation baseline is `f7bc8a584841f5b3bac2621de7e32088748f46f9`. At the final recheck, HEAD was
`89169f32f3a26d2222105d69fe5847664f35f02d`; the only intervening change was the active goal's handoff. References below
name functions as well as files so later edits do not make line numbers authoritative. The implementation agent was
working separately; this review changed no implementation or application files.

Verification ran in a source copy under `/private/tmp/elmdb-review-6d0j24bh` on the development Mac, on 2026-10-26,
using the pinned Rust 1.99.1 toolchain and offline dependencies. Existing Rust tests: **210 passed, one measurement
ignored**. Six additional probes reproduced the behaviors recorded below. The probes and their sync-failure hook
are preserved in the [evidence patch](evidence/elmdb-review-2026-10-probes.patch).

The Node/platform matrix was not rerun. No new production-shaped performance measurements were taken. Complexity
claims below follow the code; they are not measured latency claims. Storage corruption, environment-access races,
and power-loss behavior were not induced. Proposed features are assessed as designs, not as completed implementations.

## Executive assessment

Keep the architecture. Rust owns behavior and generated cross-language contracts; indexes participate in the data's
transaction; returned bytes are owned; primary data can reconstruct derived data. The module and crate boundaries
are appropriate for the product's present size. The model, crash, tracking, and binding suites are a useful foundation.

The immediate weaknesses are guarantees that vary by entry point: safe opening, committed-but-not-durable outcomes,
observer lifecycle, and constraint auditing. Fix those before treating additional features as evidence of maturity.
A rewrite, a general query framework, or replacing LMDB is not supported by this review's evidence.

P1 means a high-consequence safety or correctness issue to resolve before broad release. P2 means a reliability,
scale, or API issue to schedule explicitly. These priorities do not claim that every trigger occurs in AgentBuddy today.

## Findings register

All findings start with disposition **proposed**. Preserve this dated assessment; append disposition changes with the
fix commit and acceptance evidence rather than rewriting the original observations. Valid later dispositions include
accepted, fixed, deferred with rationale, and disproven with evidence.

| ID | Priority | Finding | Evidence / confidence | Active-goal coverage |
|---|---|---|---|---|
| R01 | P1 | Duplicate-binding registration mutates the process environment unsafely and admits a race | Source and Rust contract; high | No explicit deliverable |
| R02 | P1 | Permission failure implicitly disables LMDB locking | Source and LMDB contract; high | No explicit deliverable |
| R03 | P1 | Failed syncs leave committed admin/schema changes incompletely finalized | Fault-injection probes E04/E06; confirmed | No explicit deliverable |
| R04 | P1 | A clean integrity report can miss declared constraint violations | Probes E02/E03; confirmed | P5/P9 touch related validation, but do not require this repair |
| R05 | P2 | A panicking Rust listener strands its watch | Probe E01; confirmed | Watch preservation alone does not cover panic handling |
| R06 | P2 | A Rust listener's blocking close waits on its own caller | Source proof and bounded-close probe E05; high | No explicit deliverable |
| R07 | P2 | Background index chunks repeatedly scan the whole type | Code-derived complexity; high | P6 changes postings, not this traversal |
| R08 | P2 | Recovery memory and writer occupancy grow with the whole database | Source; confirmed characteristic, impact unmeasured | Compression may change costs, but supplies no resource bound |

### R01 — Duplicate-binding registration

**Source:** [process.rs](../../crates/elmdb-node/src/process.rs), `claim_process` (baseline lines 21–35).

The guard reads `ELMDB_BINDING`, then calls unsafe `std::env::set_var`. Its safety comment considers what elmdb runs,
but cannot establish that Node, Electron, libc, or another native library is not concurrently reading the environment.
Rust's Unix safety requirement includes those readers. The module also acknowledges that two independently loaded
binaries can race their first claims and both succeed. A `Once` private to each binary does not serialize those claims.

**Consequence:** the mechanism intended to prevent duplicate LMDB copies has its own undefined-behavior risk and does
not enforce the design's process-wide exclusion under concurrency. Neither a crash nor duplicate-open corruption was
reproduced. [Rust's `set_var` safety contract](https://doc.rust-lang.org/std/env/fn.set_var.html).

**Recommendation:** use atomic registration shared across independently loaded binaries, without process-environment
mutation. Specify ownership identity, worker behavior, platform behavior, and failure handling before choosing the
mechanism; a Rust static inside each binary is insufficient.

**Acceptance:** two physical binding copies race their first opens from workers; exactly one claims ownership and the
other receives `ALREADY_LOADED`. Repeated opens from the accepted copy work. No environment mutation is involved.

### R02 — Implicit lockless read-only access

**Source:** [store/mod.rs](../../crates/elmdb/src/store/mod.rs), `Store::open` (baseline lines 171–180).

A permission-denied read-only open retries with `READ_ONLY | NO_LOCK`. A reader's permissions do not establish that
another user, or a process with an existing writable handle, cannot write. The fallback supplies no external
synchronization. LMDB requires the caller using `MDB_NOLOCK` to manage concurrency and protect readers from page reuse.
[LMDB's environment-open contract](https://raw.githubusercontent.com/LMDB/lmdb/mdb.master/libraries/liblmdb/lmdb.h).

**Consequence:** an ordinary open can silently abandon a storage-safety requirement. Actual corruption was not induced.

**Recommendation:** fail ordinary opens when reader registration is unavailable. If immutable copies need lockless
access, make that an explicit mode with a clear immutability requirement; do not infer it from permissions.

**Acceptance:** exercise a permission-restricted reader alongside an active writer. Ordinary open preserves locking or
refuses. Tests for an explicitly immutable mode establish its separate contract.

### R03 — Commit finalization after sync failure

**Sources:** [engine/mod.rs](../../crates/elmdb/src/engine/mod.rs), `admin_unit` and `Database::write`;
[maintenance.rs](../../crates/elmdb/src/engine/maintenance.rs), `reset` and `replace_with`;
[database.rs](../../crates/elmdb/src/database.rs), `apply_schema` and `set_schema`.

Normal batch writes preserve the distinction between a failed commit and a failed sync after commit. Admin paths use
`store.write(..., true)?`, which returns the sync error before watch delivery. Schema application similarly commits
the stored declaration before syncing, but `set_schema` updates its in-memory declaration only on success.

**Consequence:** committed changes can leave observers stale. More seriously, an open database can enforce a schema
different from the schema another open will read. A generic `IO` error also conceals that retrying may repeat a mutation.
E04 reproduced a committed reset with no watch notification; E06 reproduced old schema in memory and new schema after
reopen. These probes inject failure at the sync boundary, not at LMDB commit.

**Recommendation:** centralize commit finalization across batches, units, schema changes, and admin operations. After a
successful commit, install committed in-memory state and notify observers even when durability fails. Return an explicit
committed-but-not-durable outcome carrying the sequence. Preserve the existing distinction between visibility and sync.

**Acceptance:** inject sync failure during schema change, reset, replace, load, rename, and rebuild. Verify committed
state, schema agreement, observer notification, error classification, and safe caller retry behavior. Test actual commit
failure separately: it must not install the proposed state or emit a successful-change notification.

### R04 — Incomplete constraint auditing

**Source:** [derive.rs](../../crates/elmdb/src/engine/derive.rs), `violations` (baseline lines 291–397), compared with
[relations.rs](../../crates/elmdb/src/engine/relations.rs), `check_rules`, and
[entities.rs](../../crates/elmdb/src/engine/entities.rs), `check_unique`.

E02 loads a two-node cycle under an acyclic relation kind; `check()` reports no differences or violations. E03 loads
duplicate 257-byte strings into the unique `Task.slug` field; the report is also clean. Uniqueness auditing reads
derived index entries, which omit these long values, while ordinary writes enforce uniqueness through a primary-data
walk. The relation audit checks missing ends and cardinality but does not audit every rule the write path enforces.

**Consequence:** schema tightening, unchecked imports, and recovery can receive a false clean bill of health.

**Recommendation:** inventory constraints and map each to write enforcement and independent audit enforcement. Include
acyclicity, unique pairs, endpoint restrictions, self-links, relation info, and uniqueness for unindexed values. Check
nested reference declarations against the intended reference-audit contract as part of that inventory.

**Acceptance:** introduce invalid primary data for every declared constraint through unchecked load. Each case produces
a specific violation. Rebuilding derived data cannot conceal the violation. Existing valid data stays clean.

### R05 — Listener panic leaves a watch permanently silent

**Source:** [watch.rs](../../crates/elmdb/src/engine/watch.rs), `deliver` (baseline lines 134–179).

Delivery takes the listener from the registered watch and restores it after invocation. If invocation panics and the
caller catches the unwind, the watch remains registered without its listener. Later deliveries set `again` and return.
E01 demonstrates this state after a committed write, then confirms that another write does not invoke the listener.

**Consequence:** observer failure can escape through a writer after commit and silently disable future delivery.
This finding concerns Rust listeners; AgentBuddy's JavaScript connection separately catches listener exceptions.

**Recommendation:** define panic behavior and provide unwind-safe cleanup that restores or unregisters the watch.
Keep commit outcome distinguishable from observer failure. Decide explicitly whether callback panics propagate.

**Acceptance:** panic on initial and subsequent delivery, catch the unwind, then verify the documented watch state,
tracking cleanup, committed data, and subsequent writes. Turn E01 into an assertion of the chosen corrected behavior.

### R06 — Reentrant blocking close

**Sources:** [engine/mod.rs](../../crates/elmdb/src/engine/mod.rs), `Database::watch` and `Database::write`;
[watch.rs](../../crates/elmdb/src/engine/watch.rs), `deliver`;
[database.rs](../../crates/elmdb/src/database.rs), `close_as` (baseline lines 244–283).

The operation delivering a listener retains `Arc<Store>`. A listener calling blocking `close()` waits for exclusive
ownership that cannot become available until that listener returns. E05 verifies the ownership obstruction using
`close_within(Duration::ZERO)`; it does not deliberately run an indefinitely blocked `close()`.

**Consequence:** a natural shutdown action in a Rust listener can deadlock its own caller.

**Recommendation:** reject a reentrant blocking close with a specific error, or defer closure through an explicit
lifecycle mechanism. Document which operations listeners may invoke.

**Acceptance:** a subprocess case invoking close from a listener terminates predictably. Cover listener unwatch,
write, schema change, and recursive delivery so a lifecycle fix preserves supported reentrancy.

### R07 — Index construction does not bound traversal to its chunk

**Source:** [derive.rs](../../crates/elmdb/src/engine/derive.rs), `build_chunk` (baseline lines 501–514).

Every chunk scans both complete type ranges, filters rows after its previous sequence, sorts the remaining rows, and
takes about 1,000. For N entities and chunk size C, scanning alone is approximately O(N²/C), with additional repeated
sorting. Collection memory remains O(N) per chunk. These are code-derived bounds, not measured elapsed times.

**Consequence:** background construction increasingly occupies the single writer despite its small write chunks.

**Recommendation:** seek after the previous sequence in each range, merge the two cursors, and stop after C + 1 entries.
Keep progress and concurrency semantics explicit, including changes between live and trashed ranges.

**Acceptance:** count visited entries across construction at increasing N; traversal is approximately linear overall
and per-chunk memory follows C. Verify interruption, resume, concurrent insertion, deletion, and trash transitions.

### R08 — Recovery resource envelope is unspecified

**Sources:** [dump.rs](../../crates/elmdb/src/dump.rs), `load` (baseline lines 114–134);
[maintenance.rs](../../crates/elmdb/src/engine/maintenance.rs), `replace_with`;
[derive.rs](../../crates/elmdb/src/engine/derive.rs), `derive`, `check`, and `rebuild`.

Dump loading retains every decoded load before applying it. Replacement collects source entries; checking and
rebuilding construct substantial expected-state collections. Large operations also hold the writer for extended work.

**Consequence:** peak memory, disk headroom, and writer occupancy can make recovery impractical as data grows. The
implementation characteristic is confirmed; the scale at which it becomes a user problem has not been measured.

**Recommendation:** establish a supported recovery envelope. Evaluate streaming into an exclusively owned staging
database, validating there, and installing it deliberately after success. Preserve atomic visibility without requiring
all decoded data in memory. Make cancellation and failure cleanup own only files created by that operation.

**Acceptance:** record peak memory, writer occupancy, disk headroom, interrupted-load cleanup, and retry behavior on
production-shaped data at increasing scales. Preserve the earlier database and backup on unsuccessful publication.

## Hypotheses and open guarantees

These are investigation targets, not confirmed defects. They need deterministic tests or measurements before fixes.

| ID | Concern and source | Next evidence / decision |
|---|---|---|
| H01 | Reads clone the schema separately from taking their LMDB snapshot (`Database::read`, `engine_schema`, `set_schema`) | Pause a reader between these steps, change/rebuild an index, resume it, and verify coherent schema/snapshot pairing; cover async binding reads |
| H02 | Backup publication uses a fixed partial filename, a one-time destination-open check, and file sync before rename (`Store::copy_to`) | Test concurrent publication and destination opening; define the supported filesystem/power-loss contract and directory-sync requirements |
| H03 | Tracked reads execute under the shared tracking mutex (`watch::run_tracked`, `read_tracked`, `deliver`) | Measure lock wait and commit latency with concurrent long searches and tracked reads before changing synchronization |
| H04 | Checking and rebuilding share derivation code (`derive::check`, `derive::rebuild`) | Extend the independent model and inject mutations showing that shared implementation errors cannot pass both paths unnoticed |

## Assessment of the active goal and prospective designs

The active goal's set reads, planner fixes, field typing, measured postings work, and fewer reads per field write
address real costs. Retain that direction. It does not explicitly repair R01–R08, and its acceptance should not be
interpreted as resolving them. Its existing watch-model requirement is valuable but does not cover callback lifecycle
or failed-sync finalization.

| Work | Review requirement before declaring it complete |
|---|---|
| P3: calls per action | Measure complete user actions with view re-runs, crossing counts, and commits; retain before/after conditions |
| P4: indexed filters and streaming | Preserve upstream order, duplicates, scopes, missing/null behavior, and intermediate limits; do not skip prefix dependencies in tracked reads |
| P4: heap top-k | Preserve stable ties and the exact existing ordering contract; compare with full sorting on generated inputs |
| P4: EXPLAIN | Expose actual scanned entries, field reads, materialization, and sorting as well as rows yielded; instrumentation must not change answers |
| P5: field types | Align generated types, runtime validation, schema evolution, existing-data auditing, and unchecked imports |
| P6: postings | Measure term skew, dictionary size, bytes rewritten, and latency alongside corpus size; validate block boundaries and deletions |
| P7: fewer reads per write | Preserve savepoint undo, compound-index values, deferred checks, and text/vector maintenance |
| P8: compression | Separate storage encoding from protocol encoding; audit raw-byte row paths, dumps, check/rebuild, and admin reads; bound decompression allocation |
| P9: substring/regex | Prove candidate selection has no false negatives; define fallback for short strings and regexes without usable trigrams; specify Unicode and case behavior |
| P10: designs | State semantic boundaries, resource costs, failure behavior, measurements, and acceptance cases; naming a predecessor system is insufficient |

Specific design corrections to resolve in P10:

- **Bitmap algebra:** [plan-algebra.md](../roadmap/plan-algebra.md) proposes final-stage ordering and limits, but
  `limit(5).where(...)` differs from `where(...).limit(5)`. Define reorderable regions, ordering/multiplicity properties,
  and semantic barriers. Use bitmap membership within those boundaries rather than treating every plan as an unordered set.
- **Incremental views:** [incremental-views.md](../roadmap/incremental-views.md) needs a successor-access mechanism
  when a top-k member leaves. A heap containing only k results cannot find the replacement. State retained candidates,
  ordered indexes, fallback behavior, and worst-case maintenance costs; cost proportional only to the delta is not assured.
- **Optimistic units:** [javascript-boundary.md](../roadmap/javascript-boundary.md) has a useful ingredient in tracked
  ranges, but also needs read versions, intervening-write history, atomic validation, retry semantics, and side-effect
  guidance. Schema/admin writes and retention of conflict history belong in the contract.
- **Change log:** [change-log.md](../roadmap/change-log.md) must distinguish today's affected-object change sets from
  replayable mutations. Incremental backup needs values, deletions, transaction boundaries, checkpoints, retention,
  schema/format treatment, and verified restoration. Device conflict resolution is a separate product decision.
- **Lazy decoding:** retain owned bytes as the default. Any alternative must measure proxy overhead, retained memory,
  and pinned snapshots alongside decode savings. Compression and lazy decoding should share an explicit value boundary.

## Architecture, maintainability, and API direction

Keep the existing crates, generated protocol, centralized key prefixes, owned results, and transactional index
maintenance. The most valuable structural change is one commit-finalization path: R03 demonstrates a concrete cost of
duplicating it. Do that before broader module rearrangement.

After P4 settles, separate planning, execution, and answer encoding inside the existing engine crate. Give execution
stages explicit ordering, cardinality, tracking, and resource contracts. Keep the implementation simple enough that
an index path can be compared with a scan path. A new framework or crate requires evidence of independent value.

Extend the independent model with generated plan combinations and optimization-enabled/disabled comparisons. Current
fixed query templates do not prove arbitrary planner rewrites equivalent. Include stable ties, missing/null fields,
list membership, duplicate input ids, trash scopes, and limits before and after filters. Persist minimized failures.

Make correct use the default: ordinary open preserves locking; mutation outcomes identify whether commit happened;
observer failure cannot silently strand a subscription; lifecycle calls terminate predictably; and a clean integrity
report means the declared constraints were actually checked. Rust constructors and byte-oriented entry points should
share validation guarantees where they promise the same contract.

## Relevant mature-system comparisons

These comparisons justify particular mechanisms, not adopting another database's entire architecture.

- **LMDB:** its ordered ranges support the bounded cursor traversal in R07. Its explicit environment-open and
  transaction-lifetime requirements are the authority for R02 and the map-growth review.
  [LMDB API contract](https://raw.githubusercontent.com/LMDB/lmdb/mdb.master/libraries/liblmdb/lmdb.h).
- **SQLite:** its testing approach combines fault simulation and independent checks, including comparisons with
  optimizations disabled. Apply that principle to elmdb's planner and audit tests. Its plan diagnostics also provide
  a useful reference for distinguishing scans, indexed searches, and temporary sorting.
  [Testing](https://www.sqlite.org/testing.html), [EXPLAIN QUERY PLAN](https://sqlite.org/eqp.html).
- **FoundationDB:** optimistic transactions validate read/write conflicts and require retry-aware application behavior.
  Use that as a checklist for the proposed async units; tracked-read invalidation alone is not equivalent.
  [Developer guide](https://apple.github.io/foundationdb/developer-guide.html).

## Recommended sequence

This sequence is a proposal for a separate remediation goal after findings are accepted. It neither changes the active
goal's authorization nor authorizes a second implementation agent in its files.

1. **Storage-safety guarantees:** R01 and R02. Done when process-wide registration is atomic, ordinary opens retain
   locking, and the relevant worker/platform cases pass.
2. **Commit and lifecycle semantics:** R03, R05, R06. Done when all mutation entry points share committed-state
   finalization, failed syncs preserve observer/schema consistency, and callback lifecycle behavior is tested.
3. **Trustworthy recovery:** R04 and R07, then R08's measurements and design. Done when the constraint inventory is
   audited, index traversal is bounded per chunk, and recovery's resource envelope is recorded and tested.
4. **Finish the active goal with semantic gates:** use the P3–P10 requirements above and review its final diff against
   this baseline. Done when the new implementation supplies evidence for each changed invariant and records regressions.
5. **Simplify around established contracts:** consolidate query-stage responsibilities and resolve H01–H04 with tests
   or measurements. Done when each hypothesis is fixed, disproven, or explicitly deferred with evidence.
6. **Larger features only with concrete designs:** advance incremental views, replay logs, bitmap layouts, and async
   units when workload evidence justifies them. Each design must state resource bounds and failure/retry semantics.

Before broad release, establish a format-compatibility point, verify restoration from the supported backup/dump paths,
and run the repository's full required checks and binding platform matrix on the accepted fixes. Keep findings linked
to their fix commits and acceptance evidence; green unrelated tests do not close a finding.

## Remediation dispositions — 2026-10-26

This addition preserves the assessment above. Implementation is on the isolated `session/elmdb-remediation` branches;
[the completed remediation goal](../archive/goals/goal-elmdb-remediation.md) owns final acceptance and
[implementation evidence](../evidence/elmdb-remediation.md) records source checkpoints, exact inputs, measurements,
fault injection and firing mutations. Local engine gates pass (254 tests, two measurements ignored; 35 Node tests;
workspace lint, generated protocol, single-LMDB and rebuilt binding checks). Final engine
[platform CI](https://github.com/spankyed/elmdb/actions/runs/40443984290) passes all eight jobs at 9aea4c1.
Final [apack strict-chain CI](https://github.com/spankyed/apack/actions/runs/40444016830) passes all six strict jobs
at 20de66e19f, pinning exact engine source 9aea4c1. Final acceptance is recorded on 2026-10-27 in the archived goal;
the table below records the accepted finding dispositions.

| ID | Disposition | Source and acceptance evidence |
|---|---|---|
| R01 | Fixed: atomic process claim without environment mutation; accepted Windows image stays loaded | `2414390` / `6b839bd`, `elmdb-node/src/process.rs`; worker, duplicate-image/race and independent-process cases; final eight-job matrix passes |
| R02 | Fixed: ordinary read-only opens register readers or refuse; Windows view bounds follow physical file length | `2414390` / `9aea4c1`, `store/mod.rs`; permission/concurrent-reader and mapping-policy regressions; explicit read-only grow refuses; apack CLI fixtures retain writable reader registration. Final engine matrix passes all eight jobs, Windows Rust/Node included; app strict chain also passes |
| R03 | Fixed: committed state/schema and observer finalization precede NOT_DURABLE | `af141ad` / `24be744` (apack `5edd51adf7`), `engine::finish_commit`, schema application and apack committed-error handling; all mutation entry points, fault injection and firing proofs. Reset's atomic markers remain in its mutation; optional physical recycling preserves one logical sequence/notification and required sync |
| R04 | Fixed: primary constraint auditing is independent of whether a derived index can represent a value | `42423db`, `engine/derive/audit.rs`; field inventory, nested values, uniqueness, cycles, cardinality/endpoints/self-links and rebuild-invariant reports; firing mutations |
| R05 | Fixed: initial/later panicking Rust listeners are removed without stranding other watches | `5146508`, `engine/watch.rs`; panic, later-write and independent-listener cases |
| R06 | Fixed: same-database callback close refuses before waiting; teardown tolerates destroyed TLS | `5146508` / `27c76ae`, `Database::close_as`, callback scope; bounded subprocess and thread-exit regressions |
| R07 | Fixed: build chunks seek directly from primary sequence cursors | `42423db`, `engine/derive.rs::build_chunk`; exact cursor-visit growth, interruption/resume and write/trash/delete transitions |
| R08 | Fixed for the measured operating envelope: bounded dump staging, spilled derivation/rebuild, streamed replacement and owned publication | `97e2675` / `bce836a` / `c32a87a` / `614170c`, `dump.rs`, `derive/sorted.rs`, `maintenance.rs`, `store/backup.rs`; 100k/200k rows, vectors and large values, restoration, SIGKILL/retry and failure cleanup. Reference 100k rebuild/replace meet 2 GiB RSS / 30 s; 200k replace takes 47.693 s. These are stated shapes, not universal limits |
| H01 | Confirmed and repaired: schema and snapshot must agree locally and across processes | `7a4df3c` / `744ebf8`, shared schema guards plus persisted fingerprint validation in the transaction; SCHEMA_CHANGED close/reopen contract; deterministic migration/inventory/builder child interleavings and pooled/sync/tracked Node cases |
| H02 | Confirmed and repaired within the explicit publication contract | `c32a87a`, owned temporary/reservation lifetime, atomic publication and Unix directory/Windows write-through handling; concurrent/failure/restoration cases. Callers exclusively own a backup destination against other processes; no universal power-cut claim |
| H03 | Confirmed and repaired with bounded optimistic registration | `5146508`, first query attempts run outside the tracking mutex; validation/registration preserves notification order, with bounded serialized fallback. Alternating token/watch/locked measurements and deterministic firing cases; synchronous callback latency remains caller-owned |
| H04 | Resolved by independent verification, not by trusting shared derivation | `42423db`, independent query/model and primary fixtures; shared count mutation leaves check clean but fails the independent count assertion. Shared check/rebuild remains deliberate and is not a standalone correctness oracle |

The principal-engineering direction remains to keep the architecture and consolidate around these contracts. Preserve
the independent models and fault-injection cases as features evolve. Enforce production-shaped measurements before
optimizing, document resource envelopes by shape, and keep unsafe/environment/filesystem ownership explicit. Current
integration targets landed v1/live-query behavior; future planner, types, compression and search changes belong to the
paused next goal and need their own affected checks when implemented. No rewrite or feature-branch combination is
required to accept these repairs.

## Evidence appendix

The scratch copy was made from the baseline source, excluding build outputs and dependencies. The existing Rust suite
ran with only an inert test-only sync hook added; the review probes were excluded from that suite run. The hook returns
an injected `IO` failure from `EnvSyncer::sync` when requested. No production files in the shared checkout were changed.

| Evidence | Probe | Observed result |
|---|---|---|
| E01 | `review_listener_panic_disables_watch` | Catching a listener panic leaves a registered watch silent on the next write |
| E02 | `review_check_accepts_cycle_loaded_unchecked` | An unchecked-loaded cycle receives a clean integrity report |
| E03 | `review_check_accepts_duplicate_unindexed_unique_values` | Duplicate 257-byte unique strings receive a clean report |
| E04 | `review_reset_commits_without_notifying_when_sync_fails` | Reset commits, returns `IO`, and does not notify its watch |
| E05 | `review_callback_close_cannot_obtain_exclusive_store` | Zero-wait close inside initial delivery cannot obtain exclusive ownership |
| E06 | `review_schema_sync_failure_leaves_memory_and_disk_disagreeing` | Failed schema sync leaves old schema in memory and new schema after reopen |

The patch uses zero context to avoid trailing whitespace in embedded diff context. To reproduce, use a disposable
checkout at the baseline commit, apply the preserved patch there, and run:

```sh
git apply --unidiff-zero /path/to/elmdb-review-2026-10-probes.patch
cargo test --offline -p elmdb review_ -- --nocapture --test-threads=1
cargo test --offline -p elmdb -- --skip review_
```

Use the baseline toolchain and cached dependencies, or prepare dependencies before the offline commands. The first
command's probes deliberately assert the observed defective behavior; **six passing probes reproduce findings, not
correctness**. E01 prints an expected caught panic. Serial execution isolates the process-global failure hook. The
second command produced 210 passed, zero failed, one ignored; the six-probe run produced six passed, zero failed.
There are no benchmark conclusions in these counts.

The patch is historical evidence, not a proposed production patch. Accepted fixes need regression assertions that fail
on the baseline and pass on the corrected behavior, plus the area's required suites. Revalidate all source references
and findings against the implementation agent's final commit before scheduling overlapping work.
