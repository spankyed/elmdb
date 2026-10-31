# Rank index

Status: designed, 2026-10-31. Builds on `rank` (graph algorithms, stages 0–2, elmdb #35) and its weights
([rank-weights.md](rank-weights.md), elmdb #44). A declared index that holds
PageRank scores and keeps them current as commits change the graph, so a query reads scores rather than computing
them.

## Where it starts

`rank` computes at query time: it builds the projection from the plan's set, runs power iteration from nothing, and
drops both when it answers ([graph-algorithms.md](../archive/roadmap/graph-algorithms.md), The projection is the plan's set). Every run
reads every entity and edge in the set. Through the release binding, from one document over five `refs` edges a
document, both ways, ten answered: 2.60 ms at 1,000 documents, 28.08 ms at 10,000 and 352.93 ms at 100,000 (2026-10-31,
Apple M1 Pro, macOS, Rust 1.99.1, Node 23.11.1; #35's `docs/evidence/graph-algorithms.md`, Cost). A live view over a
`rank` pays that after every commit into its set, whatever the commit changed.

## What it covers, and what it doesn't

- **Covered: PageRank with every node a seed.** There is one answer per graph, so it can be stored and kept current.
- **Not covered: personalized PageRank.** Every seed set has its own answer, so there is nothing to store ahead of a
  query. It stays the `rank` step, computed at query time, unchanged.

The index doesn't replace `rank`. A one-off read, a read over a set the index doesn't declare, and personalized PageRank
all use the step.

## Declaring

A rank index spans types (a code map's graph joins files and symbols), so it is declared at the top of the schema
document beside `types` and `relations`, not on a type:

```json
"ranks": {
  "noteRank": { "types": ["Note"], "relations": ["references"], "direction": "both",
                "weights": true, "damping": 0.85, "tolerance": 1e-6 }
}
```

- **Graph:** the live entities of `types`, and the edges of `relations` whose two ends are both among them, followed
  `out`, `in` or `both` (`Direction`). A trashed entity leaves the graph and a restored one joins it.
- **`weights`**, default `true`, as `rank`'s: an edge of a kind declared weighted counts by its weight, an edge of a
  kind not weighted by 1, and `false` counts every edge 1.
- **`damping`** in (0, 1), default `0.85`. **`tolerance`** > 0, default `1e-6`: the bound on the scores' L1 error that
  the index keeps (Error, below).
- **Names:** lowerCamelCase, unique across the schema's indexes. `types` and `relations` must be declared, and
  `relations` is required, as `rank`'s `kinds` is.

### The contract: within a bound of true PageRank

**One definition of PageRank.** The index's graph is the one `rank` builds from the same declaration (`project()`:
the same entities, kinds, direction and weights), and true PageRank is what `rank` answers over it, every node a seed
of weight 1. The contract below is stated against that, so a rule `rank` follows — a node whose edges weigh nothing in
all is dangling, parallel edges add — is the index's too, and never a second implementation of it.

The index is approximate, and the error bound is its contract. Any incremental method reaches an approximation by a
path, and the path is the history of commits, so two databases holding the same data can hold different bits. True
PageRank is unique and depends on the graph alone, so the index promises what both share:

- **Every stored score is within the reported error bound of true PageRank** (Error is reported, not hidden, below).
  The bound is below `tolerance` once no drain is pending. The bound is on the L1 error of the whole score vector, so
  it bounds each score's error too.
- **Order:** two entities whose scores differ by more than twice the bound are in the same order in every database
  holding the same data. Closer than that, their order is not promised, as an approximate nearest-neighbour search
  doesn't promise the order of near neighbours (an HNSW graph, which the vector index keeps, also depends on the order
  vectors were added).
- **Within one database, answers are deterministic:** ties go to set order, and the same history of commits gives the
  same bits on every platform (When the work runs, Determinism).

`check()`, `rebuild()` and restores are specified against this contract (Rebuild and check), not against stored bits.

Changing a schema treats it as every index is treated (`schema.md`, Changing a schema): added, it builds in the
background and a `ranked` read is refused until it has built, with a new error code naming the index (a rank index
has no rows to walk in its place, as a value index does); changed, it rebuilds under its name;
removed, it is dropped. `weighted` added to or removed from a relation kind the index reads changes the index's graph,
so the index rebuilds, though the change rewrites no relation.

## How it stays current

### The algorithm: forward push with residuals

The index keeps, per node, an **estimate** `p(v)` and a **residual** `r(v)`, under the invariant of forward push
(Andersen, Chung and Lang, 2006): the true scores are the estimate plus what the residuals would push. Pushing a node
moves `(1 − damping)` of its residual into its estimate and spreads the rest over its out-neighbours' residuals,
each by its edge's share of the node's: `w(u, v) / W(u)` with weights, `1 / out-degree` without. The
total residual mass bounds the estimate's error, so pushing until it falls below `tolerance` gives scores within that
L1 bound.

A graph change updates residuals locally while keeping the invariant (Zhang, Lofgren and Goel, 2016, *Approximate
Personalized PageRank on Dynamic Graphs*):

- **An edge added or removed** at `u` changes `u`'s out-weight `W(u)` (its out-degree without weights), which adjusts
  the residuals of `u` and its far end from `p(u)`: constant work per edge changed.
- **An edge's weight changed** is the same correction, from the old weight and the new, which the commit reads in its
  own transaction: constant work, with no re-read of `u`'s other edges.
- **A node joining or leaving** (created, destroyed, trashed, restored, its type entering the graph) adds or removes
  its teleport share at that node alone.

Pushing then spreads the change only as far as it is large enough to matter.

### Two terms the formulation needs, so no change costs O(nodes)

- **Unnormalized scores.** Each node's teleport is `1 − damping`, not `(1 − damping) / n`, so a node joining doesn't
  change every other node's share. A read divides by the total mass, held in the index's totals.
- **Dangling mass as one shared term.** Today `rank` sends a node with no out-edges, or whose edges weigh nothing in
  all, to the teleport, which spreads it
  over every node. The index holds that mass as one shared term added uniformly at read, not written per node.

### When the work runs

- **In the commit:** a unit's edge and node changes update residuals in the same transaction, so the index is
  consistent with the data at every commit, as a text index is. The unit then pushes up to a **budget** of node pushes
  (a constant, to be set from the benchmarks), so a link to a hub never makes one write unbounded.
- **After it:** residual left over the threshold is drained by the database's own thread, in chunked units as an index
  build is (`engine.md`, Admin, Index builds), so writes go on between chunks. Closing stops a drain between chunks, and
  the next open finishes it.
- **Determinism:** pushes run in a fixed order (a queue ordered by the node's place), and sums run serially, so the same
  history of commits gives the same bits on every platform.

### Error is reported, not hidden

The index keeps its total residual mass in its totals. A read answers that bound beside the scores (`hits` answers
`rank: { index, sequence, error }`), so a reader knows how far the scores may be from a full recompute while a drain is
behind. With no drain pending it is below `tolerance`.

## Storage

Every prefix is declared in `format`'s table, all derived:

| Prefix | Holds |
|---|---|
| `d/ra` index entity | a node's estimate and residual, f64 each |
| `d/rk` index score entity | the nodes in score order, highest first, so a top-k reads a range |
| `d/rq` index place | a node whose residual is over the threshold, waiting for a push |
| `d/rs` index | the totals: node count, total mass, residual mass, the dangling term, the last sequence pushed to |

A push that changes a score rewrites its `d/rk` key. That doubles the writes of a push; the benchmarks say whether
`d/rk` should be rewritten only when a score moves past a granularity.

## Reading it

- **`ranked(index)` step:** orders the plan's set by the index's scores, highest first, with ties by set order, and
  records each score for `hits`, as `rank` does. The order it promises across databases is the contract's (Declaring,
  The contract). A set outside the index's graph is refused with `BATCH_INVALID`, naming
  the index's types.
- **Top-k without reading the set:** a plan that starts from the index's types and ends `ranked(index).limit(k)` reads
  `d/rk` from the top, so it costs `k`, not the graph's size. The planner picks it, as it picks a value index.
- **EXPLAIN** names the index and the stage's error bound.
- **Watch:** a watch over a top-k is tracked over the `d/rk` range it read, so it runs again when a score in that range
  moves. Pushes move many scores a little, so such a watch may run after most commits into the graph; the benchmarks
  measure how often, and the step may round scores read under watch to the index's tolerance so a move below it
  changes no answer.

## Rebuild and check

Each is held to the contract (Declaring, The contract), not to stored bits:

- **`check()`** verifies the contract directly. It computes true PageRank by power iteration (`rank`'s code over the
  same graph, weights included, run to a tolerance far below the index's) and reports each stored score further from it than the stored error bound allows.
  It also checks the invariant, that each estimate plus what the residuals would push reproduces the scores within the
  same bound, so a corrupted residual is caught even while its estimate still looks right. Every other index's check
  compares bits; this one's compares against the answer, which is the stronger check of the two.
- **`rebuild()`** clears the index and pushes from the teleport vector to `tolerance`, the same code as maintenance
  rather than a second implementation. Its result meets the contract; being the same bits as before the rebuild is not
  part of it.
- **Dumps and restores** carry primary data only (`format.md`, Dumps), so a restored database builds its rank indexes
  and meets the contract, as the original does.

`format.md` states, when this is built, that a rank index's stored bits depend on history and its check is against the
contract.

## In apack

- **Manifest:** `apack.json` `data.ranks`, beside `data.relations`, with the same keys. Codegen writes it into the
  schema it opens with, as it does `search` and `embed`.
- **Builder:** `.ranked(index)` on the query builder (`packages/apack-ears/src/query.ts`), with `hits()` accepting it as
  it accepts `rank`. The index name is typed from the manifest as `weightedKinds` is (rank weights' apack PR): a
  string union member of `EarsSchema`, `rankIndexes`, filled by codegen and `never` when the manifest declares none.
- **Reports:** `npm run api:update` for the builder, `facade:update` for default-setup's facade, and the manifest's JSON
  Schema (`schema:update`).

## Verification

- **Against a full recompute:** a model test (proptest-state-machine, as `crates/elmdb/src/engine/tests/model.rs` drives the store) applies
  random sequences of links, unlinks, weight changes (`updateRelation` and a unique kind's relink), creates, trashes and
  destroys over a weighted kind and an unweighted one, and after each checks the scores against power
  iteration within the reported bound, and the invariant.
- **The contract across histories:** the same graph built by random permutations of its commits gives scores within
  the bound of each other, and the same order wherever two scores differ by more than twice it.
- **`check()` catches damage:** a stored estimate, residual or total altered past the bound is reported; one altered
  within it is not.
- **Determinism:** a golden of the exact bits after a scripted history on the karate club graph, which the CI matrix
  (Linux, macOS, Windows) holds identical, as #35's rank golden is.
- **Bounded commits:** a test that one link to a hub of 10,000 does at most the budget's pushes in its commit, and the
  drain finishes the rest.
- **Crash:** the crash tests kill a drain between chunks and check the index after reopening.
- **Cost:** benches of one link added and one node created at 1,000, 10,000 and 100,000 nodes, against a full
  recompute at the same sizes; a top-k read against `rank` plus `limit`; a dhat case for the index's bytes per node;
  the new benches in `scripts/diagnostics-overhead.py`'s `GATED`.
- **Mutations:** `mutants in the change`, nothing missed or each explained.

## Stages and PRs

1. **elmdb PR:** the schema key, storage, maintenance in the commit with its budget, the drain, `rebuild()` and
   `check()`, and the `ranked` step with top-k reads and EXPLAIN. One PR, because the index isn't readable without the
   step or testable without `check()`.
2. **apack PR:** the manifest key, codegen, `.ranked()` on the builder, reports and specs, pinned to the elmdb PR's head
   and then its merge commit.

## Risks

- **Watch churn:** a live top-k may run after most commits into the graph (Reading it, Watch). Rounding to the
  tolerance is the fix if the benchmarks show it.
- **Write cost on hubs:** the budget bounds a commit, and the drain carries the rest; the error bound tells a reader
  when the drain is behind.
