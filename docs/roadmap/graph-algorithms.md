# Graph algorithms

Status: planned, 2026-10-30. Nothing is built. This plan puts graph algorithms in the engine, as plan steps and
answers, rather than leaving each caller to read every edge and compute over them in JavaScript.

## Why in the engine

apack's data is a graph: notes link notes (`references`), folders hold folders and documents (`parent_of`, `contains`),
threads fork and nest, flows are nodes joined by transitions. An AI development environment will ask that graph the
questions graph algorithms answer: which notes are central, which ones form a topic, what matters most from where the
user is now. The last is the strongest case: ranking context for a prompt by personalized PageRank from the entities
the user is looking at, fused with text and vector search, is how graph-based retrieval (HippoRAG, GraphRAG) ranks
context.

Computing that in JavaScript over one read of the edges works on a small graph and is the wrong default:

- **Every caller writes its own.** A graph view, a related-notes panel and a prompt builder each implement PageRank,
  each untested, each with its own convergence rule, dangling-node rule and tie order. Their answers disagree.
- **The edges cross the binding.** Every relation row is encoded in msgpack, copied through N-API and decoded, to be
  reduced to a few dozen scores. The engine already holds them in a memory map.
- **Several reads are several snapshots.** A computation that reads the nodes, then the edges, then the rows to show
  can see three different commits. A plan reads one.
- **A live view re-runs it correctly only by accident.** A watch over a plan is written into exactly by the commits that
  touch what it read (tracked reads). Edges read outside a plan and reduced in JavaScript give the watch no projection to
  track beyond the raw edge read.
- **Nothing bounds it.** A JavaScript power iteration has no iteration cap the engine knows about, no EXPLAIN stage and
  no budget a caller can see.

In the engine, an algorithm is written once in Rust, tested against a reference over random graphs, and runs over one
snapshot, tracked, explained and bounded. It composes with the plan as it stands: the set a plan has built, from a
type, an index lookup, a search or a walk, is the subgraph the algorithm runs on.

## Design

### The projection is the plan's set

Graph analytics systems run an algorithm over a projection: which nodes, which relation kinds, which direction (Neo4j
GDS calls it a graph projection; Memgraph and Kùzu have the same idea). In elmdb the projection is what the plan has
already built:

- **nodes:** the entities of the set before the step;
- **edges:** the relations of the kinds named, between two entities of that set, followed `out`, `in` or `both` ways
  (`Direction`, as `pathTo` and `degrees` take it), so an undirected algorithm reads `both`;
- **scope:** the plan's, so a trashed entity is in the projection only under `withTrashed`.

There are no named, stored projections. A plan builds its projection from the snapshot it reads and drops it when it
answers, so it can never be stale. A cache keyed by commit sequence waits for a measurement showing projection
building costs.

Building it: one pass over the set gives each entity a dense `u32` index in set order, and one pass over each entity's
edges (`relations::visit_edges`, the edge reads the walks use) builds compressed sparse rows (CSR), keeping only
edges whose far end is in the set. Its memory is about `8 × nodes + 4 × edges` bytes plus the id table.

### Steps and answers

- **`rank` step:** orders the set by score, highest first, keeping the top `k` when given. The algorithm is one of:
  - `pagerank`: damping `0.85` by default, uniform teleport;
  - `personalized`: PageRank whose teleport goes to the **seeds**, a second set named by ids or by a nested plan's
    answer. Its score answers "how much does this matter from there".

  The scores go where a search's do, so `hits` answers them beside the ids, and a `rank` after a `hybrid` or `search`
  re-ranks its results. `hits()` is refused today without a search step; `rank` counts as one.
- **`components` terminal:** the weakly connected components of the projection, as `groupBy` answers groups: each
  group's ids, or with `counts` its size, largest first, ties by first member's place in the set.
- **`communities` terminal:** Leiden communities (Traag, Waltman and van Eck, 2019) in the same shape, with
  `resolution` (default `1.0`) and a `seed`. Leiden rather than Louvain, because Louvain can return communities that
  are not connected, and Leiden guarantees they are.

All three take `kinds` and `direction`. `rank` and `communities` also take `iterations` (default `100`) and
`tolerance` (default `1e-6`, L1 change in scores between rounds).

### Rules every algorithm keeps

- **Deterministic.** The same snapshot and arguments give the same answer, bit for bit, on every platform. Nodes are
  numbered in set order, sums run in a fixed order, nothing runs in parallel in the first stages, and Leiden's
  randomness comes from its `seed` through a seeded generator in the engine. Ties go to the earlier entity in the set.
- **Bounded, and partial says so.** Iteration stops at convergence or at `iterations`, whichever comes first. The
  answer says which: `{ converged: bool, iterations: n }` beside the scores. No time limit: a clock is not a check.
- **Standard edge cases, written down.** A node with no out-edges (a dangling node) gives its score to the teleport
  targets, all nodes for `pagerank` and the seeds for `personalized`. An empty set answers nothing. A seed outside the
  projection is ignored; no seed inside it answers nothing, rather than falling back to plain PageRank.
- **Unweighted.** Every edge counts once. Weights wait for relation fields declared in the schema
  (`deferred/graph-traversal.md`, item 2); `info` is undeclared and unindexed, and an algorithm must not guess a
  number out of it.
- **Tracked and explained.** The projection is built through the read that tracked reads record, so a watch over a
  `rank` is written into by a commit that adds, removes or moves an edge between two of its entities, or changes the
  set, and by nothing else. EXPLAIN names the stage and reports nodes, edges, iterations and whether it converged.

### Library or our own code

A mature library covering the surface is preferred over hand-written code, and the choice is recorded with its
evaluation in `development-libraries.md`, as that roadmap's stages record theirs. Stage 0 evaluates, against these
requirements:

- personalized PageRank with explicit seeds, dangling-node handling as above, and convergence reported;
- Leiden with a seed and resolution, deterministic for a given seed;
- building from our CSR without copying the graph into the library's own structure, or a measured cost for copying;
- deterministic output and no parallelism we can't turn off;
- licence compatible with MIT OR Apache-2.0, maintenance, and the dependencies it brings.

Candidates include `petgraph`, which has connected components and a PageRank without personalization, and the Rust
crates that implement Leiden. Weakly connected components is a union-find of about thirty lines, and power-iteration
PageRank over CSR is about sixty; if no library meets the requirements, those two are written here and Leiden is the
one to look hardest for.

## Stages

0. **Evaluate the libraries** against the requirements above, and record the decision in `development-libraries.md`.
1. **Projection and `components`.** CSR from the set, the `components` terminal, EXPLAIN, tracked reads, the protocol
   and its TypeScript declarations. Components first, because it is exact and its reference is trivial, so it proves
   the projection.
2. **`rank`: PageRank and personalized PageRank**, with scores through `hits`, seeds by ids or a nested plan, and
   `converged`/`iterations` in the answer.
3. **`communities`: Leiden.**
4. **apack.** `@apackdev/ears` exposes `.rank(...)`, `.components(...)` and `.communities(...)` on the typed query
   builder and `untypedQuery`, with its API report updated. The first callers are the ones that justified this:
   related notes ranked by personalized PageRank from the open note over `references` both ways, prompt context
   ranked by personalized PageRank from the entities the user mentions and fused with hybrid search, and a graph view
   coloured by community and sized by rank.

Later, each when a caller needs it: node similarity (Jaccard over neighbours, "notes linked like this one"), k-core,
betweenness centrality (Brandes), label propagation, and weights once relations declare fields.

## Verification

Each stage, beside the usual `cargo fmt`, clippy with warnings denied, `cargo nextest run --workspace`,
`cargo xtask check-generated` and the binding's suite:

- **A plain reference in the tests:** components by breadth-first search, and PageRank by dense power iteration over an
  adjacency matrix, both written as plainly as the model harness's reference model. Property tests over random graphs
  (`proptest`): components equal exactly; scores equal the reference within `1e-9`.
- **Invariants that hold for any correct implementation:** scores sum to 1; relabelling entities permutes scores and
  nothing else; a directed cycle and a complete graph score every node equally; personalized PageRank with every node
  a seed equals PageRank; a community is always connected (Leiden's guarantee).
- **One known answer:** Zachary's karate club (34 members, 78 edges), the standard fixture: one component, and PageRank
  scores matching networkx's for the same damping and tolerance.
- **Determinism:** the same plan twice, and the same plan on the CI matrix (Linux, macOS, Windows), give identical bytes.
- **Tracked reads:** a watch over a `rank` is named by a commit linking two entities in its set, and not by one linking
  entities outside it.
- **Mutation testing:** the `mutants in the change` CI jobs, every mutant in the new code caught or explained.
- **Cost:** the Criterion benches and instruction counts (`docs/architecture/testing.md`) gain a projection and each
  algorithm at 1,000 and 10,000 entities, and a heap bound like `maintenance_heap` holds the projection to its formula.

## Not in this plan

A graph query language, stored or named projections, parallel or distributed execution, weighted algorithms before
relation fields exist, and incremental maintenance of a rank across commits (`incremental-views.md` is where that would
go, when a measured re-run says it should).
