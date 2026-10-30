# Graph traversal

Status: deferred, 2026-10-30. Nothing here is needed by apack today; this records what graph query languages (Cypher,
Gremlin) answer that a plan doesn't, how far each gap is from the engine as it stands, and the order to take them in
when a caller needs one.

elmdb is described as an embedded database, not a graph database, because a graph database promises pattern matching
over bound variables (item 7), which a plan doesn't do. Items 1–5 would make "graph-shaped data with traversal and
shortest paths" an honest claim without changing that.

## What a plan answers now

A plan starts from types, ids or everything, applies steps, and answers one ordered set of entities
(`crates/elmdb/src/engine/plan.rs`):

- one hop out or in by relation kind, of the types given (`Out`, `In`);
- filters on whether an edge exists (`LinkedTo`, `LinkedFrom`, `UnlinkedTo`, `UnlinkedFrom`, `LinkedWith`, either way);
- every entity reached through the kinds, any number of edges deep, nearest first, each once (`Descendants`,
  `Ancestors`: `reach` in `reads.rs`, a level-by-level breadth-first walk);
- whether a link would close a cycle (`WouldCreateCycle`), and the edges out of or into a set as relation rows (`Edges`);
- ordering, distinct, paging, counts and grouping.

A relation carries an `info` value (`Link`, `UpdateRelation` in `batch.rs`), not schema-declared fields, and no index
covers it.

## Small: extensions of the existing walk

1. **A depth bound on `Descendants` and `Ancestors`** (Cypher `*1..3`). `reach` already advances one level per pass, so
   this is an optional `depth` counted by those passes. It also bounds the walk's work, which today collects every
   reachable entity before the first is visited.
2. **A hop both ways** (Cypher `-[:KIND]-`, Gremlin `both()`). `LinkedWith` filters on an edge either way, but no step
   follows one. A `Both` step is `Out` and `In` over the same kinds, each far end once.
3. **The shortest path between two entities** (Cypher `shortestPath`), unweighted. The same breadth-first walk,
   recording each entity's parent and stopping at the target, answering the ids along the path from start to target,
   or none. A start set of one entity and a target id; several of either is a later question.
4. **Degree**: how many edges of the kinds each entity of the set has, as a terminal. One count per entity over what
   `Edges` already reads.

## Medium

5. **A plan's answer as a set inside another plan.** `Within` takes a fixed list of ids; taking another plan's answer,
   read from the same snapshot, gives intersection, union and difference, which covers "people who know both A and B"
   and "documents linked to X but not Y". The tracked-read and EXPLAIN accounting has to follow the inner plan.
6. **A hop filtered by its relation's info** (Cypher `-[r {since: 2020}]->`). Each hop would read the relation rows
   it crosses, unindexed. Worth doing only after relation info gains declared fields in the schema, which is its own
   change.
7. **Weighted shortest path** (Dijkstra over a numeric info field). Depends on 3 and 6.

## Large: a different answer shape

8. **Pattern matching that binds names and answers tuples** (`MATCH (a)-->(b)<--(c) RETURN a, b, c`), and optional
   matches. A plan answers one set of entities; this needs rows of bindings through every step, terminal and the
   protocol. This is the line between elmdb and a graph database, and nothing apack does needs it: several reads in one
   snapshot answer the same questions.
9. **Every path a match took**, not only its ends.
10. **Graph algorithms** (PageRank, connected components, community detection). These belong in a library over the
    engine's reads, not in plans.

## When to take it up

Take 1–3 together, when a caller first needs a bounded walk or a path: they share `reach` and its tests. 4 and 5 each
wait for a caller of their own. Each step is added to the plan, its TypeScript declarations regenerated
(`cargo xtask generate`), the model suite (`docs/architecture/testing.md`) taught the step, and EXPLAIN given its stage.
