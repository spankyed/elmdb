# Research plan: lessons from lmdb-js

Status: planned, 2026-10-26. The initial survey was read-only. This document schedules research and design decisions,
not production implementation, and does not reopen completed remediation.

## Purpose and relationship to other work

Identify mechanisms in lmdb-js that could improve elmdb's performance, resource behavior, and maintainability while
preserving its contracts: Rust owns behavior, a plan/batch crosses once, results own their bytes, index maintenance is
transactional, and refusals occur at the operation that caused them.

This research plan is separate from the [completed remediation](../archive/goals/goal-elmdb-remediation.md) and
[goal-elmdb-next.md](../archive/goals/goal-elmdb-next.md), closed 2026-10-28. Their acceptance does not depend on this research. After evidence exists, propose a specific implementation
in its owning goal or roadmap; adding a required deliverable to a goal needs user authorization. A demonstrated
correctness issue should be reported promptly with its reproduction and existing requirement, without silently adding a performance project.

The [engineering review](../reviews/elmdb-review-2026-10.md), [JavaScript boundary](javascript-boundary.md),
[text-index scaling](text-index-at-scale.md), and [testing strategy](../architecture/testing.md) are related owners.
Link to their decisions rather than duplicating or overriding them here.

## Survey provenance and limits

- Companion checkout: `/Users/spankyed/Develop/Projects/apack/lmdb-js`, package lmdb 3.5.9, commit
  `963d7d2c2cd17e30836ad64bad00eb2aa1324aab`, clean when inspected on 2026-10-26.
- elmdb reference: committed remediation checkpoint `2414390`, documentation through `45ea0d5`. The survey also
  read the source checkout's unchanged read/codec paths. Concurrent Phase 2 edits are not completed behavior or
  experimental evidence. Pin fresh exact commits before running experiments.
- This survey inspected source, tests, README and workflow definitions. It did not run lmdb-js tests or comparative
  benchmarks. No throughput ranking, crash-safety ranking or verified cross-platform claim follows from it.
- lmdb-js is a general key/value binding. elmdb adds entity/relation rules, derived indexes, query execution and tracked
  reads. A raw get comparison is a boundary measurement, not a comparison of equivalent application work.

## Initial comparison

| Mechanism | lmdb-js source observation | elmdb opportunity or tradeoff |
|---|---|---|
| Small reads | Reusable buffers, decoder lifetime choices, optional V8 fast calls | Measure allocation and codec passes first; keep owned JavaScript results and N-API portability |
| Read snapshots | Explicit reset/release and retained versus renewing cursor bookkeeping | Bound cached idle snapshots without expiring active reads or explicit units |
| Writes | Native instruction queue, event-turn batching, off-thread work and backpressure | Consider only an explicit async contract if workload evidence warrants it |
| Cold reads | Off-thread prefetch followed by decoding | Evaluate elmdb's existing async read before adding a prefetch API |
| Decoded cache | Optional object caching, including validation against transaction metadata | Any proposed cache must preserve snapshots, isolation, mutation and invalidation rules |
| Compression | LZ4, dictionaries, thresholds and reusable buffers | Compare end-to-end costs with proposed zstd, including allocation and event-loop delay |
| Concurrency/lifecycle | Focused regressions for cursor renewal, close with reads, workers and lock ordering | Borrow test subjects and invariants rather than native ownership machinery |
| Conditional writes | Version conditions support atomic compare-and-write operations | Research separately from holding an LMDB writer across await; do not assume they are equivalent |

elmdb's strengths to retain are one plan/batch crossing, engine-owned constraints and derived indexes, generated
cross-language contracts, owned results, and explicit grouping. Both products inherit much of their storage behavior
from LMDB. Rust, additional tests, or more functionality alone do not establish greater reliability.

## Research method and stopping rules

1. Freeze elmdb, lmdb-js, monorepo, dependency and generated-binary inputs. Record machine, runtime, data shape,
   durability settings, build profile and cache state. Check that the executable matches the source.
2. Start each workstream with one explicit question and a measurable acceptance rule. Set workload-based bounds before
   optimizing; explain them using the supported app/profile workload. Do not choose thresholds after seeing a result.
3. Run a baseline, one focused mechanism experiment, and a confirmatory comparison where needed. Extend only to
   diagnose a named failure or test a changed relevant input, not to keep searching for a favorable result.
4. Report observed effects, semantic differences and uncertainty separately. A negative or inconclusive result closes
   the research question with its limitation and a concrete reopening trigger; it does not trigger a redesign.
5. Use production-shaped copies and synthetic controls. Compare identical application behavior and durability where
   claiming product gains; label lower-level experiments that intentionally remove engine work.
6. Use repository measurement commands and idle checks. If busy, alternate versions and record conditions. Coordinate
   one expensive execution owner; do not contend with active remediation validation or restart paused goal-next work.
7. Keep all database writes in disposable copies. Use bounded subprocesses for crash/deadlock tests, preserve output,
   and follow monorepo's native/Linux execution rules. Never run the existing benchmark scripts blindly: inspect their
   test-data paths and cleanup behavior first. Never drop system-wide caches on the developer machine.

Evidence belongs in `docs/evidence/lmdb-js-research.md`, created when experiments actually run. Record source inputs,
commands, data generators, results and limitations there. This plan remains the question/decision register.

## Workstreams, in priority order

### L01: idle snapshots and page retention

**Question:** can elmdb's cached read transaction pin old pages indefinitely when its process goes idle while another
process writes, and what policy bounds retention without erasing the read-cache benefit?

**Source:** lmdb-js `read.js` (`renewReadTxn`, `resetReadTxn`, `useReadTransaction`), `src/txn.cpp`, and the
`Renewing cursor snapshot release` cases in `test/index.test.js`; elmdb
[store/mod.rs](../../crates/elmdb/src/store/mod.rs) (`read`, `current_idle`, `drop_idle`) and
[store/tests.rs](../../crates/elmdb/src/store/tests.rs).

**Experiment:** one process reads once and then performs no further database operations. Another repeatedly overwrites
fixed-size values in the same database. Compare normal cached reads with a no-idle-cache control, first with one
reader and then several readers. Observe reader transaction IDs, committed IDs, allocated/free pages and file growth
across repeated batches; distinguish map reservation, physical file size and page reuse. Also test a local commit racing
with completion of an older read. Do not confuse fresh answers on the next read with release of a currently idle snapshot.

**Decision:** if retention is demonstrated, compare bounded idle-cache release with an upstream-supported reset/renew
mechanism. Recheck pinned heed capabilities; do not assume an API exists or patch dependencies as the first step.
Choose the idle-retention bound from app/tool usage before prototyping. Active snapshots and explicit units retain their
existing semantics. Report latency/allocations with retention and disk behavior, not latency alone.

**Done when:** a deterministic reproduction or disproof, a measured policy comparison, the selected design or rejection,
and regression criteria. A production fix is a separately owned deliverable, not this research plan's implicit next step.

### L02: allocations, copies and repeated codec work

The focused read investigation and selected internal fixes are owned by
[Read allocations, copies and codecs](read-allocation-codec.md). Use that plan for execution and
read-specific evidence; the write-path questions below remain research here. Do not run duplicate read investigations.

**Question:** which redundant operations dominate an actual elmdb read/write, and which can be removed without changing
validation, errors, storage bytes or result ownership?

**Source:** lmdb-js `read.js` (`getBinaryFast`, `getBinary`, `get`, `retain`), `open.js` (encoder setup), and
`src/dbi.cpp`; elmdb [unit.rs](../../crates/elmdb/src/engine/unit.rs) (`Read::get`),
[values.rs](../../crates/elmdb/src/engine/values.rs) (`check_spec`, `encode`),
[engine/mod.rs](../../crates/elmdb/src/engine/mod.rs) (byte decoders), and
[reads.rs](../../crates/elmdb/src/engine/reads.rs) (row/projection encoding).

**Experiment:** instrument allocations, allocated/copied bytes, codec passes and elapsed time for one field, one row,
a projected set read, a field update, and a grouped batch. Include small scalars, long strings, nested values and binary
fields. Attribute costs separately to crossing, validation, primary reads, indexes, serialization and JS decoding.
Test direct validation of already decoded values and internal transaction-scoped borrowing as isolated prototypes.
If using scratch buffers, account for decoder-retained data, reentrancy, concurrent reads and oversized values.

**Decision:** prefer an internal change preserving owned results and generated protocol over a new public fast/unsafe API.
A proposal must preserve malformed-input refusals and equivalent Rust/byte API validation. Shared-record or columnar
encoding is a separate format/protocol proposal; msgpackr extensions cannot simply enter elmdb's current value contract.

**Done when:** a cost breakdown, one evidence-backed candidate or a reason to retain the current implementation, and
semantic/failure cases for the candidate. Do not infer a product gain from removing validation in a microbenchmark.

### L03: cold reads and event-loop responsiveness

**Question:** does disk-backed read latency hold up the app, and is elmdb's existing asynchronous read sufficient?

**Source:** lmdb-js `read.js` (`getMany`, `prefetch`) and native prefetch code; elmdb
[binding](../../crates/elmdb-node/src/lib.rs) (`read`, `read_sync`, `ReadTask`) and monorepo EARS call sites.

**Experiment:** compare warm and controlled cold-ish reads using isolated data larger than the selected memory budget
or otherwise documented cache conditions. Use single rows, projected set reads and representative searches. Measure
throughput, p50/p95/p99 response time, event-loop lag, page faults and memory. Compare existing synchronous and async
elmdb reads before prototyping prefetch. Pin snapshot expectations; prefetch must not silently change the read's contract.

**Done when:** identify a real affected call site, select existing async reads, a justified prefetch design, or no change,
and document scheduling/cancellation/error behavior. No new API solely because lmdb-js has one.

### L04: compression as an execution policy

**Question:** what combination of codec, threshold, buffer policy and execution location benefits actual stored fields?

**Source:** lmdb-js `src/compression.cpp`, `write.js` and `open.js`; elmdb
[format.md](../architecture/format.md), Values, and goal-next Phase 8. This research supplies a comparison to Phase 8;
it neither starts that paused phase nor overrides its zstd decision without evidence and reconciliation.

**Experiment:** sample field-size/entropy distributions from copied production-shaped data. Compare uncompressed data,
LZ4 and zstd using the same logical values; include incompressible binary, tiny values and large message/tool content.
Measure size, compression/decompression time, allocation, temporary memory, writer occupancy and event-loop effects.
Include dictionary availability/rebuild costs, incompressible expansion, and long-read behavior. Compare synchronous
work with off-thread prototypes only under explicit mutation semantics.

**Done when:** a codec/threshold/buffer decision matrix with measurements and a proposed design for the owning phase.
The design must define tags, size bounds, damaged-payload errors, dictionary identity and export/recovery behavior.
README throughput claims are not substitute measurements.

### L05: lifecycle regressions and stale readers

**Question:** which lmdb-js lifecycle test subjects expose gaps in elmdb's contracts or current test coverage?

**Source:** lmdb-js `test/index.test.js` (close with reads in flight, cursor renewal), `test/threads.cjs`,
`test/readonly-threads.cjs`, `test/cross-env-deadlock.cjs`, and `src/txn.cpp` (reader-full recovery); elmdb's binding
exit/process tests, database close tests, and heed's `clear_stale_readers` API.

**Experiment:** map each subject to an explicit elmdb invariant and existing tests before adding cases. Candidates are
async read/close overlap, repeated close, worker exit with pending work, map growth with readers, multiple-database
lock ordering, and a killed child leaving a stale reader slot. Distinguish dead readers from live pinned readers.
Evaluate at most a bounded cleanup/retry on READERS_FULL; preserve a clear failure when live readers exhaust capacity.
Do not transplant lmdb-js's timing assumptions or polling into elmdb tests.

**Done when:** a coverage matrix and minimal reproductions for genuine gaps, plus a stale-reader recovery/diagnostic
recommendation. Each proposed regression has an observable invariant and a named firing edit on a disposable copy.
Do not count the presence of a mature project's test as proof that elmdb lacks coverage or needs its implementation.

### L06: caching, conditional writes and explicit async writes

**Question:** is there application demand that justifies the consistency and scheduling cost of these larger features?

**Source:** lmdb-js `caching.js` (validated reads and pending writes), `write.js` (conditional operations, batching,
commit/flush promises and backpressure), `src/writer.cpp`, and the asynchronous transaction ordering documentation.
Compare with the existing [boundary decisions](javascript-boundary.md) and elmdb's committed-error contract.

**Research first:** identify repeated decode-heavy call sites and actual workflows needing conditional updates or async
writes. Quantify decoding saved versus validation/invalidation costs. For async writes, measure writer pressure and
queue growth; separate visible commit from durability and specify cancellation, shutdown and committed-error outcomes.
Conditional writes, writer-held async callbacks and optimistic units are different contracts; document which one a
proposed workflow needs. Do not describe lmdb-js's asynchronous callback transactions as proof of an optimistic design.

**Decision:** any cache proposal covers snapshots, write units, failed/rolled-back writes, external commits, schema/admin
changes, object mutation and memory bounds. Any queued-write proposal is explicit and bounded; it preserves the current
synchronous API. Native pointer sharing between binding copies and V8-specific calls remain separate high-cost options.

**Done when:** a concrete application need and scoped design with correctness/resource acceptance, or an explicit
not-now decision with a reopening trigger. This stream must not become an open-ended API redesign.

## Comparison controls

Maintain two distinct result sets:

- **Mechanism comparisons:** native crossings, copied bytes, decoder behavior, reset/renew, compression and raw LMDB
  calls. State deliberately omitted work and whether V8 fast paths or decoded caching were enabled.
- **Application comparisons:** equivalent entity/field layout, index maintenance, validation, transaction grouping,
  durability, result ordering, read-your-writes and observer behavior. Include per-action calls and time so a raw-get
  advantage does not stand in for a whole-plan result. Do not rebuild the removed EARS implementation just to create
  a competitor; narrow or qualify a comparison that lacks an equivalent implementation.

Record active readers, database size relative to RAM, schema/index configuration, result ownership, compression and
cache modes. Inspect lmdb-js's vendored LMDB format/build options and elmdb's heed/LMDB before attributing differences
to the bindings. Never open one product's database files through the other to make a benchmark convenient.

## Deliverables and completion

| Deliverable | Required proof |
|---|---|
| Survey verified on pinned inputs | Source locators, observed behavior, assumptions and test coverage separated |
| L01–L05 conclusions | Reproduction/measurement or explicit negative/inconclusive result; no unsupported performance claim |
| L06 disposition | Concrete demand and design, or not-now with a reopening trigger |
| Ranked proposals | Expected benefit, measured cost, invariants, complexity, risks, dependencies, owner and acceptance checks |
| Owner reconciliation | Link proposals to remediation/goal-next/roadmap without silently changing those goals' scope or pause |
| Research closure | Evidence commands/results and an Outcome recording what was learned, rejected and proposed |

No production fix or speedup is claimed until separately implemented and validated. The research is successful if it
produces defensible decisions, including decisions not to adopt mechanisms that cost more than their workload benefit.
