# The engine

The schema, the operations and the protocol. Status: designed, not built.

## Opening

```ts
const db = Database.open(path, { schema, readOnly?, mapSizeBytes?, syncIdleMs?, syncIntervalMs?, syncs? })
```

`syncs: false` skips group commit's syncs, for tests on a temporary directory, where a crash of the machine can't
matter; `durable()` then resolves at once.

A database is one directory. A program that needs data with its own life cycle opens a second one: apack opens `main`
and `runs` (its flow run history), which reset and back up independently.

Several processes may open one database: one writes at a time, LMDB's lock serializing them, and a read-only open
reads while the app writes (apack's `apack db`).

## Schema

Types, fields, relation kinds, roles and indexes are declared as data (`schema.md`), stored in the database, and
compared at open. A type, field, relation kind or role the schema doesn't declare is refused everywhere it is named.

## Reads: plans

A plan is a start, steps and a terminal:

- **Starts:** a type or types, ids, every entity, the trash.
- **Filters:** a field's value, has or lacks a field, a role, a type, within ids, linked to or from (an entity, or
  any), not linked, linked either way.
- **Walks:** out and in along relation kinds, descendants and ancestors (each entity once, so a cycle ends the walk),
  edges as relation rows.
- **Search:** `search`, `near`, `hybrid` (`search.md`).
- **Shaping:** order by a field (the engine's collation), reverse, distinct, limit, a cursor after a position, group by.
- **Terminals:** ids, count, exists, first, last, rows (chosen fields, or all), edges, scores and highlights for a
  search.

`db.readSync(plan)` runs on the calling thread. `db.read(plan)` runs on a reader thread in its own snapshot and
resolves with the same result, so a large plan doesn't block the event loop.

A caller's own function can't cross: a builder holding one sends the plan up to that step, and runs the rest itself.

**Results** are owned bytes in one envelope: ids as strings, each row's values as the value bytes the caller wrote,
counts and scores as numbers. The caller decodes values.

## Writes: batches

`db.write(batch, { durable? })` applies one unit:

- **Operations:** create (a whole row; the engine assigns the id unless an import brings one, the code, `createdAt`
  and `updatedAt`), upsert, set, set many, append to a list, merge into an object, delete a field, remove from a list,
  trash, restore, grant and revoke a role, grant exclusively among entities, link (with info, replace, and refusing a
  cycle in a kind), unlink, unlink all, update and remove a relation, destroy.
- **Savepoints:** a batch may hold nested groups; a failing group rolls back alone when the batch says so, as apack's
  nested `transact` does.
- **Refusals** are the engine's, each an error code: an undeclared type, kind or role, a missing required field, an id
  in use, an id of another type, no such entity or relation, a reserved field, a value that isn't a list, an invalid
  or taken code. A relation's ends are checked at commit, so a batch may link to an entity it creates later in the
  batch.
- **The result** carries what the batch created (ids, codes), the rows asked for back, and the commit's change set.

A batch is data, so the engine can retry it (after growing the map, `design.md`) without involving the caller.

## Durability

A unit is visible when it commits and survives the process crashing once `write` returns. Surviving the machine
failing is a sync: group commit syncs once writes pause for `syncIdleMs` (100 ms) and at most `syncIntervalMs` (1 s)
after the first unsynced commit, on a background thread. `db.durable()` resolves once everything committed before it is
on disk, and rejects if the sync it waits on fails, leaving those commits owed to the next one. `durable: true` on a
write commits with a sync. `close()` syncs what is owed.

## Change sets and watch

Every commit produces a change set: the entities created, changed, trashed, restored and destroyed, by type, with the
fields and relation kinds touched.

- `db.changes(listener)` hears every change set after its commit.
- `db.watch(plan, listener)` runs the plan, records what it read (types, fields, relation kinds, ids), and runs it
  again after each commit whose change set touches that, handing the listener the new result. This is how a view stays
  current without the code that writes knowing who is looking.

## Admin

- `backup(dir)`: LMDB's copy of one read transaction, compacted.
- `replaceWith(dir)`: every entry replaced by another database's, in one durable unit, the database staying open.
- `reset()`: every entry removed, in one durable unit.
- `stats()`: entries and bytes per type, field and index, index build progress, pending embeddings.
- `check()`: walks LMDB's pages and free list, rebuilds every derived structure in a scratch transaction and compares
  it with what is stored, and reports each difference.
- `rebuild(index?)`: drops derived data and rebuilds it from the primary data.
- `bulkLoad(rows, relations)`: one unit without per-write checks, for imports and upgrades.
- Raw writes for tests that need data the checks would refuse.

## Errors

Every failure is an `ElmdbError` with a code from one enum, the same in Rust and TypeScript. LMDB's own errors map to
codes too (`MAP_FULL`, `READERS_FULL`, `CORRUPTED`). LMDB's assertions are compiled out of a release build (heed's
`lmdb-master-sys` defines `NDEBUG` outside debug builds), so a release build never aborts on one; damage LMDB doesn't
detect itself is what `check()` is for (`docs/evidence/phase-1.md`).
