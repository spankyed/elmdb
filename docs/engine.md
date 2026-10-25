# The engine

The schema, the operations and the protocol. Status: designed, not built.

## Opening

```ts
const db = Database.open(path, { schema, readOnly?, mapSizeBytes?, syncIdleMs?, syncIntervalMs?, syncs? })
```

`syncs: false` skips group commit's syncs, for tests on a temporary directory, where a crash of the machine can't
matter; `durable()` then resolves at once.

The map starts at twice the data file, 64 MiB at least (`mapSizeBytes` sets it). A unit that fills it is aborted, the
map doubled while the process has no transaction open, and the unit run again, up to 16 times: a unit is data, so the
engine can run it again without the caller. On Windows LMDB sizes the file to the map, which is why it starts no
larger.

An open refuses files that aren't an elmdb database (`NOT_A_DATABASE`, which names `tools/lmdb-js-export` for files
lmdb-js wrote) and a database in another format (`WRONG_FORMAT`), and a new database is stamped with its format, and
synced, before anything else is written to it.

A database is one directory. A program that needs data with its own life cycle opens a second one: apack opens `main`
and `runs` (its flow run history), which reset and back up independently.

Several processes may open one database: one writes at a time, LMDB's lock serializing them, and a read-only open
reads while the app writes (apack's `apack db`).

## Schema

Types, fields, relation kinds, roles and indexes are declared as data (`schema.md`), stored in the database, and
compared at open. A type, field, relation kind or role the schema doesn't declare is refused everywhere it is named.

## Reads: plans

A call carries several reads, answered from one snapshot (or, in a unit, from its own transaction) in one msgpack
envelope, an answer per read. A read is a query plan, or a direct read: an entity's row, one of its fields (with
whether it is present), a relation's row, `findRelations` by kind and ends, and `wouldCreateCycle`.

A plan is a scope, a start, steps and a terminal:

- **Scope:** `live` reaches no entity in the trash, not as a start and not at a far end; `withTrashed` reaches the
  trash too; `trashed` starts from the trashed entities only.
- **Starts:** types (in creation order), ids (in the order given), or every entity. A type the schema doesn't declare is
  refused (`UNKNOWN_TYPE`), as a kind or role a read names is.
- **Filters:** `ofType`, `within`, `where` (a value, or a list holding it; a `null` matches a field that is null or
  missing), `has`, `lacks`, `withRole`, `linkedWith`, `linkedTo`, `linkedFrom`, `unlinkedTo`, `unlinkedFrom`.
- **Walks:** `out` and `in` (each far end once, of the types when given), `descendants` and `ancestors` (nearest first,
  each entity once, so a cycle ends the walk).
- **Shaping:** `orderBy` (booleans, numbers, timestamps, strings by the schema's collation, anything else by its JSON,
  then a missing or null value; ties keep the query's order), `reverse`, `limit`, `distinct`.
- **Terminals:** `ids`, `count`, `first`, `last`, `exists`, `rows` (whole rows, or the fields asked for), `firstRow`,
  `edges` (relation rows), `groupBy`.

A plan pulls ids through its steps one at a time, so `first`, `exists`, `firstRow` and `limit` stop once they have
their answer, and a live type's `count` is read off its counter. A `where` or `withRole` straight after a type start
reads a value index whose first field it names, a compound one's included.

`db.readSync(reads)` runs on the calling thread. `db.read(reads)` runs on a thread of the pool, in a snapshot of its
own, and resolves with the same answer, so a large read doesn't block the event loop.

A caller's own function can't cross: a builder holding one sends the plan up to that step, and runs the rest itself.

**Answers** are owned bytes: ids as strings, each row as a map of `id`, `entityType` and the fields' stored msgpack
bytes spliced in unchanged, counts as numbers. An answer holding rows says whether any map in it may hold a key
`__proto__` (`plainObjects`), so the caller can decode straight to objects when none does and keep the exact decode for
the rest; decoding is most of a long read's cost (`docs/evidence/phase-1.md`).

## Writes: batches and units

`db.write(batch, { durable? })` applies a batch as one unit. A unit can also stay open across calls, as a caller's
transaction running its own code between writes needs: `db.begin()` opens one, `unit.write(batch)` applies a batch in
it, its writes read their own, and `unit.commit({ durable? })` or `unit.abort()` ends it. One unit is open at a time,
and while it is `db.write` refuses (`UNIT_OPEN`): its writes go through the unit. The unit's write transaction belongs
to the thread that began it. A unit opening on a map more than half used grows the map first, since an open unit can't
grow it.

A batch is applied in a savepoint of its unit, so a refused operation keeps none of its batch, and the unit goes on.

The operations:

- **Operations:** create (a whole row; the engine assigns the id unless an import brings one, the code, `createdAt`
  and `updatedAt`), upsert, set, set many, append to a list, merge into an object, delete a field, remove from a list,
  trash, restore, grant and revoke a role, grant exclusively among entities, link (with info, replace, and refusing a
  cycle in a kind), unlink, unlink all, update and remove a relation, destroy.
- **Savepoints:** `unit.savepoint()` opens one and answers its depth; `unit.rollbackTo(depth)` undoes everything since
  it, and `unit.release(depth)` keeps its writes in the unit around it, as apack's nested `transact` does. A savepoint
  is an undo log of each key's previous value, so rolling one back costs what it wrote.
- **Refusals** are the engine's, each an error code: an undeclared type, kind or role, a missing required field, an id
  in use, an id of another type, no such entity or relation, a reserved field, a value that isn't a list, an invalid
  or taken code, a value its field's type doesn't take (`FIELD_TYPE`), a field its type doesn't declare
  (`UNKNOWN_FIELD`), and a link its kind's rules don't allow (`SELF_LINK`, `ENDPOINT`, `CARDINALITY`, `CYCLE`; a cycle
  counts every edge, a trashed entity's included, since restoring it would close the cycle). A relation's ends are
  checked at commit, so a unit may link to an entity it creates later.
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
- `check()`: derives every derived key from primary data and compares it with what is stored (a built index's entries
  included, an unbuilt one's left out), and reports each difference as missing, extra or differing; and reports each
  row that breaks a rule of the schema: a missing required field, a value its field doesn't take, a field its type
  doesn't declare, a `ref` naming no entity, a relation with a missing end or past its kind's cardinality. heed exposes
  no walk of LMDB's pages, so damage to the B+tree itself is what LMDB reports as `CORRUPTED` when a read reaches it.
- `rebuild(index?)`: deletes derived data, every index's or one index's, and writes what primary data derives, in one
  durable unit.
- Index builds: an index is read once built (its `d/xb` marker). One added over existing data is built by a thread of
  the database's in chunks of 1,000 entities, each a unit of its own, so writes go on between them; writes keep every
  index all along, so a build fills in only what predates it. A unique index over rows holding the same values is left
  unbuilt, and `indexStatus()` names them. Closing stops a build between chunks, and the next open finishes it.
- `purge(what)`: deletes what a removed declaration left: a type's entities, a field of a type, a relation kind's
  relations, a role. It refuses a name the schema still declares.
- `renameType`, `renameField`, `renameRelation`: move stored data to a name the schema declares, in one durable unit.
  Renaming a type rewrites its entities' ids, the relations joining them and the declared `ref` fields naming them.
- `bulkLoad(rows, relations)`: one unit without per-write checks, for imports and upgrades.
- Raw writes for tests that need data the checks would refuse.

## Errors

Every failure is an `ElmdbError` with a code from one enum, the same in Rust and TypeScript. LMDB's own errors map to
codes too (`MAP_FULL`, `READERS_FULL`, `CORRUPTED`). LMDB's assertions are compiled out of a release build (heed's
`lmdb-master-sys` defines `NDEBUG` outside debug builds), so a release build never aborts on one; damage LMDB doesn't
detect itself is what `check()` is for (`docs/evidence/phase-1.md`).
