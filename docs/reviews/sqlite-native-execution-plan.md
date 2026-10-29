# Evaluate native Rust execution for EARS over SQLite

Status: proposed, 2026-10-29. This document authorizes no implementation or adoption.

## Objective

Determine whether moving a narrowly selected EARS operation into Rust materially improves
the SQLite candidate's latency, allocations or responsiveness enough to justify owning
another native implementation. Preserve the existing EARS contracts and use SQLite for
storage, transactions and recovery. Do not start by porting the candidate wholesale.

This is a proposed supplementary investigation for apack's `docs/goals/goal-ears-sqlite.md`,
not a replacement for its outstanding production acceptance requirements. The implementation
belongs with that candidate; this document does not propose changing elmdb's LMDB engine.

## Hypotheses and limits

A complete operation crossing into Rust once could avoid repeated JavaScript/native calls,
intermediate objects, repeated decoding and JavaScript traversal. Direct Rust regex execution
could also avoid the current WASM boundary. These are hypotheses, not established wins.

SQLite already executes SQL natively. A thin binding that executes the same statements and
returns the same intermediate rows may make little difference. Existing candidate profiling
places most scalar-probe samples inside the native driver call; that does not identify whether
SQL execution, binding or result construction dominates. Separate those costs first.

Rust does not remove inefficient query plans, broad FTS work, single-writer contention,
checkpoint costs, durable storage flushes or the need to qualify relevance and vector recall.
Moving a synchronous call to a Rust thread while JavaScript synchronously waits still blocks
the caller. An asynchronous JavaScript worker is a separate option that does not require Rust.

## 1. Pin evidence and select one operation

Inspect the current SQLite candidate and its retained evidence before starting. Record exact
apack/elmdb commits, dependency versions, SQLite version and compile options, loaded native
binary hashes, extension versions, database schema, dataset hash and machine/resource limits.
Do not reuse historical numbers as the current baseline.

Select one representative operation with demonstrable JavaScript orchestration or repeated
crossing cost, such as a multi-hop graph read with selective projection. Include a scalar
lookup dominated by SQL as a control. Select fixtures from actual app query shapes and include
small, medium and large result sizes. Include a broad search as an additional control only
if its implementation can be compared without changing ranking semantics.

Measure crossing count, SQL plans and statement count, intermediate and returned rows/bytes,
JavaScript CPU/allocation, native execution time where observable, and end-to-end latency.
Driver spans that include binding and marshalling must be labelled as such.

Before implementation, commit the selected workloads, expected answers, app latency budgets,
trial order, sample counts, resource limits and benefit threshold. Proposed advancement rule:
at least 25% lower median end-to-end latency on the selected target, no more than 5% regression
in its p95/p99 or the scalar control, and no violation of existing app/resource budgets.
These are proposed experiment thresholds, not existing product requirements; agree and freeze
them before collecting results. Retain cold samples and specify how uncertainty is assessed.

## 2. Build a minimal, comparable native slice

Use established crates, provisionally rusqlite and napi-rs. First verify that the binding can
use the same SQLite version/options and compatible extensions as the current candidate.
Record unavoidable differences; do not attribute a SQLite upgrade to Rust.

Implement one read-only EARS operation, not a general query engine. Keep storage layout,
snapshot semantics, ordering, projection, values and expected answers unchanged. Compare:

| Variant | Purpose |
| --- | --- |
| Current candidate | Actual EARS baseline |
| Thin Rust SQL call | Isolate binding differences with the same SQL and result shape |
| Batched Rust operation | Test one crossing and native intermediate execution |
| JavaScript worker, asynchronous call | Test responsiveness independently of language |

The batched variant may reduce orchestration calls, but must disclose all SQL/algorithm changes.
Where feasible, reproduce such improvements in the existing candidate to distinguish a better
algorithm from a language benefit. Do not change multiple design variables without controls.

Keep the new cross-language request/result contract small, defined in Rust with generated
TypeScript. Return owned results; do not expose borrowed SQLite memory. Bound statement
retention, intermediate state and output. Preserve value semantics, error codes and cleanup.
Do not reintroduce request/result MessagePack transport without measuring why it is necessary.

Use a private connection to a disposable fixture or immutable logical copy. Do not mix bindings
on a live database or share raw connection pointers. Read and audit extension loading, close,
thread ownership and duplicate SQLite-library implications before loading both in one process;
separate processes are the default benchmark arrangement.

## 3. Separate responsiveness from synchronous latency

First compare synchronous variants with the current synchronous contract intact. Then evaluate
worker execution as a distinct proposed API change. Measure queue delay, operation time,
end-to-end latency, cancellation, shutdown and event-loop heartbeat under foreground load.

Specify connection ownership, bounded queue/backpressure and read snapshot lifetime. A result
must not outlive a closed transaction through borrowed memory. Do not imply that asynchronous
dispatch preserves synchronous units, read-your-writes or watch settlement automatically.
Any asynchronous API adoption requires an explicit contract and consumer review.

Do not introduce writes merely to make the first slice comprehensive. If the read slice wins,
qualify write batching separately with identical transaction boundaries, durability and commit
acknowledgments. Larger transactions or weaker durability are different experiments.

## 4. Prove correctness and benchmark fairly

Run the same independent contract cases through each applicable variant. Include empty and
large results, null/missing values, ordering ties, Unicode, trash and relation membership,
snapshot consistency, invalid inputs, close/refusal and native resource lifetime. Demonstrate
a relevant safeguard failing on a disposable mutated copy. Worker variants additionally need
queue, cancellation and shutdown proofs.

Use independent processes, balanced interleaved trial order and pinned resource limits. Separate
warm, cold/reopen and sustained-load results. Retain every trial, variance and tail latency;
do not retry unchanged failures into acceptance. Measure RSS, JavaScript heap, result bytes,
CPU, heartbeat and throughput alongside latency. Explain unmeasurable attribution explicitly.

Build native artifacts and run broad checks only in isolated disposable Linux/CI, following
the candidate's existing validation policy. Use independent dependencies, target directories
and fixtures; do not borrow another agent's mutable builds or daily-use profile. Starting
implementation needs a separately arranged isolated checkout in the main conversation.

## 5. Decide before expanding

Produce a short evidence report containing exact source/artifact provenance, correctness
results, raw measurements, confidence/variance, maintenance costs and a recommendation.

- If the thin binding gains little and batching gains little, keep the existing binding.
- If batching wins and equivalent JavaScript/SQL batching captures the benefit, prefer that
  simpler implementation.
- If a narrow Rust operation delivers the preregistered benefit and passes product budgets,
  propose only that operation for integration, including packaging and ownership costs.
- If a worker solves responsiveness adequately, evaluate that independently; it does not
  establish that underlying query latency or durable throughput is acceptable.
- If success requires recreating a general planner, custom index engine or broad duplicated
  EARS implementation, stop and compare that ownership burden with retaining elmdb.

An accepted slice must have one authoritative implementation in the chosen production path,
not permanent duplicate execution logic. Resolve ownership before merging it. No outcome
waives search relevance, vector recall, recovery, durability, platform or final app qualification.

## Deliverables

1. Preregistered workloads, budgets, hypotheses and exact baseline provenance.
2. One isolated native operation with generated types and independent correctness proofs.
3. Balanced measurements separating binding, batching and worker effects.
4. A go/no-go recommendation with maintenance and distribution costs, followed by removal
   of unsuccessful experimental production paths.

## References

- [SQLite WAL behavior and concurrency](https://sqlite.org/wal.html).
- [SQLite synchronous durability settings](https://sqlite.org/pragma.html#pragma_synchronous).
- [rusqlite connection and statement APIs](https://docs.rs/rusqlite/latest/rusqlite/struct.Connection.html).
- [elmdb design and existing cross-language principles](../architecture/design.md).

These references explain mechanisms, not measured performance of this candidate.
