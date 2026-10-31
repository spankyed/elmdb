# Query work efficiency

Status: done, 2026-10-29: [the readings](../../evidence/query-work-efficiency.md).

## Outcome and existing guarantees

Retain indexed filtering, streaming early termination, bounded top-k and projections, and extend them
only for demonstrated costly queries. Reduce visited entries, materialized rows or returned bytes
while preserving the exact query contract. No speculative optimizer rewrite is scheduled.

The existing [engine contract](../../architecture/engine.md#reads-plans) and
[plan algebra](../../roadmap/plan-algebra.md) document built behavior: an indexed filter among leading filters can
drive a type start, matching entries stream to early terminals/limits, bounded sorting retains top-k,
and row projections return requested fields. First prove whether an affected plan already uses these
capabilities before changing execution. Existing completed work is not reopened by this plan.

This document owns workload admission, bounded execution fixes and work regressions. Plan algebra
continues to own cardinality selection, bitmap storage and set execution designs; its larger phases
are not activated here. [Read allocation/codec](read-allocation-codec.md) owns making necessary result
work cheaper. apack's `docs/plans/ears-query-efficiency.md` owns finding consumers, narrowing requests
and integration. A caller-only fix need not become a Rust change.

## Admit a query before extending execution

Use at most three application-derived queries from the companion apack investigation. If none is
available, inspect existing evidence and build an explicitly synthetic control; do not claim app impact.
Each query dossier must include:

- Exact plan and source caller, fixture generator/seed, schema/indexes, size and data distribution.
- Expected values, order, multiplicity, pagination/limit behavior and snapshot/watch dependencies.
- EXPLAIN stages where supported, actual visits/kept rows, returned bytes, unprofiled timings and memory.
- Named costly operation and a falsifiable hypothesis: missed index use, avoidable materialization,
  late limit/projection, or a caller request that defeats existing execution.
- A predeclared work target and latency/memory tradeoff, with reproduction and a finite stopping rule.

EXPLAIN timing itself has overhead. Cross-check ordinary execution and distinguish entries visited,
fields decoded, rows materialized and rows returned. Do not infer early termination from a short answer.

## Change policy

First retain or restore existing intended execution for the admitted plan. Prefer a localized change
using existing indexes and plan operations, with public API and storage unchanged. Reordering is legal
only when it preserves filtering, order, ties, duplicate handling, errors and snapshot behavior.
A limit must not be pushed before a predicate that changes its answer. Projection must not remove
fields needed for filtering/sorting or their tracked dependencies.

Treat first/exists/count, offset/cursor paging, relation walks, ordering ties, null/missing fields,
trashed entities and mixed starts according to their actual current contracts. Count need not stop
where existence can. JavaScript predicates remain an explicit application execution boundary; do not
pretend their behavior can be moved into Rust without an equivalent plan operation.

For a planner extension, compare estimated and actual work on both selective and unselective data.
Account for planning overhead on small queries. New statistics/index maintenance entails write, disk,
check/rebuild and schema-lifecycle costs, not only a faster read. If a candidate requires such a broader
change, hand it to the owning plan with a reproduction and explicit scope decision; this plan cannot
silently authorize new layouts, bitmap execution or a general cost-based optimizer.

## Checkpoints and acceptance

| Checkpoint | Deliverable / dependencies | Done when |
|---|---|---|
| Query admission | At most three dossiers, from app callers or labeled controls | Existing behavior verified; observed waste and expected reduction named before optimization |
| Minimal fix or no-change | Localized execution candidate, or caller-only/rejected disposition | Answer/order/error equivalence established; counters show the predicted reduction; planning and small-query costs measured |
| Regression proof | Focused semantic and work checks using existing model/plan harnesses | Tests cover relevant ties/limits/projection/invalidation; a deliberate loss of the optimization on a disposable copy causes the work guard to fail |
| Integrated closure | Required engine/binding gates and companion application verification | Exact committed inputs recorded; candidate tradeoffs, unsuccessful cases and unresolved integration are explicit |

Use work counters as deterministic regression guards. Counter checks must cover ordinary execution or
establish that EXPLAIN follows the same execution, so explanation-only optimizations cannot pass.
Differential checks compare with an unoptimized/reference path across varied distributions, including
empty data and adversarial ties. Tests assert declared behavior, not one preferred internal plan
where multiple plans satisfy the same work contract.

A result-preserving optimization must also preserve tracked reads: changes to fields used solely for
membership/order still invalidate watches. Validate relevant writes, unit rollback and schema/index
changes where execution dependencies change. Follow [testing guidance](../../architecture/testing.md)
and [required engine checks](../../../CLAUDE.md). No ordinary timing gates or storage changes in this plan.

## Evidence, stopping rules and current handoff

Create `docs/evidence/query-work-efficiency.md` when execution begins. Record source/build identities,
commands, fixtures, runtime/resources, raw samples, work/complexity before and after, memory, write
costs where affected, correctness checks and limitations. Explain asymptotic claims using the variables
that actually change (candidate count, page size, sort input and returned bytes).

Stop after the admitted query set. Close non-beneficial candidates with a reopening trigger. New
queries or larger optimizer mechanisms are additional scope, not an obligation to keep tuning.

Done, 2026-10-29: [the readings](../../evidence/query-work-efficiency.md). apack admitted three queries; two were
caller-only. The third, a thread's last messages, admitted one engine change: a hop streams its edges, and `reverse`
straight after a hop from one entity over one kind reads them from the last, so `reverse` then `limit k` costs `k`, not
the list. Not admitted, each with its reopening trigger there: `orderBy` over a walk, `after`, index entries reading
their records, `count` after a hop.
