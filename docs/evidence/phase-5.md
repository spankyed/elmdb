# Phase 5: reads and value indexes

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Rust 1.99.1.

## What was built

- **The read protocol** (`engine/plan.rs`): several reads per call, each a query plan (scope, start, steps, terminal)
  or a direct read, with the TypeScript generated (`protocol/Reads.ts` and what it names).
- **The executor** (`engine/reads.rs`): apack's `query.ts` semantics step for step, a pull pipeline so the early
  terminals stop early, the value index read for a `where` or `withRole` straight after a type start, a type's count off
  its counter, ICU's collation for `orderBy` (D8), and one msgpack envelope of answers with rows' values spliced in.
- **Reads in the binding**: `readSync`, `read` on the thread pool (rejecting with elmdb's code), and `unit.read`.

## Decided while building

- **A `null` probe matches a missing field as well as a stored null, always.** apack's walk matched both and its index
  read only stored nulls, so one query answered two ways depending on whether the type had an index. elmdb walks for a
  null probe.
- **A compound index serves a lookup of its first field**: its entries under that field's value are a prefix range.

## The ports

apack's `tests/engine/queries.spec.ts` and the walk cases of `relations.spec.ts`: in `engine/tests/reads.rs`, 14
cases. A case passing a JavaScript predicate is the facade's to run (Phase 9); the cases counting what a predicate saw
count the entities a plan visited instead (`first_exists_limit_and_count_read_only_as_far_as_their_answer`):

| Read | Entities visited |
|---|---|
| `first`, `exists`, `firstRow` of a type | 1 |
| `limit(2).ids()` | 2 |
| `has('title').count()` over four | 4 |
| a type's `count()` | 0, its counter read |
| an indexed `where` with one match | 1 |
| an unindexed `where` over four | 4 |

## The model harness

After every step it now also answers about a hundred plan reads in one call and compares each with the model: each
type's live, trashed and `withTrashed` ids in creation order, its count, `where` on the indexed and an unindexed field
for every probe the workload writes (long strings no index holds included), `withRole`, and each id's `out` and `in`
for each kind. A mutation letting the index read admit trashed entities was caught (on a copy of the workspace).

## Checks

`cargo test --workspace` (104 passed, 1 measurement ignored), `cargo clippy -- -D warnings`, `cargo fmt --check`,
`cargo xtask check-generated`, `cargo xtask check-one-lmdb`, the binding's `npm run typecheck` and `npm test` (10 passed).
