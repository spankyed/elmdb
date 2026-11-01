# Rank weights

Status: built, 2026-10-31 (`docs/evidence/graph-algorithms.md`, Weights): elmdb #44, and apack's side in apack #304.
Where the build differs from the design below:

- **The weight is stored only when it isn't 1**, on the row and in the edge entries, so an unweighted kind's rows and
  entries, and a weighted kind's edges of weight 1, are the bytes they were. Adding or removing `weighted` rewrites
  nothing.
- **A weight is a property of the edge, never part of what identifies it:** linking a pair again with another weight
  replaces the edge's (`architecture/schema.md`, Relations: an edge is its ends and its kind's key).
- **A weight, an edge's or a seed's, is at most 2^960**, so no sum of them a rank adds up reaches infinity and loses
  the walkers it holds; -0 is held as 0.
- **An id named twice among the seeds adds its weights.**
- **The hits answer's `rank` record gains no `weighted` flag**: EXPLAIN's `weight`, the total weight read, says whether
  weights were read.
- **The heap allowance for a weighted `rank` is 256 bytes an entity**, the reads' 192 leaving too little room over the
  184 it holds.

Designed 2026-10-31. Builds on `rank` (graph algorithms, stages 0–2, elmdb #35). Edges that carry a weight,
and seeds that carry one, so `rank` can say a strong link matters more than a weak one and one starting point more than
another. One PR: both change `rank`'s sums, its references and its benches.

## Why

`rank` treats every edge as equal and every seed as equal. Both are wrong for the graphs it is meant for:

- **apack's code index** (apack `docs/plans/code-index.md`) resolves calls with a measured confidence per edge, from 1
  for a compiler-resolved call down to a name guessed among several. Ranked unweighted, a guess pulls as hard as a
  certainty.
- **Repeated links:** a file calling a function forty times and one calling it once are one edge each.
- **Seeds:** a code map seeded from the open file and two files it imports, or a knowledge graph seeded from search
  matches of differing strength (HippoRAG's seeds are weighted by match), has starting points that don't matter equally.

## Edge weights

### Declared on the relation kind

```json
"calls": { "cardinality": "many-to-many", "weighted": true }
```

- **`weighted: true`** gives every edge of the kind a **weight**: a finite number, zero or more, `1` when a link gives
  none. A weight that isn't one is refused (`VALUE_INVALID`, naming the kind and the value).
- **A weight on an unweighted kind is refused**, so a weight is never stored and then silently ignored.
- **A weight is the edge's, not its info's.** `info` is opaque to the engine (`schema.md`, Relations), so a number inside
  it can't be checked at write or read without a parse per edge. A first-class weight is checked once, on the write.

Changing a schema treats it like any rule (`schema.md`, Changing a schema): `weighted` added to a kind with edges gives
them weight `1`; removed, the stored weights stay in the rows and nothing reads them.

### Written and read

- **Writes:** linking takes a weight beside the info (`link(kind, target, { info, weight })` through the binding), and
  an update to a relation may change it. A change set records a weight change as the relation `changed`.
- **Reads:** `getRelation`, `findRelations` and the edge reads (`edges`, `outEdges`, `inEdges`) answer `weight` for a
  weighted kind.

### Stored where a walk reads it

- **Primary:** the relation's row (`p/r`) holds the weight.
- **Derived:** a weighted kind's edge entries (`d/o`, `d/n`) hold `[relation, far end, weight]` rather than
  `[relation, far end]`, so building a projection reads the weight where it reads the far end
  (`relations::visit_far_ends`), with no lookup of the row. `rebuild()` writes them from the rows; `check()` compares
  them as every derived entry.
- **Format:** an in-place change to format 1 (`format.md`, Versions). The edge entries are derived, so `rebuild()` makes
  them; the rows gain a field only on kinds declared weighted.

## In `rank`

### Weighted edges

- **Transition:** from a node `u`, a walk follows edge `(u, v)` with probability `w(u, v) / W(u)`, where `W(u)` is the
  sum of `u`'s weights to nodes in the projection. Parallel edges between the same two nodes add.
- **Dangling:** a node whose `W(u)` is zero — no edges, or only zero weights — sends its mass to the teleport, as a
  node with no edges does today. A zero weight keeps an edge without letting it carry anything.
- **`direction: "both"`:** an edge contributes its weight in both directions.
- **Which weights:** `rank` uses the weights of every weighted kind it is given and `1` for each edge of an unweighted
  kind; `weights: false` ranks as if every edge weighed `1`. A kind declared weighted is declared so because its weights
  mean something, so using them is the default.
- **Unit weights change nothing:** with every weight `1`, the sums are the ones `rank` computes today, bit for bit
  (Verification).

### Weighted seeds

- **`seeds`** takes ids, as today, or ids with weights: `{ "seeds": [{ "id": "…", "weight": 3 }, …] }`.
- **Teleport:** each seed in the projection receives `weight / total` of it. A weight that isn't finite and above zero
  is refused (`BATCH_INVALID`); an id given twice adds its weights; seeds outside the projection are left out of the
  total, and none inside answers nothing, as today.
- **Plain ids are weight `1` each**, so today's calls answer what they answer now.

### Answer and EXPLAIN

The hits answer's `rank` record gains `weighted` (whether any weight was read), and EXPLAIN's graph line gains the
total weight read beside `nodes` and `edges`.

## Components and communities

- **`components`** ignores weights: whether two nodes connect doesn't depend on how strongly. A zero-weight edge still
  connects.
- **`communities`** uses weights where `leiden-rs` takes them (Leiden's modularity is defined over weighted graphs);
  stage 1 checks its graph type takes an edge weight, and if it doesn't, `communities` stays unweighted and says so in
  the docs.

## Projection

The CSR gains `weights: Vec<f64>`, built only when a weight is read: `8 × edges` bytes more, so a weighted projection is
about `8 × nodes + 12 × edges` plus the id table. The heap allowance for weighted `rank` is declared from that formula.

## In apack

- **Manifest:** `data.relations.<kind>.weighted: true` (`packages/default-setup/apack.json`), passed into the schema
  apack opens with.
- **Writes:** `write(id).link(kind, target, { weight })`; relation updates take `weight`. Typed: a weight on a kind the
  manifest doesn't declare weighted doesn't compile.
- **Reads:** `findRelations` and the edge reads answer `weight`.
- **Builder:** `.rank(kinds, { seeds, weights })` with `seeds` as ids or `{ id, weight }` pairs.
- **The code index** declares `calls`, `references` and `imports` weighted and writes each edge's measured confidence as
  its weight.
- **Reports:** `npm run api:update`, `facade:update`, and the manifest's schema (`schema:update`).

## Verification

- **Against a reference:** a dense weighted power iteration over random weighted graphs (the proptest strategy of
  `algorithms/tests.rs`, weights drawn including zeros and repeats) within 1e-9, and random weighted seeds.
- **Against networkx:** Zachary's karate club with its edge weights (networkx 3.4.2's `karate_club_graph` carries them):
  `pagerank(G, weight="weight")`, and with a weighted personalization, within 1e-9; an exact-bits golden of each, which
  the CI matrix (Linux, macOS, Windows) holds identical, as #35's are.
- **Unit weights are today's bits:** the karate club with every weight `1` reproduces `KARATE_BITS` exactly, and plain-id
  seeds reproduce `KARATE_FROM_0_BITS`.
- **Rules:** a zero-weight edge carries nothing; a node with only zero weights is dangling; parallel edges add; a
  negative, infinite or NaN weight is refused at write; a weight on an unweighted kind is refused; a seed weight of zero
  is refused.
- **Storage:** `check()` reports an edge entry whose weight differs from its row; `rebuild()` restores it.
- **Tracked:** a watch over a weighted `rank` runs again when an edge in its projection changes weight.
- **Cost:** benches of weighted against unweighted `rank` at 1,000 and 10,000 nodes (the `graph` group), and the binding
  route at 100,000 (`bench/rank.mjs`), with a budget declared now: weighted `rank` within 1.2× of unweighted. The heap
  case against `8 × nodes + 12 × edges`. The new cases in `scripts/diagnostics-overhead.py`'s `GATED`.
- **Mutations:** `mutants in the change`, nothing missed or each explained.

## Stages and PRs

1. **elmdb PR:** `weighted` on relation kinds, the stored weight and its edge entries, the link and update calls, the
   reads, `rank`'s weighted edges and seeds, `communities`' weights if `leiden-rs` takes them, EXPLAIN, protocol types
   (`cargo xtask generate`).
2. **apack PR:** the manifest key, typed writes and reads, the builder's options, reports and specs, pinned to the elmdb
   PR's head and then its merge commit.
