# Query work efficiency: readings

The evidence for [Query work efficiency](../roadmap/query-work-efficiency.md). 2026-10-29, on `session/bounded-walk`
from `5f65c43`. Machine: Apple M1 Pro, 10 cores, 32 GiB, macOS 26.3.1, Node 23.11.1, the binding built in release.
Load averages 9 to 15 throughout, so the times are readings, and the work counts are the evidence.

## The admitted query

apack's companion investigation (`docs/references/evidence/ears-query-efficiency.md` there) measured four views and
admitted three. Two of them, the thread list and the notes search, were caller-only fixes. The third, opening a chat
thread, needed the engine:

- **Plan:** `ids [thread] → out(contains, Message) → reverse → limit k → rows [status]`, the thread's last messages.
- **Costly operation:** `hop` collected the entity's whole edge list (`relations::edges`, a `scan`) before yielding its
  first far end, and `reverse` collected its whole input. Both are O(M) for a thread of M messages, whatever `k` is.
- **Hypothesis:** a forward hop that visits its edges as it reads them stops at a `limit`. A `reverse` straight after
  a hop from one entity over one kind can read that entity's edges from the last, so `k` edges cost `k`, with no
  storage change.

## Change

- `Read::visit_back` visits a prefix's entries from the last; `tracking::Recording` records a stopped one from its
  stop to the range's end.
- `relations::visit_edges` visits an entity's edges in place order or backward, stopping when its visitor does.
- `hop` streams through it.
- `Step::Reverse` straight after `out`/`in` asks the hop to walk backward. The hop does when it has one source and one
  kind, and the `reverse` then passes ids on as they come; otherwise the `reverse` collects as before.
- For a kind that isn't `unique`, a far end joined by several edges is visited at its first edge's place
  (`relations::first_between`), as the forward walk visits it.

## Readings

Through the binding in apack's test runtime, one thread of M messages, medians of 31 runs:

| Read | M = 1,000, before → after | M = 2,000 | M = 4,000 |
|---|---|---|---|
| `out(...).reverse().limit(54).pick(['status'])` | 0.87 → 0.16 ms | 1.74 → 0.16 ms | 3.65 → 0.15 ms |
| `out(...).limit(54).pick(['status'])` | 0.32 → 0.10 ms | 0.58 → 0.09 ms | 1.11 → 0.10 ms |
| `out(...).pick(['status'])` (the whole list) | 1.54 → 1.59 ms | 2.97 → 3.28 ms | 6.25 → 6.66 ms |
| apack's chat page, which moved onto the first read | 1.52 → 0.18 ms | 3.05 → 0.18 ms | 6.36 → 0.17 ms |

The whole-list read was not a target. Its after readings were taken at load 14 against 12 before, within this
machine's spread.

## Checks

- **Equivalence** (`crates/elmdb/src/engine/tests/walks.rs`): a hop reversed whole equals the forward walk reversed,
  and a reversed hop with `limit` of 0, 1, 5, the length and past it equals its prefix. This holds for an outward and
  an inward hop over a kind that isn't unique and one that is, with and without a type filter. An end linked twice
  comes at its first edge's place, a trashed end is left out, and a hop from several entities or kinds still
  reverses the whole walk.
- **Work:** EXPLAIN's hop stage keeps 5 for `reverse, limit 5` over a 400-edge list. EXPLAIN runs the same stages with
  a counter around each.
- **Tracked reads:** a stopped backward read is written into by a new last edge and by unlinking an edge it kept.
- **Mutations**, each on a disposable copy, each failing:
  - the hop never asked to walk backward fails the work check;
  - the first-edge check removed fails the duplicate, equivalence and tracked cases;
  - the tracked range recorded on the wrong side of the stop fails the tracked case.
- **Tracked range, pinned further:** over a unique kind, which reads no edge's ends, an unlink of the edge the read
  stopped at writes into it, and one before it doesn't. A unit's read walks back over its own new edge.
- **CI:** the in-diff mutation jobs, Linux and Windows, caught 42 of 42.
- **Suites:** `cargo nextest run --workspace` passed all 345 tests, the five new ones and the model harness among them.

## Not admitted

- `orderBy` over a walk still reads every candidate, and `after` still scans from the start.
- `holding` still reads each index entry's entity record.
- `count` after a hop is still O(M).
- Plan algebra owns the larger designs.

Reopen with a measured query that leads on one of them.
