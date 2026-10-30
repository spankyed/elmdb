# Graph traversal

Status: deferred, 2026-10-30. Nothing here is needed by apack today; this records what graph query languages (Cypher,
Gremlin) answer that a plan doesn't, how far each gap is from the engine as it stands, and the order to take them in
when a caller needs one.

elmdb is described as an embedded database, not a graph database, because a graph database promises pattern matching
over bound variables (item 5), which a plan doesn't do.

## What a plan answers now

A plan starts from types, ids or everything, applies steps, and answers one ordered set of entities
(`crates/elmdb/src/engine/plan.rs`, `docs/architecture/engine.md`, Reads):

- one hop out, in or both ways by relation kind, of the types given (`out`, `in`, `both`);
- filters on whether an edge exists (`linkedTo`, `linkedFrom`, `unlinkedTo`, `unlinkedFrom`, `linkedWith`);
- every entity reached through the kinds, nearest first, each once, at most `depth` edges away when given
  (`descendants`, `ancestors`);
- an unweighted shortest path from the set to an entity, following edges out, in or both ways (`pathTo`);
- whether a link would close a cycle (`wouldCreateCycle`), the edges out of or into a set as relation rows (`edges`),
  and each entity's count of them (`degrees`);
- ordering, distinct, paging, counts and grouping.

A relation carries an `info` value (`Link`, `UpdateRelation` in `batch.rs`), not schema-declared fields, and no index
covers it.

## Small

1. **`edges` takes a `direction`.** `pathTo` and `degrees` name a side as `out`, `in` or `both`; `edges` still takes
   `outward: bool`, so it answers one side at a time. Moving it to `direction` changes apack's one caller with it.

## Medium

2. **A plan's answer as a set inside another plan.** `within` takes a fixed list of ids; taking another plan's answer,
   read from the same snapshot, gives intersection, union and difference, which covers "people who know both A and B"
   and "documents linked to X but not Y". The tracked-read and EXPLAIN accounting has to follow the inner plan.
3. **A hop filtered by its relation's info** (Cypher `-[r {since: 2020}]->`). Each hop would read the relation rows
   it crosses, unindexed. Worth doing only after relation info gains declared fields in the schema, which is its own
   change.
4. **Weighted shortest path** (Dijkstra over a numeric info field). Depends on 3.

## Large: a different answer shape

5. **Pattern matching that binds names and answers tuples** (`MATCH (a)-->(b)<--(c) RETURN a, b, c`), and optional
   matches. A plan answers one set of entities; this needs rows of bindings through every step, terminal and the
   protocol. This is the line between elmdb and a graph database, and nothing apack does needs it: several reads in one
   snapshot answer the same questions.
6. **Every path a match took**, not only its ends.
7. **Graph algorithms** (PageRank, connected components, community detection). These belong in a library over the
   engine's reads, not in plans.

## When to take it up

Each waits for a caller of its own. A step or answer is added to the plan (`plan.rs`, `reads.rs`), its TypeScript
declarations regenerated (`cargo xtask generate`), given an EXPLAIN stage name, and tested case by case and against a
plain reference over random graphs, as `crates/elmdb/src/engine/tests/graph.rs` tests the walks.
