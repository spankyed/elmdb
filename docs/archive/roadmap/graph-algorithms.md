# Graph algorithms

Status: built, 2026-10-31 ([the readings](../../evidence/graph-algorithms.md)): stages 0 to 2, 4 and 5 in elmdb, stage 3
in apack; stage 5 has no consumer yet. This plan puts graph algorithms in the engine, as plan steps and answers, rather than leaving each
caller to read every edge and compute over them in JavaScript. `../../architecture/engine.md`, Reads, has them as built.

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
- **The edges cross the binding, every time it runs.** Every relation row is encoded in msgpack, copied through N-API
  and decoded, to be reduced to a few dozen scores. The engine already holds them in a memory map. The power iteration
  is cheap; reading its input across the boundary is the cost, and it is paid per run: prompt context ranks on every
  prompt, and a live view ranks again after every commit into its set. A graph of threads, messages and flow runs
  grows without a ceiling, so that cost grows with the user's history. How large it is has not been measured; stage 2
  measures it (Verification, Cost).
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

`kinds` is required and has no default. Which relations an algorithm follows decides its answer more than the
algorithm does: a containment kind (`parent_of`, `contains`) joins every member of a folder through the folder, so a
rank over it calls everything in a large folder related to everything else in it. Each caller names the kinds whose
edges mean what it is asking; related notes follows `references` alone.

There are no named, stored projections. A plan builds its projection from the snapshot it reads and drops it when it
answers, so it can never be stale. A cache keyed by commit sequence waits for a measurement showing projection
building costs.

Building it: one pass over the set gives each entity a dense `u32` index in set order, and one pass over each entity's
edges (`relations::visit_far_ends`, which lends each far end where it lies rather than copying it) builds compressed
sparse rows (CSR), keeping only edges whose far end is in the set. The graph is `4 × nodes + 4 × edges` bytes beside
the set's ids and a map from id to node: a graph read holds about 184 bytes an entity at five edges each, which the
heap test bounds.

### Steps and answers

- **`rank` step:** orders the set by score, highest first, with damping `0.85` by default; a `limit` after it keeps
  the highest, and a hit is recorded only for an entity handed on, so a limit keeps that many and no more. It is one
  algorithm, personalized PageRank, whose teleport goes to the **seeds**:
  - `personalized`: the seeds, named by ids. A nested plan's answer was considered and is not built: no plan nests
    another, and both callers already hold their seeds' ids. Its score answers "how much
    does this matter from there", which is what apack's callers ask, so this is the case built and tested first;
  - `pagerank`: every node of the projection is a seed, so the teleport is uniform. It answers "what is central
    overall", and is the same code with a different seed set rather than a second implementation.

  The scores go where a search's do, so `hits` answers them beside the ids, and a `rank` after a `hybrid` or `search`
  re-ranks its results. `hits()` is refused today without a search step; `rank` counts as one.
- **`weakComponents` terminal:** the weak components of the projection, the groups its edges join whichever way they
  point, as `groupBy` answers groups: each
  group's ids, or with `counts` its size, largest first, ties by first member's place in the set.
- **`strongComponents` terminal** (stage 5): the strong components, the groups whose entities each reach every other
  following the edges' direction, in the same shape. An entity on no cycle is a group of its own, so a group of more
  than one is a cycle: an import cycle in a code map, threads that block each other.
- **`communities` terminal** (a separate later stage, see Stages): Leiden communities (Traag, Waltman and van Eck, 2019) in the same shape, without `counts`, with
  `resolution` (default `1.0`) and a `seed`. Leiden rather than Louvain, because Louvain can return communities that
  are not connected, and Leiden guarantees they are.

All four take `kinds`. `rank` takes `direction` too; `weakComponents` and `communities` don't, an edge joining its ends
whichever way it points, so a direction would change nothing, and nor does `strongComponents`, whose groups are the
same with every edge reversed. `rank` and `communities` also take `iterations`
(default `100`), and `rank` a `tolerance` (default `1e-6`, L1 change in scores between rounds): at damping 0.85 the
change after `n` rounds is at most 2 × 0.85ⁿ, below 1e-6 by the 90th, so the defaults always settle. Leiden stops
early when a round no longer improves the partition.

### Rules every algorithm keeps

- **Deterministic.** The same snapshot and arguments give the same answer, bit for bit, on every platform. Nodes are
  numbered in set order, sums run in a fixed order, nothing runs in parallel in the first stages, and Leiden's
  randomness comes from its `seed` through a seeded generator in the engine. Ties go to the earlier entity in the set.
- **Bounded, and partial says so.** Iteration stops at convergence or at `iterations`, whichever comes first. The
  `hits` answer says which, as `rank: { converged, iterations }` beside the hits, and EXPLAIN's stage does too: settled
  in 0 rounds for a rank with no seed in its set, and absent when a search after the rank scored the hits instead. No
  time limit: a clock is not a check.
- **Standard edge cases, written down.** A node with no out-edges (a dangling node) gives its score to the teleport
  targets, all nodes for `pagerank` and the seeds for `personalized`. An empty set answers nothing. A seed outside the
  projection is ignored; no seed inside it answers nothing, rather than falling back to plain PageRank.
- **Unweighted.** Every edge counts once. Weights wait for relation fields declared in the schema
  (`deferred/graph-traversal.md`, item 2); `info` is undeclared and unindexed, and an algorithm must not guess a
  number out of it.
- **Tracked and explained.** The projection is built through the read that tracked reads record, so a watch over a
  `rank` is written into by a commit that adds, removes or moves an edge between two of its entities, or changes the
  set. It reads each entity's edges of the kinds it follows, so an edge of those kinds from one of its entities to one
  outside the set writes into it too, changing nothing it answers; an edge of another kind, or between two entities
  outside it, doesn't. EXPLAIN names the stage and reports nodes, edges, iterations and whether it converged.

### Library or our own code

A mature library covering the surface is preferred over hand-written code, and the choice is recorded with its
evaluation in `development-libraries.md`, as that roadmap's stages record theirs. Stage 0 evaluates, against these
requirements:

- personalized PageRank with explicit seeds, dangling-node handling as above, and convergence reported;
- building from our CSR without copying the graph into the library's own structure, or a measured cost for copying;
- deterministic output and no parallelism we can't turn off;
- licence compatible with MIT OR Apache-2.0, maintenance, and the dependencies it brings.

Candidates include `petgraph`, which has connected components and a PageRank without personalization, and the Rust
crates that implement Leiden. Weakly connected components is a union-find of about thirty lines, and power-iteration
PageRank over CSR is about sixty; if no library meets the requirements, those two are written here. Leiden is not
part of this evaluation: it has its own when its stage starts, and is the one to look hardest for a library for, being
much the largest of the three to write and to verify.

## Stages

0. **Evaluate the libraries** against the requirements above, and record the decision in `development-libraries.md`.
1. **Projection and `weakComponents`.** CSR from the set, the `weakComponents` terminal, EXPLAIN, tracked reads, the protocol
   and its TypeScript declarations. Components first, because it is exact and its reference is trivial, so it proves
   the projection.
2. **`rank`: personalized PageRank**, with scores through `hits`, seeds by ids, and
   `converged`/`iterations` in the answer; `pagerank` as every node seeded.
3. **apack.** `@apackdev/ears` exposes `.rank(...)` and `.weakComponents(...)` on the typed query builder and
   `untypedQuery`, with its API report updated, and no app caller yet. Related notes and prompt context were built and
   withdrawn: a notes graph this small and sparse ranks about as a two-step walk does, and the prompt context sent
   notes the user had not chosen. A caller comes with a need, such as ranking across every entity type.
4. **`communities`: Leiden**, off the critical path: its one caller is a graph view coloured by community, which
   stages 1 to 3 do not wait on. Built through leiden-rs after its own evaluation (`development-libraries.md`, stage
   6), with `@apackdev/ears`'s `.communities(...)`; the graph view isn't built. Like `weakComponents`, it takes no
   `direction`: modularity is of the undirected graph, an edge joining its ends whichever way it points.
5. **`strongComponents`: Tarjan's algorithm**, written in elmdb (about sixty lines, walking with its own stack so a
   long chain can't overflow the thread's). No consumer: `@apackdev/ears` doesn't expose it yet. The code map's
   import-cycle check would be the first; a check of one new link, such as the thread link form's, needs only
   `wouldCreateCycle`.

6. **`topological` and `similar` steps**, written in elmdb. `topological` is Kahn's algorithm over the strong
   components, each time taking the ready one whose first member is earliest in the set, so a cycle stays together
   and the answer is the order the edges allow nearest the set's; EXPLAIN counts its `cycles`. `similar` scores the set
   by Jaccard or Adamic–Adar over neighbours, read two edges out from `to` rather than across the set, its neighbours
   counted wherever they are, so files can be alike by the symbols they use; after a type start it reads membership
   per candidate, so its cost is the neighbourhood's, and Adamic–Adar's logarithm is libm's, the same on every platform. `@apackdev/ears` exposes both on the query
   builder, with no app caller.

Later: k-core, betweenness centrality (Brandes), label propagation, and weights (`rank-weights.md`).

## Verification

Each stage, beside the usual `cargo fmt`, clippy with warnings denied, `cargo nextest run --workspace`,
`cargo xtask check-generated` and the binding's suite:

- **A plain reference in the tests:** weak components by breadth-first search, and PageRank by dense power iteration
  over an adjacency matrix, both written as plainly as the model harness's reference model. Property tests over random graphs
  (`proptest`): weak components equal exactly; scores equal the reference within `1e-9`.
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
  Stage 2 also records the route this plan replaces, beside the engine's, in `docs/evidence/`: every edge of the kinds
  read through the binding and personalized PageRank computed in JavaScript, against `rank` through the binding, at
  1,000, 10,000 and 100,000 entities, each with its date and conditions. The reading sizes the speed reason above,
  which until then is an expectation and not a figure.

## Not in this plan

A graph query language, stored or named projections, parallel or distributed execution, weighted algorithms before
relation fields exist, and incremental maintenance of a rank across commits (`incremental-views.md` is where that would
go, when a measured re-run says it should).

Measured and not taken: handing leiden-rs each edge from its lower end, so its builder merges A → B with B → A. On a
graph of 10,000 nodes whose 100,000 edges each have their reverse, `communities` took 14.2 ms either way and found the
same partition (2026-10-31, Apple M1 Pro, release build), and on the bench graph, whose edges rarely have one, its heap
stayed at 764 bytes an entity. Merging can't lower that peak anyway: leiden-rs keeps every edge added (24 bytes each),
and its `build()` sorts them and copies them into a second list of the same length before building its CSR, so the
peak holds every edge twice whatever order they come in. Cutting it for real means handing leiden-rs fewer edges or
none: a graph built from our CSR directly, which leiden-rs 0.8.1 has no constructor for (an upstream change), or
Leiden written over our own CSR. Dropping duplicate edges ourselves first would cost a hash set per edge and still pass
the rest through both lists.
