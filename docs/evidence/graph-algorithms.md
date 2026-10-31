# Graph algorithms: what they answer and what they cost

The evidence for [the graph algorithms](../roadmap/graph-algorithms.md), stages 0 to 2 and 4. 2026-10-31, on
`session/graph-algorithms` from `f613add`, on an Apple M1 Pro (macOS), Rust 1.99.1, Node 23.11.1.

## Correct

- **Against references** (`crates/elmdb/src/engine/algorithms/tests.rs`): over 256 random graphs of up to 12 nodes
  and 30 edges, personalized PageRank equals a dense power iteration over an adjacency matrix within 1e-9, scores sum
  to 1, relabelling the nodes permutes the scores, and weak components equal a breadth-first search's.
- **Invariants:** a cycle and a complete graph score every node alike; a node with no edges sends its walker back to
  the seeds; seeds outside the graph are ignored, and none inside answers nothing; rounds stop at the tolerance or
  the limit and say which.
- **Known answer:** Zachary's karate club (34 members, 78 friendships, both ways), against networkx 3.4.2's pure-Python
  `pagerank` (alpha 0.85, unweighted, tolerance 1e-15): PageRank and PageRank personalized to member 0 each agree
  within 1e-9, and the club is one weak component.
- **Deterministic:** the club's scores at the defaults are recorded bit for bit and asserted on every platform CI runs
  (Linux, macOS, Windows): 30 rounds for PageRank and 39 from member 0.
- **Through the store** (`crates/elmdb/src/engine/tests/ranks.rs`): over 64 random graphs of 7 tasks and two kinds,
  the scores and weak components the engine answers equal those of the graph built by hand from the same edges, out, in
  and both ways; a rank follows only edges inside the set and the scope; a tracked rank is written into by a link
  between two of its entities and not by one outside it; EXPLAIN reports each graph's nodes, edges and rounds.

## Cost

**Against the route it replaces** (`node crates/elmdb-node/bench/rank.mjs`, the release binding): personalized
PageRank from one document over five `refs` edges a document, both ways, its ten highest answered. One route reads
every id and every edge through the binding and ranks in JavaScript; the other is `rank` and `limit`. Both answer the
same ten, checked first. Medians of 20 runs, 5 at 100,000:

| Documents | Through the binding, ranked in JavaScript | `rank` in the engine | |
|---:|---:|---:|---:|
| 1,000 | 12.88 ms | 2.60 ms | 5.0× |
| 10,000 | 145.33 ms | 28.08 ms | 5.2× |
| 100,000 | 1,670.68 ms | 352.93 ms | 4.7× |

The first build of the graph copied each edge's relation id and far end out of the store, and `rank` was 2.8 to 3.0×
faster than the JavaScript route; reading the far end where it lies (`relations::visit_far_ends`) made it 5×.

**Where the time goes** (`ELMDB_BENCH_ROWS=1000,10000 cargo bench -p elmdb --bench maintenance -- graph`, Criterion
medians, the same graph):

| Read | 1,000 | 10,000 |
|---|---:|---:|
| `weakComponents` | 1.30 ms | 14.14 ms |
| `rank`, every document a seed, edges out | 1.30 ms | 14.05 ms |
| `rank` from one document, edges both ways | 2.57 ms | 28.79 ms |

A rank costs what its graph does: one over edges both ways reads each entity's edges twice and takes twice as long,
and power iteration's rounds are within the spread. Building the graph is linear in the set, about 1.4 µs an entity
and direction at five edges.

**Heap** (`tests/maintenance_heap.rs`, release, dhat): a rank or weak components read holds 184 bytes an entity at five
edges, measured between 5,000 and 10,000 documents, against an allowance of 256. The graph itself is 24 of them.

**Instruction counts** (`benches/instructions.rs`): `graph` at 1,000 documents for each read, which CI counts on every
pull request and `scripts/diagnostics-overhead.py` gates. Their maps hash with fixed keys: with the standard library's
per-process keys, one build's personalized rank counted 2.9% fewer instructions than another's in one run and 0.0085%
in the next, which no comparison of two counts can gate on; with fixed keys the two builds agree to 3 instructions.

## Communities (stage 4)

Leiden through leiden-rs 0.8.1, no default features (`development-libraries.md`, stage 6).

- **Invariants** (`crates/elmdb/src/engine/algorithms/tests.rs`): over 128 random graphs and four seeds, every node is
  in exactly one community, every community is connected (Leiden's guarantee), the partition's modularity is never
  below every node alone, and a seed answers the same partition twice.
- **Known answer:** the karate club at resolution 1, seed 0: four communities of 12, 11, 6 and 5 members, modularity
  0.4197896, the club's best known (Brandes et al., 2008), with the two leaders, members 0 and 33, apart. The
  partition is recorded exactly and asserted on Linux, macOS and Windows.
- **Resolution:** 0.25 finds fewer communities than 2; at 0, every edge only adds, so each component is one community.
- **Through the store** (`crates/elmdb/src/engine/tests/ranks.rs`): over 64 random graphs, the engine's answer equals
  the communities of the graph built by hand from the same edges; two four-cliques joined by one edge answer as the
  two cliques, at seeds 0 and 7.
- **Time** (Criterion, the same graph as the other reads): 3.66 ms at 1,000 documents and 54.6 ms at 10,000, about
  four times `weakComponents`' 1.30 and 14.14: Leiden's rounds of moves, refinement and aggregation over the graph it
  copied, beside the reads every graph costs.
- **Heap:** about 764 bytes an entity at five edges, measured between 5,000 and 10,000 documents: leiden-rs copies
  the graph into its builder's edge list and a CSR holding each edge both ways. The heap test holds it to 1,024.
