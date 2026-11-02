# Rank index: what it promises and what it costs

The evidence for [the rank index](../roadmap/rank-index.md). 2026-10-31, on `session/rank-index` from `283c805`, on an
Apple M1 Pro (macOS), Rust 1.99.1, release builds, other sessions' builds running (a load average about 5.5).

## Correct

- **Against `rank`** (`crates/elmdb/src/engine/tests/rank_index.rs`): over 48 random histories of up to 30 links,
  unlinks, weights replaced, creates, trashes, restores and destroys over six tasks, each direction, after every step
  the index's scores are within the bound it reports of `rank` run to 1e-14 over the same graph, two scores further
  apart than twice the bound are in `rank`'s order, and `check()` is clean. A scripted graph over three kinds — one
  weighted, one weighted and unique, one plain — holds the same through parallel edges, a weight replaced, an edge
  removed and a node trashed, restored and destroyed, in each direction and without weights.
- **The bound** is `E = 2R / ((1 − α) s)`, `R` the residuals' sum and `s = Σx̂` the estimates'. The index's estimates
  `x̂` and the solution `x` of `x = (1 − α)𝟙 + αPᵀx` differ by `(I − αPᵀ)⁻¹ r`, whose L1 norm is at most `R / (1 − α)`
  since `P` is substochastic. PageRank with dangling nodes' walkers returned to the seeds — every node here — is
  `x / Σx`, with `x > 0`, and a read answers `x̂ / s`, so the index holds no dangling term; and
  `‖x̂/s − x/Σx‖₁ ≤ (‖x̂ − x‖₁ + |Σx − s|) / s ≤ 2‖x̂ − x‖₁ / s` whatever the signs in `x̂`, a removed edge's pushes
  being negative. Over most graphs `s` is near `n`, where the bound is `1/(1 − α)` tighter than `2R / ((1 − α)² n)`,
  the bound with `‖x‖₁ ≥ (1 − α) n` in place of `s`, which also needed every estimate non-negative.
- **Pushing ends:** a node waits to be pushed while its residual is over `θ = tol (1 − α)² / (2 + tol)`. With every
  residual within `θ`, `s ≥ Σx − R / (1 − α) ≥ (1 − α) n − nθ / (1 − α)` since `x ≥ (1 − α)𝟙`, and then
  `E ≤ 2nθ / ((1 − α) s) ≤ tol`: once none waits, the bound is within the tolerance. Pushes stop sooner, once `R` is
  within `tol (1 − α) s / 2`; and a drain owes pushes, and an index counts as unfinished, only while a node waits, so
  rounding can't leave one owing a push it has no node for.
- **`check()` catches damage:** an estimate moved breaks the residual invariant and the score order; a residual moved
  breaks the invariant; totals altered are reported; a rebuild clears each. Every state the invariant holds in is
  within its bound — the proof above — so the comparison with `rank` is what a wrong bound or a wrong system fails.
- **Savepoints:** a savepoint rolled back after links, a create and a trash leaves the index as before it.
- **Crash:** a child process that links the head of a 6,000-node chain to its tail is killed with pushes still
  waiting (a read-only open finds them); the next open drains them, and the index is built and `check()` clean.
- **The same history, the same bits:** the karate club's 78 friendships linked one at a time with their weights, a
  node trashed and restored and two weights replaced, each commit's pushes drained before the next: an exact-bits
  golden, which the CI matrix (Linux, macOS, Windows) holds. Commit pushes and the drain follow one sweep, kept in the
  totals, so the bits don't depend on how many pushes each did.

## Cost

Criterion, `ELMDB_BENCH_ROWS=1000,10000,100000 cargo bench -p elmdb --bench maintenance -- 'graph/'`, the `graph`
group's documents of five `refs` edges each, a rank index over them at the default tolerance, 1e-6:

| Documents | `rank` then `limit(10)` | `ranked` then `limit(10)` | one edge linked and unlinked, each drained | ten linked then drained, ten unlinked then drained |
|---:|---:|---:|---:|---:|
| 1,000 | 1.01 ms | 6.9 µs | 19.0 ms | 27.4 ms |
| 10,000 | 11.3 ms | 7.8 µs | 74.7 ms | 117 ms |
| 100,000 | 143 ms | 8.0 µs | 614 ms | 1.06 s |

- **A read** of the top ten reads ten keys: 7 to 8 µs at every size, 145 to 17,900 times faster than `rank`. The
  heap test holds it to no growth with the graph (`tests/maintenance_heap.rs`, `ranked`: 4,449 bytes at 5,000 and
  10,000 documents).
- **A change** moves nearly every score a little, as an edge in a well-connected graph does to PageRank, so a drain
  reads the graph once — what a `rank` read costs — pushes in memory, about 10 ms a million pushes, and writes back
  every node it moved. One change drained alone costs a few times a `rank`; a lone change also waits out the drain's
  5 ms pause twice in the column above.
- **A burst shares a drain:** a drain waits until 5 ms pass without a commit leaving its index pushes, at most 100 ms,
  so twenty changes in two bursts cost 5.8 ms each at 10,000 documents, about half a `rank`, and 53 ms each at
  100,000, about a third of one.

### The tolerance, and the bound from the estimates' sum

`rankIndexChange/<tolerance>` and `rankIndexBurst/<tolerance>` in the same group: 2026-11-01, the same machine at a load
average of 10 to 16, so a difference under about a tenth is the load. Before is the bound `2R / ((1 − α)² n)`, after is
`2R / ((1 − α) s)`, read on `34732bd` and `66c9de6`, those commits before they were rebased onto the relation key:

| Documents | Tolerance | one change, before | after | ten and ten, before | after |
|---:|---:|---:|---:|---:|---:|
| 1,000 | 1e-6 | 19.6 ms | 20.1 ms | 26.9 ms | 27.0 ms |
| 1,000 | 1e-4 | 19.4 ms | 19.2 ms | 27.7 ms | 27.2 ms |
| 10,000 | 1e-6 | 74.2 ms | 78.1 ms | 116 ms | 117 ms |
| 10,000 | 1e-4 | 77.2 ms | 90.0 ms | 112 ms | 117 ms |
| 100,000 | 1e-6 | 644 ms | 675 ms | 1.11 s | 1.09 s |
| 100,000 | 1e-5 | 665 ms | 661 ms | 1.12 s | 1.11 s |
| 100,000 | 1e-4 | 592 ms | 0.14 ms | 1.11 s | 1.11 s |

- **A drain costs its graph, not its pushes:** a hundredfold looser tolerance leaves every drain's cost where it was,
  since a drain reads every node and edge and writes back every node it moved whatever it then pushes. Pushing less
  buys a drain nothing measurable.
- **What the bound buys is no drain at all:** a change whose residuals a commit's pushes bring within the budget leaves
  nothing to drain. The budget grows with `s`, so the tighter bound reaches that sooner: one change at 100,000
  documents and a tolerance of 1e-4 costs 0.14 ms rather than 592 ms. Ten changes leave more than the budget, so a
  burst still drains.
- **What makes a drain cheaper** is fewer rows to read and write, not fewer of them read: a change that leaves a drain
  moves every node (below, where a drain's work goes). The graph is kept between drains (the edge mirror, below); the
  node rows are block storage's (`docs/roadmap/rank-index.md`).

### A node's place in the score order, compared before its keys are built

The write-back built both of a moved node's order keys to find most of them unchanged (`e570eef`), 8% of a drain in a
sample of 40 drained changes at 100,000 documents (`examples/profile-rank-drain.rs`, `sample` on macOS: about 45%
reading the graph, 37% writing back, 5% pushing). 2026-11-01, the same machine at a load average of 7 to 15, at the
default tolerance:

| Documents | one change, before | after | ten and ten, before | after |
|---:|---:|---:|---:|---:|
| 10,000 | 77.2, 78.4 ms | 76.9, 77.4 ms | 115 ms | 113 ms |
| 100,000 | 625 ms | 584 ms | 1.09 s | 1.03 s |

At 10,000 documents the two builds ran in turn, twice, since one run of each had read 35% apart under a moving load.

### The graph kept between drains

The drains read the graph through the edge mirror (`6604060`), its maps hashed with FxHash (`5d9dbe6`). 2026-11-01, the
same machine at a load average of 4 to 28, each pair of builds run in turn, at the default tolerance:

| Documents | one change, before | after | ten and ten, before | after |
|---:|---:|---:|---:|---:|
| 10,000 | 78.5, 78.2 ms | 63.4, 63.0 ms | 113 ms | 83.1, 83.7 ms |
| 100,000 | 573, 583 ms | 446, 441 ms | 1.02 s | 737, 739 ms |
| 100,000, FxHash | 429, 446, 423 ms | 384, 389, 379 ms | 770, 744, 706 ms | 654, 627, 624 ms |

The last row's "before" is the mirror with SipHash. Sampled at 100,000 documents after the mirror, a drain is about a
third writing back (node rows, 19%; order keys deleted, 8%; their keys, 5%), a quarter reading the node rows, and the
pushes and the mirror's lookups the rest. The node rows, read and written, are what is left to cut.

What the mirror keeps: about 278 bytes an entity at five edges, some 28 MB at 100,000 documents, where an open database
without the index keeps none an entity (`tests/maintenance_heap.rs`, `rankIndexKept`, release, dhat, 2026-11-01, Apple
M1 Pro). Its names are some 91 of that, and each edge is a 24-byte entry at both ends, its weight held as a number no
weight is when it has none. Each list is filled straight from the store and fitted: left with the room it grew into it
kept 350, and collected from the buffer the store was read into, which Rust reuses in place, 542. A list a commit
empties is dropped, and the names are counted afresh from the lists once commits have removed more entries than the
lists hold, so an entity deleted leaves nothing behind and a drain filling the mirror never counts.

Where a drain's work goes, from its pushes and the nodes it moved, logged from an uncommitted copy at 100,000 documents,
2026-11-01: one edge changed moves every node. At the default tolerance a drain makes some 940,000 pushes, writes all
100,000 node rows back and 25,000 order keys. At 1e-4 it makes 9,000 to 93,000 pushes and moves 49,000 to 100,000 nodes,
and a larger share of order keys, up to 93,000, since each push moves an estimate further. Pushing the largest residual
first instead of sweeping halved the pushes, moved as many nodes and took three times as long. So a drain is a whole
rewrite of the index however few edges changed, and reading less of the graph for it saves nothing; fewer rows per
node, which block storage is, does.

### A drain reads what its pushes reach

A drain reads the queue, then each node and its arcs the first time a push reaches it, and reads every node in one pass
once a quarter of them are reached; its totals are kept by its pushes and counted from the nodes once as many pushes as
nodes have added to them. What one change reaches depends on the graph, measured with `examples/profile-rank-drain.rs`
at 100,000 documents and five links each: where each document links others spread across the graph (the benchmarks'
graph), a change pushes about 940,000 times and moves every node; where each links the five after it, about 560 pushes
move about 575 nodes.

2026-11-01, the same machine at a load average of 4 to 9, the edge mirror's build before and this one after, run in
turn, at the default tolerance:

| Documents | Links | one change, before | after | ten and ten, before | after |
|---:|---|---:|---:|---:|---:|
| 10,000 | spread | 56.2 ms | 63.0 ms | 73.2 ms | 75.6 ms |
| 100,000 | spread | 374 ms | 450 ms | 612 ms | 620 ms |
| 10,000 | near | 23.9 ms | 21.3 ms | | |
| 100,000 | near | 128 ms | 21.1 ms | | |

`ranked` is unchanged, 7.6 and 8.1 µs. Over near links a change costs its commits and the drain's 5 ms wait for them to
pause, twice, whatever the graph's size. Over spread links a drain reading every node pays for numbering what it
reached and for the queue's bitset over a sweep of the nodes in order, 12 to 20% on one change and nothing measurable
on a burst; the profile's remainder is reading the node rows into the maps a drain that reads only some of them needs.

How long a drain holds the write lock follows what it reaches: over near links, the some 575 nodes it reads and writes.
Over spread links a drain still holds it for the whole drain, and splitting it into shorter units would write every node again in each,
since every unit's pushes reach nearly all of them; what would shorten it is deriving off the write lock.

### What it took to get there

The first build pushed through the store, one node read and written at a time, and its pushes took the first waiting
node each time:

- **Push order:** taking the first waiting node let a hub and the node before it hand residual back and forth while
  the rest waited, the pushes growing geometrically with the hub's degree: 2.07 million for a twelve-leaf star at a
  tolerance of 1e-9. A sweep, kept in the totals across commits and drains, takes 742.
- **Pushing through the store:** one edge linked and unlinked cost 274 ms at 1,000 documents and 3.1 s at 10,000,
  some 13,000 pushes a change at about 10 µs each. Draining in memory, in one unit, and keying the score order on 16
  significant bits so most small moves leave it alone, took that to 6 ms and 61 ms before the pause; the pause took a
  burst of ten from 580 ms to 117 ms at 10,000.
