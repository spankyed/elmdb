# Read allocations, copies and codecs

Status: planned, 2026-10-29. Documentation only. Surveyed elmdb `main` at
`6007d8607372716ec8e25c302b60610c20411dff`; inspect current source before implementation.

## Outcome and relationship to existing plans

Identify where representative reads allocate, copy or repeat codec work, then remove only demonstrated
waste with simple internal changes. Preserve storage bytes, generated protocol, public API, validation,
errors, snapshot behavior and owned results. No gain is assumed; retaining current code is a valid outcome.

This is the focused implementation path for [lmdb-js research L02](lmdb-js-research.md#l02-allocations-copies-and-repeated-codec-work).
That document retains the broader survey and write-path research. This plan owns read profiling and
any selected read fixes; do not create another independent L02 investigation. It does not activate other
research streams or the lazy-row/optimistic-unit designs in [the JavaScript boundary](javascript-boundary.md).
[Query work efficiency](query-work-efficiency.md) owns reducing how much work the plan requests.
apack's companion `docs/plans/ears-query-efficiency.md` owns application recipes and JavaScript handling.

## Questions and scope

For each workload, locate allocation counts/bytes, peak live memory and CPU/elapsed cost across:
plan decoding and validation; primary/index reads; field assembly and decompression where applicable;
answer encoding; N-API ownership transfer; JavaScript decoding and EARS result construction/freezing.
Distinguish a measured allocation from an inferred copy. Distinguish peak live bytes, total allocated
bytes, RSS and LMDB mapping size. They answer different questions.

Start from `crates/elmdb/src/engine/reads.rs`, `values.rs`, the engine byte entry points and
`crates/elmdb-node/src/lib.rs`; follow actual current call paths rather than assuming each stage exists
as an independent allocation. EARS connection processing is measured in apack, without importing
application-specific concepts into Rust.

| Workload | Purpose |
|---|---|
| One entity/field | Fixed crossing, request validation and small-answer overhead |
| A 50-message page | Representative bounded read with small and large fields |
| Projected list versus the same whole rows | Separate avoided data from codec/assembly efficiency |
| Large materialized result used by export-like consumers | Expose copy/allocation scaling; explicitly not a dump-streaming benchmark |

Fixtures include scalars, long strings, nested maps/lists, binary fields, missing and null fields and
compressed values where supported. Fix seed, schema, sizes, selectivity and checksums. Larger inputs
must stay within a declared memory/time envelope. Do not use active developer databases.

## Method and stopping rule

1. Pin source, dependency/native build identities, release profile, runtime, machine/resource limits,
   fixture and cache/warmup conditions. Verify the loaded binary matches the claimed source.
2. Establish unprofiled elapsed-time and work baselines. Separately collect CPU and allocation profiles
   using supported tools for the execution host; begin with existing benchmark tooling. Record profiler
   configuration and overhead. Run native and JavaScript profiles separately when necessary and explain
   boundaries that cannot be measured independently without changing execution.
3. Produce a cost breakdown with call sites and allocation/copy evidence. Declare the candidate's target,
   meaningful-benefit threshold and acceptable CPU/memory tradeoff before implementing it. Base the
   threshold on the workload and observed variability, not a universal percentage.
4. Evaluate at most two localized candidates. Use balanced baseline/candidate trial order, fixed inputs
   and raw samples. Instrumentation-heavy profile timings do not prove uninstrumented speedup.
5. Confirm promising changes on the other workloads and an application-shaped EARS read. Stop when the
   selected candidates are accepted or rejected. An inconclusive result closes with its limitation and
   reopening trigger; it does not authorize a search for more optimizations.

Prefer eliminating an unnecessary temporary collection, repeated traversal or ownership-neutral copy.
Buffer sizing/reuse is a candidate only if measured: account for oversized answers, retained capacity,
reentrancy, worker concurrency and error cleanup. Internal borrowing is permitted only inside the
transaction lifetime and with normal Rust ownership guarantees. Owned answer bytes still cross out.

A necessary validation pass, decompression or copy establishing ownership is not automatically waste.
Do not add caches, unsafe public reads, V8-specific fast calls, lazy rows, a custom serializer or new
storage/protocol encoding in this work. If the profile points there, record a separate proposal rather
than expanding these fixes.

## Checkpoints and acceptance

| Checkpoint | Deliverable / dependencies | Done when |
|---|---|---|
| Baseline and profile | Reproducible fixtures, release baseline and profiles; consumes current read/codec implementation | Costs attributed with uncertainty, profiler overhead and unsupported counters explicit; no speedup claim from profiling alone |
| Candidate decision | At most two localized changes selected from profiles | Each names redundant work, invariants, predicted benefit, complexity and predeclared acceptance; no-change is allowed |
| Implement selected fixes | Independent reviewable commits; depends on candidate evidence | Matched trials show worthwhile improvement/tradeoff; no unexplained regression on other workloads; semantic and failure cases pass |
| Integrated closure | Source-specific evidence, required checks and resulting docs | Every candidate has a disposition; exact committed source passes required engine/binding checks; cross-repo JavaScript changes have their own acceptance |

Correctness checks cover exact result/value equivalence, row/field ordering where contractual, nested
and binary values, malformed protocol refusals, missing versus null, units/read-your-writes, watch
tracking, and reads surviving subsequent commits/database close without borrowed memory. Exercise
reentrancy/concurrency and oversized/error cases if reusable state changes. Reuse existing model and
protocol tests where they cover the invariant; add focused gaps rather than duplicating the suite.
For each new meaningful regression check, demonstrate its failure on a mutated disposable copy as
required by [repository guidance](../../CLAUDE.md).

Run focused checks while developing and the complete engine-change gates in CLAUDE.md before landing
engine changes. Timing evidence is a controlled comparison, not a noisy ordinary-test assertion.
Coordinate expensive execution with other sessions and respect apack's execution rules for app-native
runs. Documentation-only changes need link/diff review, not builds.

## Evidence and current handoff

Create `docs/evidence/read-allocation-codec.md` when measurements begin. Record source/binary identities,
commands, fixture generator, environment, raw samples/profiles, allocation definitions, semantic checks,
failed trials, candidate dispositions and limits. Keep historical evidence separate from this plan.

Plan only. Next: inspect current read and codec call paths, select the four bounded fixtures, establish
baselines and profile them. No performance result or implementation is claimed.
