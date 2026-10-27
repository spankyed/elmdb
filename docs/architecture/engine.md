# The engine

The schema, the operations and the protocol. Built: `crates/elmdb/src/engine/`.

## Opening

```ts
const db = Database.open(path, { schema, readOnly?, mapSizeBytes?, syncIdleMs?, syncIntervalMs?, syncs? })
```

`syncs: false` skips group commit's syncs, for tests on a temporary directory, where a crash of the machine can't
matter; `durable()` then resolves at once.

The map starts at twice the data file, 64 MiB at least (`mapSizeBytes` sets it). A unit that fills it is aborted, the
map doubled while the process has no transaction open, and the unit run again, up to 16 times: a unit is data, so the
engine can run it again without the caller. Mapping capacity and physical file length are separate. On Windows,
a read-only view covers the current physical file: its configured bound is capped to that length at open and when
adopting external growth, so newly appended pages trigger remapping before access. Read-only handles cannot grow
the writable capacity. Writable mappings retain the normal capacity policy.

An open refuses files that aren't an elmdb database (`NOT_A_DATABASE`, which names lmdb-js for files lmdb-js
wrote) and a database in another format (`WRONG_FORMAT`), and a new database is stamped with its format, and
synced, before anything else is written to it.

A database is one directory. A program that needs data with its own life cycle opens a second one: apack opens `main`
and `runs` (its flow run history), which reset and back up independently.

Several processes may open one database: one writes at a time, LMDB's lock serializing them, and a read-only open
reads while the app writes (apack's `apack db`). Read-only access still requires writable reader registration in
LMDB's lock file. A permission failure is refused; it never silently disables locking.

The Node binding takes a nonblocking exclusive file lock before its first database open. One binary owns the process
claim until process exit, including after its databases close or its first worker ends. Another physical binding copy
is refused with `ALREADY_LOADED`; a claim-file I/O failure is refused with `IO`. Independent processes have independent
claims. Claim files are named by real user ID and PID in `/tmp` on Unix, and by PID in the native process temporary directory on Windows. They are not
removed: a stale unlocked file is reusable, while unlinking a live claim would break exclusion.

On macOS LMDB's lock takes two named POSIX semaphores per database, which the last process to close it removes. The
Node binding closes what is still open as the process or a worker thread ends (`crates/elmdb-node/src/exit.rs`); a
process killed by a signal, or one ending with a unit open, leaves them in the system's table until a later open of
the same path, taking the lock first, replaces them and a close removes them.

## Schema

Types, fields, relation kinds, roles and indexes are declared as data (`schema.md`), stored in the database, and
compared at open. A type, field, relation kind or role the schema doesn't declare is refused everywhere it is named.

A handle caches its declaration and the stored schema's SHA-256 fingerprint. Each schema-dependent read, export,
write, unit begin and background build validates that fingerprint inside its transaction. If another process has
changed it, `SCHEMA_CHANGED` refuses the operation before using stale rules or changing data; close and reopen the
handle. `schema()` reports the handle's cached declaration. Schema changes compute their effects and primary-data
inventory inside the same write transaction that installs them, so concurrent data cannot invalidate the decision.
Local reads hold the schema guard through snapshot execution; export captures its schema/snapshot pair before
releasing that guard for callbacks. Post-commit watch queries fetch the current declaration for every rerun.

Tracked reads and watch notifications belong to this process. Writes in other processes are visible in subsequent
snapshots and `sequence()` reads the persisted commit counter, but those writes do not dispatch this handle's watch
callbacks. Coordinate writes in one process when observers must hear every commit.

## Reads: plans

A call carries several reads, answered from one snapshot (or, in a unit, from its own transaction) in one msgpack
envelope, an answer per read. A read is a query plan, or a direct read: an entity's row, one of its fields (with
whether it is present), a relation's row, `findRelations` by kind and ends, `wouldCreateCycle` (the walk a link's
`CYCLE` check makes, a trashed entity's edges included), and search's
`suggest` and `findLabels`. A search naming an index its type lacks, or none on a type with none or several, is refused
(`UNKNOWN_INDEX`).

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
  then a missing or null value; ties keep the query's order), `reverse`, `limit`, `distinct`, `after` (the entities
  after an id, a page's cursor; an id no longer in the answer answers nothing, so a caller pages again from the
  start).
- **Search:** `search` (a text index's matches, most relevant first; `search.md`, Text search as built).
- **Terminals:** `ids`, `count`, `first`, `last`, `exists`, `rows` (whole rows, or the fields asked for), `firstRow`,
  `edges` (relation rows), `groupBy` (ids by value, or with `counts` how many), `hits` (what a search found in each
  entity).

A plan pulls ids through its steps one at a time, so `first`, `exists`, `firstRow` and `limit` stop once they have
their answer, and a live type's `count` is read off its counter. A `where` or `withRole` straight after a type start
reads a built value index on that one field (a `where` on a value other than null): a compound index is not read, since
it holds no entry for an entity missing another of its fields, and without such an index the type's entities are walked.

A read takes an idle read transaction while it is still the last commit's snapshot (its id the environment's last
committed one, which a commit in any process moves), and opens one otherwise; a commit here drops the idle ones, so none
pins the pages it freed. `db.readSync(reads)` runs on the calling thread. `db.read(reads)` runs on a thread of the pool, in a snapshot of its
own, and resolves with the same answer, so a large read doesn't block the event loop.

A caller's own function can't cross: a builder holding one sends the plan up to that step, and runs the rest itself.

**Answers** are owned bytes: ids as strings, each row as a map of `id`, `entityType` and the fields' stored msgpack
bytes spliced in unchanged, counts as numbers. An answer holding rows says whether any map in it may hold a key
`__proto__` (`plainObjects`), so the caller can decode straight to objects when none does and keep the exact decode for
the rest; decoding is most of a long read's cost (`docs/evidence/phase-1.md`).

## Writes: batches and units

`db.write(batch, durable?)` applies a batch as one unit. A unit can also stay open across calls, as a caller's
transaction running its own code between writes needs: `db.begin()` opens one, `unit.write(batch)` applies a batch in
it, its writes read their own, and `unit.commit(durable?)` or `unit.abort()` ends it. One unit is open at a time,
and while it is `db.write` refuses (`UNIT_OPEN`): its writes go through the unit. The unit's write transaction belongs
to the thread that began it. A unit opening on a map more than half used grows the map first, since an open unit can't
grow it. A background index build waits for an open unit to end, woken when it does, and goes on from where it
stopped; so does any other write but the unit's.

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
  or taken code (a code's number is at most 2^53 - 1, JavaScript's largest exact integer, and a numbering that would
  pass it is refused, `NUMBERS_EXHAUSTED`), a value its field's type doesn't take (`FIELD_TYPE`), a field its type doesn't declare
  (`UNKNOWN_FIELD`), and a link its kind's rules don't allow (`SELF_LINK`, `ENDPOINT`, `CARDINALITY`, `CYCLE`; a cycle
  counts every edge, a trashed entity's included, since restoring it would close the cycle), or a unique kind's edge
  moved onto a pair another edge joins (`DUPLICATE_EDGE`). A relation's ends are checked at commit, so a unit may link
  to an entity it creates later.
- **The result** carries what the batch created (ids, codes), the rows asked for back, and the commit's change set.

A batch is data, so the engine can retry it (after growing the map, `design.md`) without involving the caller.

## Durability

A unit is visible when it commits and survives the process crashing once `write` returns. Surviving the machine
failing is a sync: group commit syncs once writes pause for `syncIdleMs` (100 ms) and at most `syncIntervalMs` (1 s)
after the first unsynced commit, on a background thread. `db.durable()` resolves once everything committed before it is
on disk, and rejects if the sync it waits on fails. A failed sync is final until the database is reopened: an OS may
mark the pages it couldn't write clean and drop them (PostgreSQL's "fsyncgate"), so a later sync that succeeds proves
nothing about them, and every later commit is built on them; every wait after it rejects, naming it. A write or a
commit passed `durable` commits with a sync, and throws `NOT_DURABLE` if that sync fails, the commit having been made
and its change set delivered. Admin and schema mutations use the same finalization: they report `NOT_DURABLE` after
notifying watches, with committed data and in-memory schema already installed. Empty-index markers are settled inside
the schema/reset/replace transaction, not by a second commit. A caller must not retry the mutation as though it had been
refused; the error names its sequence, and `db.sequence` gives the current commit without parsing the message.
`close()` syncs what is owed, and reports a sync that failed.

After the atomic reset, a small optional LMDB checkpoint rewrites the sequence value read within its own transaction,
without changing logical sequence, schema, data or notifications. Advancing the physical generation lets a subsequent
bulk load reuse the reset's freed pages: LMDB protects the immediately preceding generation even with no readers.
The checkpoint runs before reset's required sync. If it aborts, reset still syncs its committed mutation; only the
space/performance benefit is lost. Active read snapshots retain LMDB's normal protection. Measured rationale and
failure/page-reuse regressions are recorded in [remediation evidence](../evidence/elmdb-remediation.md).

## Change sets and watch

Every commit produces a change set: the entities created, changed, trashed, restored and destroyed, by type, with the
fields and relation kinds touched, and the commit's **sequence**, a number that only increases for as long as the
database exists (`m/sequence`, raised by every logical commit, a reset's and a replace's included; reset's optional physical
checkpoint does not raise it). A batch's result carries
the change set and the sequence, and so does a unit's commit, so a caller can tell whether a result it was handed
reflects its own write (`result.sequence >= written.sequence`): read your writes, across two channels that may deliver
in either order. `db.sequence` is the last commit's.

- **Tracked reads.** A read call may ask to be tracked under a token (`{ reads, track: token }`). elmdb records the key
  ranges those reads touched — each key a read got, present or not, and each range it scanned, as far as the scan went
  (to just past the last key a stopped scan was shown) — and adds them to what the token holds, in an interval index
  held in Rust (`engine/tracking.rs`: a map of keys and an interval treap over ranges). A read through hannoy, which
  elmdb can't see into, records the index's items instead, which every vector added or removed writes. A commit's
  change set names the tokens whose keys and ranges it wrote into (`touched`), found through that index in
  O(log N + matches) a key, so a commit's cost doesn't grow with the reads being tracked; a unit logs the keys it
  writes only while some token is tracked. `db.untrack(token)` drops what a token holds, and a caller re-running its
  reads drops it first. It also cancels dependency registration by any read already in flight under that token.
  Tracked queries normally execute outside the tracking mutex. The commit sequence is checked under that mutex
  before dependencies are filed: an intervening commit retries the query once, then a second conflict uses the locked
  path. This bounds retries under continuous writes and preserves notification ordering. In-flight token reads keep
  write logging enabled; removing a token cancels their registrations. The same sequence validation applies to watches.
  Nothing larger than a token crosses the boundary, and a view is told about exactly the commits
  that wrote into what it read. A commit whose writes aren't logged key by key — `reset`, `replaceWith`, `setSchema`,
  `bulkLoad` and the admin changes — is one the caller hears as writing everything.
- `Database::watch(reads, listener)` is a convenience over tracked reads, in Rust: it runs the reads tracked under a
  token of its own, and runs them again after each commit that names it, handing the listener the new answer when it
  differs from the last; after a commit whose writes aren't logged, every watch runs again. The listener is called
  after the commit, outside its transaction, on the thread that committed. An unwinding listener panic removes that
  watch and its tracked reads; the panic hook still runs. Other watches continue, and a committed operation retains
  its ordinary commit/sync outcome. An initial listener panic returns `CALLBACK_PANICKED` without leaving a watch.
  This recovery requires unwinding; aborting panics terminate the process. Closing this database from its callback
  returns `CALLBACK_ACTIVE` immediately, including timed close; close it after the callback returns instead.

**Why tracked reads, not only `watch(plan)`:** apack's views are functions over its repositories — several plans and
filtering in JavaScript between them — and a caller's own function can't cross (above, Reads). apack's engine keeps a
view current by running the function again when a commit names its token (apack's `docs/plans/live-queries.md`). Its
JavaScript engine already does this over lmdb-js, tracking key ranges and matching commits through the same kind of
index (`@apackdev/ears`, `engine.watch`), so the contract is one both engines keep and the move swaps the
implementation under it. Tracking key ranges rather than types, fields and ids is also the more precise of the two: an
edit to one entity names no token whose reads didn't touch that entity.

## Admin

The admin calls return no change set: each commits writes that aren't logged key by key, so a caller tracking reads
hears every one as touching everything (`db.sequence` rises with it). Each that writes refuses with `UNIT_OPEN` while a
unit is open.

- `backup(dir)`: LMDB's copy of one read transaction, compacted (`mdb_env_copy2` with `MDB_CP_COMPACT`), which opens
  as a database of its own. A private temporary file is synced and atomically published; the destination is reserved
  against this process's opens and other publishers. The caller must coordinate exclusive destination ownership
  across other processes: a backup artifact is not an actively used database. Failure before publication preserves
  the old backup. Unix syncs affected directories; Windows requests a write-through replacement. If directory sync
  fails after publication, `IO` reports that the complete new backup is visible and retrying is safe. Filesystem and
  device durability still depend on the platform honoring its sync operations.
- `replaceWith(dir)`: every entry replaced by another elmdb database's primary data, with derived data rebuilt under
  this database's schema, in one durable unit, the database staying open.
- `reset()`: every entry removed but the database's own (its format and schema), in one durable unit.
- `stats()`: entities per type (live and trashed), each field's entities and bytes, relations per kind, entries per
  index, the database's entries, file and map sizes; `indexStatus()` says which indexes are built.
- `check()`: derives every derived key from primary data and compares it with what is stored (a built index's entries
  included, an unbuilt one's left out), and reports each difference as missing, extra or differing; and reports each
  row that breaks a rule of the schema: a missing required field, a value its field doesn't take, a field its type
  doesn't declare, a `ref` naming no entity (including nested lists), duplicate unique values, and relations with
  missing ends, self-links, disallowed endpoint types, invalid/missing info, duplicate unique pairs, excessive
  cardinality or cycles in acyclic kinds. Constraint auditing reads primary values/relations even when an index is
  damaged or unbuilt; rebuilding cannot hide these violations. Unique values include values an index cannot store,
  and use the write contract's numeric, map, single-field list and compound equality rules. heed exposes no walk of
  LMDB's pages, so damage to the B+tree itself is what LMDB reports as `CORRUPTED` when a read reaches it.
- `rebuild(index?)`: deletes derived data, every index's or one index's, and writes what primary data derives, in one
  durable unit. One index is refused when the schema doesn't declare it (`UNKNOWN_INDEX`) or a build of it couldn't
  finish (`UNIQUE` over rows holding the same values, `VALUE_INVALID` for an entry past LMDB's key); a rebuild of
  everything leaves such an index unbuilt, as a build does.
- Index builds: an index is read once built (its `d/xb` marker). One added over existing data is built by a thread of
  the database's in chunks of 1,000 entities, each a unit of its own, so writes go on between them; writes keep every
  index all along, so a build fills in only what predates it. A unique index over rows holding the same values is left
  unbuilt, and `indexStatus()` names them. Each chunk seeks after its last entity sequence in both live/trash
  ranges and sorts only their bounded candidates: at most 2 × (chunk size + 1) rows in temporary storage. Cursor
  visits grow linearly at fixed chunk size; sorting costs O(chunk size × log(chunk size)) per chunk. Closing stops
  a build between chunks, and the next open finishes it.
- `purge(what)`: deletes what a removed declaration left: a type's entities, a field of a type, a relation kind's
  relations, a role. It refuses a name the schema still declares.
- `renameType`, `renameField`, `renameRelation`: move stored data to a name the schema declares, in one durable unit.
  Renaming a type rewrites its entities' ids, the relations joining them and the declared `ref` fields naming them, its own included, and moves its embeddings.
- `bulkLoad(loads)`: entities, relations under their own ids, sequences, embeddings and fields taken away, applied in
  one durable unit without a write's checks (no schema, no refusals, a unique index taking duplicates), for imports, a
  dump's load, and specs that need data the checks would refuse. The derived keys are kept as a write keeps them, and
  `check()` reports what the rules refuse.
- `primary_loads` (Rust), `elmdb dump` and `elmdb load`: a database's primary data as the loads that make it again, in
  one moment and the order it was made, written to a file and read into a new database (`format.md`, Dumps).

### Recovery resources

`check()` and full `rebuild()` spill derived entries to owned scratch files: an 8 MiB capacity-accounted row
buffer plus the largest row. A merge keeps one head from each of at most 16 input runs plus pending/current rows
and I/O buffers, so merge memory also depends on the largest row. Sorting costs O(D log D) comparisons for D
emitted entries and uses temporary disk proportional to their encoded bytes. Rebuild deletes batches of at most
1,000 keys without retaining stored values. Replacement streams primary ranges from one source snapshot instead
of collecting source data. These bound those intermediate collections, not the complete operation: entity/graph
inventory, uniqueness auditing, text/vector aggregation, output reports and LMDB's dirty/resident pages remain
workload-dependent. Compound value indexes can expand with the product of list cardinalities. Single-index rebuilds
also retain index-specific work; the measurements below concern full rebuilds.

The validated operating envelope is the explicit synthetic shapes in the
[remediation evidence](../evidence/elmdb-remediation.md), measured on 2026-10-26 on a 32 GiB macOS host with local
storage, fresh processes and uncontrolled filesystem cache. It is not a row-count guarantee for arbitrary schemas
or a latency SLA. The reference shape has 100,000 entities with 1,024-byte bodies, 199,997 relations, unique slug
and state value indexes, and one title/body text index. Current full rebuild/replacement measured 14.04/22.73 s
and 879,200/992,400 KiB peak RSS, meeting the preselected 2 GiB process / 30 s writer-occupancy upper-bound budget.
At twice that population, measured rebuild/replacement used 1,876,128/1,856,864 KiB and 28.12/47.69 s: replacement
exceeds the reference time budget, so schedule a longer maintenance window. A private staged load of that larger
dump took 79.64 s and 1,932,001,280 bytes RSS. The recorded 2,000-vector (1,536 dimensions) and 2,000-row/64-KiB
value profiles also pass recovery and independent restoration checks; they do not establish arbitrary vector capacity.

For these measured shapes, reserve at least 2 GiB on the scratch filesystem and the current data-file size plus
2 GiB of free destination space per active recovery operation; add space for a dump if retaining one. These are
conservative planning allowances over measured macOS artifacts/scratch, not enforced quotas or universal bounds.
Add both allowances when the filesystems share a volume; check them separately when temporary storage is elsewhere.
Measure Windows disk allocation separately; a mapping's reserved capacity does not establish physical file length.
Use `scripts/measure-recovery.mjs` on an owned representative
copy before using materially larger values, more relations, higher-dimensional/more numerous vectors, different
vocabulary, compound/list indexes or an unmeasured schema. Keep one recovery operation active while measuring.
The runner refuses to adopt an existing workload directory and can sample scratch with `--observe`; sampled peaks
are lower bounds. `TMPDIR`/the platform temporary directory controls scratch placement.

Check and backup take read snapshots rather than the LMDB writer, but long snapshots pin pages and can delay map
growth. Rebuild/replacement hold the destination writer for their atomic transaction; their entire measured operation
time is a conservative occupancy upper bound. Dump loading writes only a private staging database until no-clobber
publication. A scratch or pre-publication failure aborts/removes owned work and preserves the earlier database or
backup. Retrying after an explicit post-publication sync error must respect whether a complete result is already
visible (`format.md`, Dumps). Filesystem/device sync behavior remains a platform dependency.

## Errors

Every failure elmdb reports carries a code from one enum, the same in Rust and TypeScript (`ErrorCode`): an `Error`
in Rust, and in Node an `Error` whose `code` is it. An argument of the wrong JavaScript type is refused by N-API before
elmdb sees it, with N-API's own code. LMDB's own errors map to
codes too (`MAP_FULL`, `READERS_FULL`, `CORRUPTED`). LMDB's assertions are compiled out of a release build (heed's
`lmdb-master-sys` defines `NDEBUG` outside debug builds), so a release build never aborts on one; damage LMDB doesn't
detect itself is what `check()` is for (`docs/evidence/phase-1.md`).
