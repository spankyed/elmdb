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
