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
- **A change** moves nearly every score a little, as an edge in a graph whose links are spread does to PageRank, so a
  drain reads the graph once — what a `rank` read costs — pushes in memory, about 10 ms a million pushes, and writes back
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

- **A drain over spread links costs its graph, not its pushes:** a hundredfold looser tolerance leaves every drain's
  cost where it was, since it reads every node and edge and writes back every node it moved whatever it then pushes. Pushing less
  buys a drain nothing measurable.
- **What the bound buys is no drain at all:** a change whose residuals a commit's pushes bring within the budget leaves
  nothing to drain. The budget grows with `s`, so the tighter bound reaches that sooner: one change at 100,000
  documents and a tolerance of 1e-4 costs 0.14 ms rather than 592 ms. Ten changes leave more than the budget, so a
  burst still drains.
- **What makes a drain cheaper** is fewer rows to read and write, not fewer of them read: a change that leaves a drain
  moves every node (below, where a drain's work goes). The graph is kept between drains (the edge mirror, below). Fewer node rows, in blocks of 64 slots,
  didn't pay on this base (2026-11-02, #55): top-k reads about 60% slower, a burst 5 to 8% slower, and one change at
  100,000 no faster.

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
first instead of sweeping halved the pushes, moved as many nodes and took three times as long. So over those links a
drain is a whole rewrite of the index however few edges changed, and reading less of the graph for it saves nothing.

### A drain reads what its pushes reach

A drain reads the queue, then each node and its arcs the first time a push reaches it, and reads every node in one pass
once a quarter of them are read or would be; its totals are kept by the changes to its nodes and counted from them once
as many changes as nodes have added to them. What one change reaches depends on the graph, read from a counter in an
uncommitted copy at 100,000 documents and five links each: where each document links others spread across the graph (the benchmarks'
graph), a change pushes about 940,000 times and moves every node; where each links the five after it, about 560 pushes
move about 575 nodes.

2026-11-02, the same machine, main and this branch run in turn twice each at a load average falling from 35 to 5, the two
runs' medians, at the default tolerance:

| Documents | Links | one change, main | this | ten and ten, main | this |
|---:|---|---:|---:|---:|---:|
| 10,000 | spread | 60.3, 60.9 ms | 61.6, 60.0 ms | 78.0, 78.2 ms | 74.7, 76.8 ms |
| 100,000 | spread | 404, 392 ms | 457, 426 ms | 670, 718 ms | 605, 594 ms |
| 10,000 | near | 25.6, 25.6 ms | 18.0, 15.8 ms | | |
| 100,000 | near | 146, 137 ms | 18.6, 16.4 ms | | |

`ranked` is unchanged, 7.9 to 8.4 µs. Over near links a change costs its commits and the drain's 5 ms wait for them to
pause, twice, whatever the graph's size. Over spread links a drain reads every node, and from then walks them in place
order comparing each residual as it reaches it, as the drain over the whole graph did: one change costs 7 to 13% more
here and a burst about 14% less.

Where one change's cost goes, in milliseconds a drain at 100,000 documents, from timers in uncommitted copies run in turn
twice each, 2026-11-02, a load average of 5 to 6: main reads its nodes in 16, its arcs in 50, pushes in 9 and writes in 97.
This branch reads its nodes in 20, its arcs in 51, pushes in 23 and writes in 95, and its count of the totals takes 2 where
main's is part of its drain. The push's figure holds the timers' own cost, around each of the 100,000 arc reads. Its
reads' extra is the map by id owning its keys. Keeping the sweep's sums in local variables moved nothing, so what is left
is shared among those phases, and no one change takes it.

The two sweeps are two loops and two arc reads because that is what this cost took. Sharing one body, a change's arc read
over spread links took 100 ms where it takes 55 apart, at 100,000 documents, and one change 525 ms; a bitset of the waiting
kept up on every arc a push crosses made the pushes three times as dear. That set is a map by place in the sweep over a
few nodes, since a place is a number from the store's one sequence and a bitset by place is sized by that.

Pushing only the nodes over a level that halves down to the threshold, so residual piles up before it is pushed, does not
pay here. On a disposable copy on 2026-11-02, at a load average of 18 to 36, so the times are rough and the push counts
exact: pushes fell by 30 to 50% (100,000 documents: 940,000 to 1.3 million down to about 660,000; 10,000: a mean of 127,000
down to 64,000), and one change over spread links went from 497 to 576 ms at 100,000 and from 63 to 67 ms at 10,000, a
burst from 645 to 760 ms. Pushes are about 10 ms of a drain of some 215 ms, by phase timers on a copy at 100,000 documents,
and the writes about 100 ms, so fewer pushes save little and the repeat passes over every node cost more than that.

### A drain computed outside the write lock

A drain is computed on a read snapshot and written under the write lock, computed again there if a commit changed the
index meanwhile. What a small write waits for while one change after another drains, from `examples/rank-write-stall.rs`
at 100,000 documents and five spread links each, twenty changes linked and unlinked, a write every 2 ms; 2026-11-02, this
Mac, a load average of 5 to 7, the two sides in turn, two runs each:

| The write | Writes made, before | after | p50, before | after | p99, before | after | Slowest, before | after |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| A field | 1,473, 1,484 | 4,825, 3,312 | 418, 382 µs | 92, 103 µs | 214, 207 ms | 1.2, 105 ms | 445, 252 ms | 160, 225 ms |
| An edge the index reads | 131, 147 | 3,282, 3,130 | 237, 227 ms | 170, 202 µs | 469, 489 ms | 235, 238 ms | 523, 492 ms | 528, 328 ms |

The slowest write stays near what the drain's writes hold the lock for, and a recount when one is due. An edge write every
2 ms changes the index under most drains, so they are computed twice; the write still waits only for the second.

That table is commits asking a shared flag whether a drain was being computed, and leaving its pushes to it. A commit now
pushes an index only if it changed it and the index owed none before it, which depends on the history alone. Against the
flag on 2026-11-02, the two sides in turn, two runs each, a load average of 5 to 10:

| The write | Writes made, flag | now | p50, flag | now | p90, flag | now | p99, flag | now | Slowest, flag | now |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| A field | 3,386, 3,323 | 2,771, 3,203 | 90, 81 µs | 67, 61 µs | 468, 442 µs | 113, 103 µs | 103, 102 ms | 103, 103 ms | 211, 153 ms | 157, 153 ms |
| An edge the index reads | 2,950, 3,193 | 3,524, 3,802 | 168, 164 µs | 139, 143 µs | 558, 551 µs | 443, 426 µs | 239, 227 ms | 197, 195 ms | 302, 532 ms | 298, 323 ms |

With a commit pushing whenever it changed the index, drain owed or not, an edge write's p50 was 482 to 529 µs and its p90
617 to 714 µs: 16 pushes through the store, which the coming drain covers. One change over spread links at 100,000 is
unmoved, 421 and 415 ms against 418 and 415, and at 10,000, 60.1 and 59.6 ms against 60.6 and 62.9. A burst is 579 and 580 ms
against 533 and 520 at 100,000, and 70.8 and 72.8 against 64.7 and 65.2 at 10,000. Near links at 100,000 are 28.1 and 24.2 ms
against 24.7 and 24.2, higher than earlier the same day on both sides.

The mirror keeps what a drain reads until an edge changes after the drain's snapshot. From a counter in a disposable copy,
with a field write every 2 ms through twenty changes at 100,000: the first drain reads 100,000 lists from the store and
every drain after it reads none. Only the write lock resets the mirror, so a drain starting between a commit and its deltas
can't empty it.

### The score order's bits follow the tolerance

Keeping 16 significant bits whatever the tolerance, an edge changed rewrote some 43,500 order keys a drain over spread
links at 100,000 documents and 1e-6, under the write lock. What rewriting none would save, the most a coarser key could,
from a copy that never moved a key, in turn with #58's build, two runs each, 2026-11-02, this Mac, a load average falling
from 78 to 11, so the first runs' times are high:

| 100,000 documents | writing keys | writing none |
|---|---:|---:|
| one change, 1e-6 | 447, 434 ms | 384, 370 ms |
| ten and ten, 1e-6 | 523, 550 ms | 411, 393 ms |
| ten and ten, 1e-4 | 679, 552 ms | 386, 399 ms |
| a field's write beside them, p99 | 104, 105 ms | 68, 69 ms |
| its slowest | 162, 158 ms | 73, 72 ms |

The width of a cell, as multiples of the tolerance, over the same graph, each width run in turn twice at a load of 4 to 8.
`rankIndexChange`-shaped loops timed from a probe, medians of 15; 16 bits is the build before this one:

| Cell | one change, 1e-6 | ten and ten, 1e-6 | ten and ten, 1e-4 | a field's write, p99 | its slowest |
|---:|---:|---:|---:|---:|---:|
| 16 bits | 439, 413 ms | 551, 534 ms | 551, 506 ms | 105, 103 ms | 158, 155 ms |
| 64 | 388, 377 ms | 468, 438 ms | 418, 442 ms | 82, 81 ms | 140, 178 ms |
| **256** | 374, 371 ms | 433, 430 ms | 424, 419 ms | 78, 76 ms | 137, 131 ms |
| 1024 | 356, 373 ms | 420, 425 ms | 416, 418 ms | 75, 76 ms | 135, 133 ms |
| 4096 | 357, 361 ms | 396, 430 ms | 394, 406 ms | 74, 75 ms | 128, 133 ms |

256 keeps what a wider cell only moves within how far runs differ, where a wider one lets more nodes hold a cell wider than
the order promises, about half the multiple of them. It takes the keys a drain writes from some 43,500 to 12,250 at 1e-6.
They stop near there at any width, some 11,000 at 4096: this graph's scores sit around 1, where the exponent changes, so a
small move changes the key whatever the bits kept. The slowest write stays over the 72 ms of no key writes at any width.

Read costs, at every width and at 16 bits alike, hits then ids, top 10 and top 1,000: 8.3 to 9.2 µs and 680 to 840 µs,
3.0 µs and 69 µs. Over this graph of alike scores no cell is wider than promised, so these don't price reading through
one. Reading through every cell instead, so that a read with `limit` always agreed with one without, took top 10 from 9 µs
to 6 ms at 20,000 documents, as nearly every node is in one cell at 16 bits too.

### Cutting how long a drain holds the lock

Writing a drain's rows in key order, each prefix in one pass, doesn't pay: its write phase at 100,000 documents read 98.3
and 97.6 ms in place order and 96.2 and 96.0 ms in key order, by a timer on a copy, 2026-11-02. Over spread links nearly
every node moves, so every page of those rows is dirtied whichever order they go in. It wasn't kept.

A drain that finds itself stale waits for the commits that made it so to pause, then computes outside the lock again, up to
twice, before computing under it. From `examples/rank-write-stall.rs` at 100,000 documents, 2026-11-02, a load average of 4
to 8, the two sides in turn, three runs each. `--bursts` is twenty edge writes 2 ms apart and then 300 ms of nothing,
`--edges` an edge write every 2 ms:

| The writer | Writes made, before | after | p50, before | after | p90, before | after | p99, before | after | Slowest, before | after | All its waits, before | after |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Bursts | 900, 920, 920 | 1,080, 1,080, 1,080 | 154, 157, 165 µs | 158, 147, 154 µs | 530, 574, 584 µs | 545, 590, 576 µs | 219, 200, 204 ms | 42, 29, 22 ms | 296, 297, 307 ms | 101, 101, 104 ms | 5.2, 4.5, 4.6 s | 1.28, 1.29, 1.33 s |
| Steady | 4,131, 3,843, 3,785 | 6,458, 6,189, 6,377 | 113, 124, 123 µs | 139, 161, 151 µs | 432, 422, 407 µs | 497, 571, 545 µs | 4.1, 188, 206 ms | 1.2, 1.2, 1.0 ms | 291, 297, 401 ms | 462, 482, 271 ms | 9.9, 10.8, 10.9 s | 4.3, 5.9, 5.0 s |

A steady write's p50 and p90 rise about a quarter, which fits more of them finding the index within its budget and so
making a commit's pushes, though that wasn't measured. Its slowest stays near what a drain under the lock holds it for, once the retries are spent. The twenty changes took
the same time. A fixed 5 ms sleep in place of the pause made the steady run's twenty changes take 27 s, not 21.

### The default tolerance, against exact PageRank

The readings above that say "the default tolerance" are at 1e-6, which it was then. `examples/rank-accuracy.rs`, 2026-11-02,
this Mac: a rank index's top after 30 ordinary changes (`--accuracy`), against `rank` run to 1e-13, over a graph with hubs (the
first document some 4% of the score at 100,000), the same graph with its hubs anywhere among the documents by three seeds, and
the spread graph. The hub graph's first documents are its hubs, which flatters a top list, since one cell's nodes come by place
and those places then follow the scores; shuffled, they don't.

How many of each true top the index's top holds, and how many in the true place:

| Documents | Top | Graph | 1e-3 | 1e-4 | 1e-5 | 1e-6 |
|---:|---:|---|---:|---:|---:|---:|
| 1,000 | 10 | hubs | 9, 9 | 9, 9 | **10, 10** | 10, 10 |
| | | shuffled, each seed | 10, 10 | 10, 10 | **10, 10** | 10, 10 |
| | 100 | hubs | 97, 94 | 99, 99 | **99, 99** | 100, 100 |
| | | shuffled, lowest seed | 99, 97 | 99, 98 | **100, 100** | 100, 100 |
| 10,000 | 10 | hubs | 9, 9 | 9, 9 | **10, 10** | 10, 10 |
| | | shuffled, each seed | 10, 10 | 10, 10 | **10, 10** | 10, 10 |
| | 100 | hubs | 97, 94 | 100, 100 | **100, 100** | 100, 100 |
| | | shuffled, lowest seed | 98, 91 | 99, 98 | **100, 98** | 100, 98 |
| 100,000 | 10 | hubs | 9, 9 | 9, 9 | **10, 10** | 10, 10 |
| | | shuffled, lowest seed | 10, 8 | 10, 10 | **10, 10** | 10, 10 |
| | 100 | hubs | 99, 99 | 100, 100 | **100, 100** | 100, 100 |
| | | shuffled, lowest seed | 97, 94 | 98, 96 | **99, 97** | 100, 100 |
| | 1,000 | hubs | 989, 869 | 995, 977 | **999, 995** | 1,000, 1,000 |
| | | shuffled, lowest seed | 983, 884 | 995, 976 | **998, 994** | 999, 997 |

So the default is 1e-5, the loosest at which the top 10 was the true top 10 in the true order and the top 100 within 1% at every
size and seed, a bar set before these were read. It is closer than one graph made it look: 1e-4 misses its top 10 only over the
graph with hubs first, by one near tie, and misses the bar on the top 100 only at 98 of 100 in one seed. Four histories are few,
and the gaps near the 10th place are about 8 times the order's margin here, so another seed could move any of these. An
application for which those swaps are acceptable can declare a looser tolerance.

What it doesn't buy is a top list over a graph whose scores are alike. Over the spread graph at 100,000 documents the top 10 holds
1, 3, 3 and 5 of the true 10 at these four tolerances, the top 100 holds 29, 62, 65 and 70, and 1e-5 loses 1.36e-4 of the top 10's
score against 3.6e-5 at 1e-6; at 10,000 its top 10 holds 5 of 10 against 7. No tolerance tried gives PageRank's list there, as
nearly every score is alike, and a top list there says little at any of them.

What one change costs at 100,000 documents, its commit and its drain: medians of 40 from a document in the later half and of 20
from the highest-scoring one, and of 20 bursts of ten, the tolerances alternated, two runs each, a load average of 4 to 5:

| Graph | Tolerance | Ordinary | Highest-scoring | Burst |
|---|---:|---:|---:|---:|
| hubs | **1e-5** | 118, 118 ms | 292, 295 ms | 145, 149 ms |
| hubs | 1e-6 | 146, 144 ms | 342, 335 ms | 170, 175 ms |
| shuffled | **1e-5** | 128, 138 ms | 286, 309 ms | 164, 159 ms |
| shuffled | 1e-6 | 156, 154 ms | 343, 356 ms | 186, 191 ms |

So 1e-5 saves 10 to 19% on an ordinary change, and 12 to 17% on one from a hub or a burst. A change from a hub is the costliest
found, over twice an ordinary one on the same graph: it moves far more residual, as the estimate it redistributes is far larger.

Reads over the graph with hubs at 1e-5 are 8.4 µs top 10 and 751 µs top 1,000 at 100,000 documents as hits, and 3.9 and 71 µs as
ids, so reading through a wide cell costs little there.

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
