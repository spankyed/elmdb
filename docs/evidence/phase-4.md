# Phase 4: writes and the model harness

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Rust 1.99.1.

## What was built

- **The batch protocol** (`engine/batch.rs`): every operation apack's write builder has, as data, with the TypeScript
  generated (`protocol/Batch.ts`, `Op.ts`, `OpResult.ts`, `ChangeSet.ts`).
- **Units** (`engine/mod.rs`, `engine/unit.rs`): `Database::write` applies a batch as one unit; `Database::begin`
  opens one held across calls, its write transaction kept with a `'static` lifetime the unit's ownership of the store
  makes sound, `!Send` as LMDB requires. Savepoints are an undo log per level; each batch in a unit is applied in one.
  Deferred checks (a relation's ends) run at commit, and a change set is gathered from the unit's events.
- **The rules** (`engine/apply.rs`, `entities.rs`, `relations.rs`, `numbering.rs`): apack's, from `write.ts`,
  `attribute-storage.ts`, `relations.ts` and `numbering.ts`, and the schema's: field types, undeclared fields,
  cardinality, `acyclic`, `unique` pairs, endpoints, unique value indexes, and the built-in short code and role
  indexes, every one kept by each attribute write.
- **The binding**: `db.write(batch)` and `db.begin()` returning a `Unit` with `write`, savepoints, `commit` and `abort`,
  batches and answers as msgpack.

## Decided while building

- **A value index may cover an `any` field**, holding its scalar values; apack's shapes give `any` for unions codegen
  can't map (`schema.md`).
- **A cycle check counts a trashed entity's edges**, where apack's skipped them (apack's `wouldCreateCycle` walked
  `out`, which skips the trash): restoring the entity would otherwise close the cycle.
- **A batch in a unit is a savepoint**, so a refused one leaves nothing; apack's writes joined the open unit with none,
  and a caller catching a refusal could keep part of an operation.

## The ports

apack's `tests/engine/writes.spec.ts`, `relations.spec.ts` and `transact.spec.ts`: their write cases are in
`engine/tests.rs`, 31 cases, each a batch, read back from the store's keys. Their query and walk cases are Phase 5's,
and `relations.spec.ts`' `replace` and `acyclicIn` cases became the schema's `unique` and `acyclic` (D9).

## The model harness

`engine/tests/model.rs`, ported from apack's `tests/_harness/`: 96 workloads per run, each a prelude creating every id
and 10 to 60 steps of every operation the protocol has, relink chunks among them, units nested as savepoints, and a
store refusing the next commit. After every step the store's rows, trash, roles and edges equal the model's, and so do
the derived keys: each type's live and trashed entities in creation order, its count, the value index's entries, the
role index, and each entity's edges out and in.

Each mutation below was made on a copy of the workspace; the harness failed on each, with a minimal failing workload:

| Mutation | Caught |
|---|---|
| An index update dropped (`entities::reindex` writes no new entry) | yes |
| A refusal skipped (a self-link allowed) | yes |
| A savepoint's rollback undoing nothing | yes |
| `updateRelation` not moving the relation | yes |

## Checks

`cargo test --workspace` (90 passed, 1 measurement ignored), `cargo clippy -- -D warnings`, `cargo fmt --check`,
`cargo xtask check-generated`, `cargo xtask check-one-lmdb`, the binding's `npm run typecheck` and `npm test` (9 passed).
