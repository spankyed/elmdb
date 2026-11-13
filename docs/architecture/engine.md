# The engine

The schema, the operations and the protocol. Built: `crates/elmdb/src/engine/`.

## Opening

```ts
const db = Database.open(path, { schema?, readOnly?, mapSizeBytes?, syncIdleMs?, syncIntervalMs?, syncs? })
```

`syncs: false` skips group commit's syncs, for tests on a temporary directory, where a crash of the machine can't
matter; `durable()` then resolves at once.

The map starts at twice the data file, 64 MiB at least (`mapSizeBytes` sets it). A unit that fills it is aborted, the
map doubled while the process has no transaction open, and the unit run again, up to 16 times: a unit is data, so the
engine can run it again without the caller. A growth also keeps out whatever else reads through the map: a sync and
each read of the environment's sizes wait for it as a transaction does. A map another process grew is adopted by the
first read to meet it, `committedElsewhere` included. Mapping capacity and physical file length are separate. On Windows,
a read-only view covers the current physical file: its configured bound is capped to that length at open and when
adopting external growth, so newly appended pages trigger remapping before access. Read-only handles cannot grow
the writable capacity. Writable mappings retain the normal capacity policy.

An open refuses files that aren't an elmdb database (`NOT_A_DATABASE`, which names lmdb-js for files lmdb-js
wrote) and a database in another format (`WRONG_FORMAT`), and a new database is stamped with its format, and
synced, before anything else is written to it.

A database is one directory. A program that needs data with its own life cycle opens a second one: apack opens `main`
and `runs` (its flow run history), which reset and back up independently.

A read transaction refused with `READERS_FULL` asks LMDB to clear stale reader slots, then retries
acquisition once. LMDB checks process liveness; live readers are never evicted. A second exhaustion
or any unrelated acquisition error remains a refusal. This applies to opening/validating a database
and to ordinary reads and replacement-source snapshots; it does not change snapshot lifetimes or the
configured reader capacity. Replacement sources require the same writable lock-file registration as a
normal read-only open, even when their data files are read-only. On Windows, a terminated process can
remain visible to the conservative LMDB liveness check until its OS process object disappears; such
slots remain protected rather than being evicted on an unproven assumption.

Several processes may open one database: one writes at a time, LMDB's lock serializing them, and a read-only open
reads while the app writes (apack's `apack db`). Read-only access still requires writable reader registration in
LMDB's lock file. A permission failure or LMDB's implicit unregistered EROFS fallback is refused. After opening,
elmdb starts a reader and verifies that a lock-table reader slot exists before accessing database contents.

The Node binding acquires an exclusive OS process claim before its first database open. One binary owns the process
claim until process exit, including after its databases close or its first worker ends. Another physical binding copy
is refused with `ALREADY_LOADED`; a process-claim I/O failure is refused with `IO`. Independent processes have independent
claims. Linux binds an abstract Unix datagram socket whose name hashes the ELF loader's per-exec randomness and PID.
Every binding copy sees the same identity without account lookup, home storage or /proc; independently executed
containers with equal PIDs have independent identities even when sharing a network namespace. The hash avoids
publishing the loader randomness itself. The socket is never used for messaging and its handle remains open until
process exit. Network-namespace changes after loading are outside this contract, as is using an inherited binding
after fork without exec.

macOS atomically reserves receive-right name `0x000e1d03` in the process's own Mach IPC table. This requires neither
launchd registration nor filesystem state. Occupation of that reserved name, including by an unrelated component,
refuses before LMDB opens; it never chooses another name. The error retains `ALREADY_LOADED` but explains that
either another binding or an unrelated component may own the name; macOS does not identify its owner. Windows uses
a session-local PID-named kernel event with a non-inheritable handle retained until process exit. The kernel reclaims these claims on ordinary exit and hard
termination. No coordination files are created, and temporary-directory cleanup cannot split a live claim.
Failed acquisition is retryable; only success is cached. OS restrictions that deny the required kernel operation
are reported as IO rather than falling back to an unprotected open. The shipped targets are Linux, macOS and Windows;
other targets refuse process-claim acquisition as unsupported.

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
callbacks. Coordinate writes in one process when observers must hear every commit. `committed_elsewhere()` answers
whether another handle has committed since this one last asked, as SQLite's `data_version` does, with no log: every
commit raises the sequence by one and every commit a handle makes, its index builder's included, passes through its
`Store::commit_txn`, so a handle that keeps the sequence it has accounted for knows any commit past it is another's.
`hear_commits_elsewhere(listener)` wakes a listener as soon as another process commits (`store/wake.rs`: a mailbox
each listening handle files under `wake/`, a FIFO on Unix as Realm uses and a named event on Windows, which a committer
that finds nobody owning removes), having queued any index work the store says is owed. A wake
carries nothing a reader depends on, so one lost costs delay, and a commit here that finds a commit it hadn't
accounted for wakes its own listener as the lost wake would have.

**Changed since.** Every entity keeps the sequence of the commit that last wrote it, a field, its trash state or a
relation at either end (a relation moved to other ends wrote the ends it left too), and for a type whose schema sets
`changeOrder`, `d/h` keeps its entities in that order (`engine/changed.rs`). The order is built at open from the entities the type holds, and dropped when the schema stops
asking. A commit to a type that keeps one costs about a fifth more (`examples/write-cost.rs`, 2026-11-06, load 6 to 9),
so only a type something reads it from sets it; a type that keeps none pays nothing. `changed_since(types,
from, limit)` reads what changed after a position, however far behind it is, in pages that may end within one commit's
entities (`Position.after`), with the entities destroyed since from the tombstones of the last 4096 commits, each once,
and none that was made again under its id, which is among the changed. A reader older than the kept tombstones, of a
type with a destroy since, is told the answer is incomplete; so is one from before a `reset`, a `replaceWith` or a
`renameType` of the type, which take entities away with no tombstone and bring others with no stamp of their own. A commit stamps what
it wrote as it commits, once whatever number of fields it wrote; a unit reads its commit's sequence (`next_sequence`)
before it commits, so a reader inside it can say where it will have read to.

**Checkpoints.** A reader that applies what changed since a position keeps its position with what it derived:
`Unit::set_checkpoint(name, bytes)` writes it in the unit, so the two commit together or not at all, and
`checkpoint(name)` reads it back, from a unit or the database. It is a reader's place, as CouchDB's local documents and
Kafka's committed offsets are, so it is no entity, stamps no change order, and goes with the rows it describes: a
backup keeps it, a dump leaves it out, and `reset` and `replace_with` drop it, so a reader starts again from nothing.

`reset` and `replace_with` move a stored epoch on. A handle's commit under an epoch it hasn't seen is refused
(`DATA_REPLACED`), since whatever it read before writing has gone, while its reads carry on over what's there now. The
holder asks `replaced_elsewhere` when woken, or when a write is refused, which names the epoch; once it has acted on what's
there, `adopt_replacement(epoch)` takes it as seen, and answers false if another replacement has landed since.

## Reads: plans

A call carries several reads, answered from one snapshot (or, in a unit, from its own transaction) in one msgpack
envelope, an answer per read. A read is a query plan, or a direct read: an entity's row, one of its fields (with
whether it is present), a relation's row (with its `weight`, for a kind declared weighted), `findRelations` by kind
and ends, `wouldCreateCycle` (the walk a link's
`CYCLE` check makes, a trashed entity's edges included), and search's
`suggest`, `findLabels` and `pendingEmbeddings`, and `catalog`: each declared type with its live entities and the fields its entities have
held, each declared kind with its relations, and the declared roles. `catalog` reads one key per name, so a watch over it
runs again only when a count moves or a field is first written; `stats()` is the walk, for what only a walk answers. A search naming an index its type lacks, or none on a type with none or several, is refused
(`UNKNOWN_INDEX`). A `search`, a `hybrid` or a `suggest` over a text index still being built is refused
(`INDEX_BUILDING`): its terms and statistics are the answer, with no rows to walk in their place as a value or a trigram
index's are, so read part way it would answer from the entities filed so far and say nothing of the rest. `waitForIndexBuilds`
resolves once it is built; a tracked read and a watch are told by the commit that ends the build (below, Change sets
and watch).

A plan is a scope, a start, steps and a terminal:

- **Scope:** `live` reaches no entity in the trash, not as a start and not at a far end; `withTrashed` reaches the
  trash too; `trashed` starts from the trashed entities only.
- **Starts:** types (in creation order), ids (in the order given), or every entity. A type the schema doesn't declare is
  refused (`UNKNOWN_TYPE`), as a kind or role a read names is.
- **Filters:** `ofType`, `within`, `where` (a value, or a list holding it; a `null` matches a field that is null or
  missing), `range` (a value within bounds, or a list holding one: `atLeast` or `above`, and `atMost` or `below`, on one
  side or both), `has`, `lacks`, `withRole`, `linkedWith`, `linkedTo`, `linkedFrom`, `unlinkedTo`, `unlinkedFrom`. A
  `range`'s bounds are all numbers or all strings. Number bounds hold numbers and timestamps, a timestamp counting as
  its milliseconds whether it is a bound or a value; string bounds hold strings, in the order of the schema's
  collation. A value of the other kind is outside every range, as a missing or null one is. A step with no bound, with
  two on one side, or with bounds of two kinds is refused (`BATCH_INVALID`).
- **Walks:** `out`, `in` and `both` (each far end once, of the types when given), `descendants` and `ancestors`
  (nearest first, each entity once, so a cycle ends the walk; with `depth`, at most that many edges away), and `pathTo`
  (a shortest path from an entity of the set to `to`, following edges `out`, `in` or `both` ways and at most `depth`
  long when given: its entities in order, both ends included, or none; of paths as short, the one found first).
- **Graph algorithms** (`archive/roadmap/graph-algorithms.md`): over the set so far as a graph, its entities the nodes and the
  edges of the kinds named between two of them the edges. `ranked` orders the set by the scores a rank index holds
  (`schema.md`, Rank indexes), highest first, ties in the set's order: every entity of the set must be one of the
  index's nodes, else `BATCH_INVALID`; an index the schema doesn't declare is `UNKNOWN_INDEX`. One not built yet
  answers from `rank` over the graph it declares, to its tolerance: the same answer, costing what `rank` does until the build ends. `whileBuilding: "refuse"` asks for `INDEX_BUILDING` instead, for a caller that would rather not pay that. A plan that starts from the index's types, in its order, and keeps the first `n` with `limit` reads
  those `n` from the index's score order, and past them only through a cell of it wider than the order promises: the index's
  scores further apart than twice the tolerance come in order, PageRank's further apart than that and the bound, and closer ones in the same cell by place, sorted when
  the answer shows scores. Its scores are hits', and the `hits` answer's `rank` is
  `{ index, error }`: the bound on the scores' L1 distance from PageRank. `rank` orders the set by personalized PageRank, highest
  first, ties in the set's order, following edges `out`, `in` or `both` ways: a walker follows an edge with probability
  `damping` (0.85) and otherwise jumps back to a `seeds` entity, every entity when none are given, which is PageRank.
  Over a kind declared weighted it follows an edge in proportion to its weight among its entity's edges, an edge of a
  kind not weighted weighing 1 and parallel edges adding; an entity whose edges weigh nothing in all sends the walker
  back to the seeds, as one without edges does. `weights: false` ranks as if every edge weighed 1. A seed is an id, of
  weight 1, or `{ id, weight }`, and the walker jumps back to each in proportion to its weight, an id named twice adding
  its weights; a seed weight that isn't above 0 and at most 2^960 is refused (`BATCH_INVALID`). With every weight 1 the
  scores are the unweighted ones, bit for bit.
  Each score is a hit's, so `hits` answers it, and a `limit` after keeps the highest. Rounds stop when the scores move
  by less than `tolerance` (1e-6, their L1 distance) or after `iterations` (100, enough to settle at the default
  damping from any start), and the `hits` answer's `rank` says whether they settled and in how many. A seed outside
  the set is ignored, and none inside answers nothing. A damping outside (0, 1), no iterations or a tolerance not above
  0 is refused (`BATCH_INVALID`). The answer is the same, bit for bit, on every platform: nodes are numbered in the
  set's order and summed in that order, on one thread. `topological` orders the set so each edge of the kinds,
  followed `out` (or `in`, which reverses it), leads from an entity to a later one: `A blocks B` puts A first. The
  members of a cycle stay together in the set's order where the cycle falls, and what the edges leave free keeps the
  set's order, so of every order the edges allow it is the one nearest the set's; `both` is refused (`BATCH_INVALID`),
  an edge followed both ways ordering nothing. `similar` answers the set's entities that share a neighbour with `to`,
  most alike first, ties in the set's order: an entity's neighbours are the far ends of its edges of the kinds,
  followed `out`, `in` or `both` ways, in the set or not and in the scope; `measure` is `jaccard` (the default, shared
  over either's) or `adamicAdar` (each shared neighbour counting 1 / ln of its degree, so a rarely linked one counts
  more). `to` itself and an entity sharing nothing aren't answered, and a `to` the scope doesn't reach answers
  nothing. Each score is a hit's, as a rank's is. It reads from `to` two edges out, not the whole set: straight after a type start an entity's membership is its type and the scope, read per candidate, so it costs what `to`'s neighbourhood holds; after other steps it collects the set. Adamic–Adar's logarithm is libm's, so its scores are the same bits on every platform. Unlike `like`,
  which ranks by embedding, it ranks by links.
- **Shaping:** `orderBy` (booleans, numbers, timestamps, strings by the schema's collation, anything else by its JSON,
  then a missing or null value; ties keep the query's order, and `descending` reverses the values and not the ties),
  `reverse`, `limit`, `distinct`, `after` (the entities after an id, a page's cursor; an id no longer in the answer
  answers nothing, so a caller pages again from the start).
- **Search:** `search` (a text index's matches, most relevant first; `search.md`, Text search as built); `containing`
  and `matching` (a trigram index's substring and pattern matches, in the set's order; `search.md`, Substrings and
  patterns as built), a pattern the syntax doesn't hold refused (`PATTERN_INVALID`); `near`, `like` and `hybrid` (a vector
  index's nearest entities, and text and vector rankings fused; `search.md`, Vector search as built).
- **Terminals:** `ids`, `count`, `first`, `last`, `exists`, `rows` (whole rows, or the fields asked for), `firstRow`,
  `edges` (the relation rows of the set's edges, `out`, `in` or `both` ways, each relation once; with `types`, an edge is answered when its far end seen from a set entity is of one of them, so with `both` an edge between two of the set is answered if either end, seen from the other, is), `degrees` (each
  entity with how many edges of the kinds it has, out, in or both ways, counting those `edges` from it alone would
  answer), `weakComponents` (the set's weak components over the kinds, each edge joining its ends whichever way it
  points: each one's ids in the set's order, largest first, ties by the earlier first member, or with `counts` how
  many), `strongComponents` (the set's strong components over the kinds, the groups whose entities each reach every
  other following the edges' direction, an entity on no cycle a group of its own, in the same order and with the same
  `counts`), `communities` (the set's Leiden communities by modularity over the kinds, each edge joining its ends
  whichever way: groups more densely linked inside than out, each connected, largest first; a higher `resolution`
  (1.0) finds smaller ones and 0 leaves each component whole; Leiden's order is drawn from `seed` (0), so a seed answers
  the same for the same data on every platform; at most `iterations` (100) rounds; an edge of a kind declared weighted
  counts by its weight unless `weights` is false; a negative resolution or no iterations is refused, `BATCH_INVALID`), `groupBy` (ids by value, or with `counts` how many), `least` and `greatest` (`{ kind, id, value }`: the entity whose `field` is least or greatest in the order `orderBy` sorts it in, among those holding a value for it, the first of equal values in the plan's order; `id` and `value` are `null` when none holds one; a null is no value; a set an index reads in the field's order, as an `orderBy` of it is read, is answered from the first entry under the index's least value or under its greatest, and nothing else of the set is read), `sum` (`{ kind, sum, n }`: the numbers the field holds over the set, and how many entities hold one. Each value is read as a 64-bit float, so an integer past 2^53 is not exact; their sum is kept exactly and rounded once, to the nearest float and at a tie to the even one (`engine/sum.rs`), so ten 0.1s sum to 1 and the answer is the same for every order of the set. A value that isn't a number is refused with `VALUE_INVALID`, naming its kind, and so is a sum an addition of which passes what a number holds on the way, even where a later value would bring it back: near the largest number, whether a sum is refused can depend on the order of the set), `hits` (what a search, a rank or a `similar` found in each entity).

A plan pulls ids through its steps one at a time, so `first`, `exists`, `firstRow` and `limit` stop once they have
their answer, and a live type's `count` is read off its counter. The planner makes these choices, none of which changes
an answer:

- **A type start's filters are read as the places their indexes hold in common.** The filters a type start begins
  with (`where`, `withRole`, `has`, `lacks`, `ofType`, `within`, `range`) keep the same set in any order, so the
  stage of the last of them reads for them all (`reads.rs`, `Query::filtered`), each start type by turn: the answer
  and its order, creation order, are those of a walk of the type testing each filter.
  - **A filter with an index is a list of places.** In each type, a `where` of a value other than null, where the
    type has a built index on that one field, and a `withRole`, read the places the index files under that one
    value or role (`format.md`, `d/x`): a list of the type's places in creation order, held in the value's own
    key or, past 256, in blocks of no more than that. An entity the filters keep is at a place
    every such list holds, so the places they hold in common are read. One list is walked, and each place it holds
    is sought in the others: a list that doesn't hold it answers with the next place it does hold, which no place
    before can be in them all, and the walk goes on to that. A place carries no id: the entity at a place the
    lists share is read from its type's rows there, which is the one read that counts among a plan's visits, and
    the filters with no list are tested against it. A type with no list has its rows walked and every filter
    tested. A `lacks` of a field with a built index on it alone reads the entities the index files as without a
    value (`format.md`, `d/xw`), a list too, and is still tested against each: the index files there an entity
    whose field is null as one that holds none. Equalities on every field of an index on several fields read its places under those values as one list in
    place of their own, the index on the most fields, while it holds no entity it doesn't order under the values
    of the fields before its last: `where owner = 9` and `where rank = 3` read an index on `[owner, rank]`.
  - **What that reads.** Nothing is counted to choose among lists. What is read is bounded by the shortest list
    and not the longest: for each place the shortest holds, a place sought in each other list, and in the one
    walked up to eight passed over and one sought. A `first`, `exists` or `limit` asks for a place or two of each
    list for each place tried, which is each entity it takes where the lists mostly agree. A key is read only
    for a place the one last read doesn't answer for: a value's own key once, and then a block of its places at
    a time, so places close together cost one key between them. The list walked is the
    one whose places have been furthest apart, one from the next, where the read has seen them: where each list's
    first place is says so to begin with, and a list whose places come to be twice as far apart is walked in its
    place. A place walked is the next one held, where a place sought is searched for, in the block held or
    from the root in another, so walking the shortest asks the fewest. Which list is walked follows from what the read has seen, which the order the
    filters are written in decides to begin with: that order can change what is read, within the bound.
  - **A range's index is read once the read has paid for it.** A `range` of numbers has an index to read where
    the type has one on its field alone that accounts for every entity, or, over one type, where the plan's first
    steps are equalities and the range and an index is on their fields and then its own, whichever way round they
    were asked: that one stands for all those steps, the one on the most of them. Its places are under its
    values in the numbers' order, so they are no list until all are read and sorted by place, which reads no
    entity. They are read once that costs no more than the read has read already, in places walked and sought,
    which is asked each time it has read twice what it had, from 16 places on, the range's places counted a
    key at a time, a value's own or a block of them, and so no further than that by more than a block; and at the start, a range holding fewer than 16 is read,
    the one holding the fewest of several. Until then the range is tested against each entity. So a read that
    stops at its first match reads no range holding more than it had read, and the whole costs a small multiple
    of the cheaper way. A read that takes every entity the filters keep, and has no list, reads a range whatever
    it holds, the one holding the fewest, each counted to twice as far until one is known to. An index on several
    fields is not read for equalities on some of its fields alone.
  - **A plan of the trash** reads the type's trash, its rows keyed by place, as a list among the others, where an
    index's places are of every entity: only entities in the trash are read. It reads a range's index only once
    it has read as many places, never at the start.
  - **A tracked read** depends on each key it read: a value's own key, each block of its places the read came
    to, in the list it walked up to where it stopped and in the others where places were sought, the row at
    each place the lists shared, and the fields it tested. A block holds up to 256 places, so a write that files
    or unfiles another entity in a block the read came to touches the read, though it asked nothing of that
    place: a tracked read is rerun more often than its answer changes, and never less. What was counted decides
    only what is read: the keys are no entity's and not among a plan's visits, and a tracked read records none
    of them.

  A filter no stage would take, a `withRole` of an undeclared role or a `range` with bounds it can't take, is
  refused before anything is read, the last written first, as when every stage runs. A page's cursor straight
  after the filters is sought: each list is read from the cursor's place, and nothing is read for a cursor the
  filters don't keep. Where no index reads any of the filters in any start type, the stage of the first of them
  walks the types for it, and each stage after tests its own filter against what the one before hands on. A
  filter after any other step filters what reaches it.
- **An index lookup visits its matches as it reads them.** One value's places are held in order, so they come in
  creation order, each entity read at its place, and a `limit`, `first` or `exists` after the lookup reads no
  entity past its answer and no block of places past the one its answer is in.
- **A walk visits an entity's edges as it reads them**, in place order, so a `limit` after `out` or `in` reads no
  further than its answer. A `reverse` straight after `out` or `in` from one entity over one kind reads that entity's
  edges from the last instead of collecting the walk, an end joined by several edges of a keyed kind
  coming at its first edge's place as in the walk, so the last `limit` edges cost the limit and not the list. From
  several entities or kinds, the `reverse` collects the walk.
- **An order is read from an index.** An `orderBy` straight after the start of one type, live or with the trash,
  reads the type's entities in the field's order from a built value index on that one field, when every value the
  index holds is a boolean or a number: false, true and the numbers as its entries have them, each value's entities
  in place order, then the entities without a value; `descending`, those first and the values from the greatest. Such
  an index accounts for every entity of its type (`schema.md`, Indexes), which is what lets the walk leave none out.
  Each entity is handed on as its entry is read, so a `limit`, `first` or `exists` after it reads no further than its
  answer: the first ten of a million read ten. A field holding a string, whose order is the collation's and not its
  entry's bytes', or a list or another value the index doesn't order, is sorted as before, as are several types, a
  start of ids, and the trash alone.
- **A least and a greatest are an order's ends.** `least` and `greatest` of a field over a set an order of that
  field would be read from an index for (the points on either side of this one) answer the first entry under the index's least
  value or under its greatest, the first in place order of those holding it, passing over an entry of an entity the
  scope doesn't reach: one entry read, where the set would be read whole. A set no index orders by the field is read
  whole.
- **An order under equalities is read from the index on both.** When nothing but `where`s of values stand between the
  start and the `orderBy`, each of its own field, with or without a `range` of the ordered field, the order is read
  from a built index whose last field is the ordered one and whose fields before it are the equalities', whichever
  way round they were asked: its entries for those values, in the last field's order. `where status = open`, then
  `orderBy n`, then `limit 10` reads ten entries of an index on `[status, n]`, and a page after a cursor is sought
  in it as in any order read from an index. A list in an equality's field is under each of its elements
  (`schema.md`, Indexes), so `where tags = x`, then `orderBy updatedAt`, reads an index on `[tags, updatedAt]`.
  The index is read while, under those values, it holds no entity it doesn't order and no entry of a string;
  otherwise the equalities are filtered and their matches sorted. What an entity under other values holds leaves
  the read alone: one task with a list for its number has its own status sorted, and no other.
- **With no such index, the order is walked while that is the cheaper way.** An `orderBy` straight before a `limit`,
  after the filters a type start begins with, has two ways to its first entities: read the filters and sort what
  they keep, or walk the order from an index on its field alone and test each entity against the filters, which
  answer the same. Which costs less is the entities', not the plan's: filters that keep half the type are met within
  a few entities of the walk, and filters that keep a thousandth cost the sort a thousandth of the type. So the walk
  is tried when reading the filters would read 128 entities or more, the entries of the index they are read from or
  the type's entities with none, and given up once the walk has read half as many entries as that reading would,
  which it asks at 128 entries and each time it has read twice as many, once it has tested the entry it asks at:
  every entry it reads counts, one of an entity in the trash as one it hands on, and a plan of both counts the
  trash's entities among what the sort would read. What it asks is counted in keys, each once over all its
  askings, and decides only how the order is read: the keys are no entity's and not among a plan's visits, and a
  tracked read depends on none of them. The two together cost a small multiple of the cheaper, what the filters
  would read being taken as what the shortest of their lists holds (`docs/evidence/million-notes.md`, A filter
  before an order). A page of it, an `after` between the `orderBy` and
  the `limit`, walks from past its cursor, sought in the order's index, under the same rule: it costs the entities
  between the cursor and the page's last. One given up, like any page of a sorted order, is sorted as a first page
  is, holding only what the `limit` takes: the entities ordered past the cursor, those of its own value that come
  after it among them. A cursor the filters don't keep has no page, and nothing is read to say so. Only an index on
  both reads a page at the cost of what it holds.
- **A range is read from an index.** A `range` of numbers among the filters a type start begins with reads the
  entries between its bounds from an index on its field, when the index holds no list or other value it doesn't
  order, and hands them on in creation order, as the first point says: at once when they are a handful, or when
  the read takes every entity and has no list to read in their place, and otherwise once it has read as many
  entries as they are. An `orderBy` of the same field straight after it reads those entries in order instead, as
  above, and a `count` of it counts them by their places, less the trash's as a search leaves them out, and reads
  no entity. Among equalities alone, a range is read from the index on their fields and then its own, as an order
  is: the range may stand before them, between them or after. A range of strings, one among any other steps, and
  one over a type without such an index filter what reaches them.
- **A page's cursor is sought.** The stage before an `after` starts past its cursor when it reads an order it can
  seek in: a type start (the cursor's place in its type's creation order, the types before its own
  left out), the filters a type start begins with, read together (each list from the cursor's place, or the walk of a
  type with no list begun there), and an
  `orderBy` read from an index (the cursor's entry, or its place among the entities without a value). The page then
  costs what it holds wherever the cursor is. A cursor that isn't in the set (another type's entity, one the scope
  doesn't reach, one a filter leaves out, one without the value) answers nothing and reads nothing. A `search` has
  no order to seek in, so it ranks the cursor among its matches and hands on those ranked past it (`search.md`,
  Text search as built): its page reads the entities it holds, though every match is scored. After any other stage,
  `after` passes over the ids up to the cursor as they come.
- **A search after a type start's filters runs within their places.** When the steps before a `search` are the
  filters a type start begins with, and a list reads every one of them in every start type, a range's among them
  read whole, the places those lists hold in common are the set the search runs within, less the trash's for a
  live plan and only the trash's for a plan of it. No entity is read to make the set, the search's ties are
  broken by the types' order and then creation's, as the set's own order would break them, and a type the filters
  keep nothing of is not searched. A filter with no list has the filters read as any are, each entity they keep
  read, and the search runs among those.
- **A search is asked for no more than its answer takes.** A `limit` straight after a `search`, or after the `after`
  straight after it, asks it for its best that many, which it keeps as it scores and no others; `first` and
  `firstRow` ask for the best one, and `exists` for any one, which straight after the search it answers from the
  indexes with no match scored; past an `after` it is the best one of the page. With any other step between, the search ranks every match and the step after stops the walk.
- **`orderBy` then `limit` holds the first `limit`** where the order is sorted, in order with ties kept as they came,
  rather than sorting the whole set: the set is still read, and the memory is the limit's.

A query asked for its `visits` answers how many entities its start and walks read from the store, one found
through an index counted as it is read at its place, which is what says whether a filter walked rows an index could
have answered. A range's places, read to make a list of them, are no visits until an entity is read at one; a
`count` of a range reads no entity and counts each place it counted as a visit.

**EXPLAIN.** A query asked to `explain` answers `{ kind: "explained", answer, visited, ms, stages }`: the answer it would
have given, the visits, the milliseconds the whole read took, and a stage per part of the plan — its start, each step and
its terminal, in order — as `{ stage, index?, visited, kept, ms }`, and a stage that built a graph (`rank`,
`topological`, `weakComponents`, `strongComponents`, `communities`) with its `nodes` and `edges`, the `weight` of its
edges added up when it read weights, a rank's `iterations` and whether it `converged`, a `ranked` stage's rank index as
its `index`, the bound on its scores as its `error` and, while the index isn't built and the stage computed them, `building: true`, and a topological order's `cycles`; a `similar` stage with the `neighbours` of `to`, the `candidates` it scored and the `edges` it read. `stage` names it (`types`, `where`, `orderBy`, `rows`,
…); `index` is the value index the stage read, each one named where it read several as lists, or `none: walks the
type` for filters no built index covers; `visited`
is what the stage before handed it plus what it read from the store itself; `kept` what it handed on (a terminal's is
what it answers); and `ms` the time spent in the stage's own code, not in the stages it pulled from or handed to, so
the stages' times add up to at most the whole. A filter before the stage that reads a type start's filters together
visits and keeps nothing of its own: that stage does its filtering. An `orderBy` stage that sorted, or walked its
order testing the filters, for want of an index that reads it in order says which would have: `wants` names that
index's fields, the equalities' and then its own, where the field is declared a number or a boolean or is a time the
engine writes, whose order an index's entries have, and each equality is of a field the schema lets an index name.
So does a `range` the `orderBy` after it reads between. A `search` stage says how many of its matches it `held` to
rank them: every one, or those ranked past a page's cursor, and of either no more than it was asked for; none when it
was asked for any one. A search that held every match before a `limit` further on is one a filter stands between.
A stage that read lists of places, or walked a type's rows, says how many `places` it was handed, each one walked
or sought; `set: "and"` when it read the places several lists hold in common; and a `search` that ran within its
filters' places names their indexes as its `index` and says how many places it ran `within`.
EXPLAIN times each crossing between stages, so its `ms` is the explained read's, a little above the plain one's.

A read takes an idle read transaction while it is still the last commit's snapshot (its id the environment's last
committed one, which a commit in any process moves), and opens one otherwise. At most eight completed
transactions are cached, each for 100 ms after its read completes. A worker releases expired transactions even if
the process makes no further database calls; expiration depends on the operating system scheduling that worker.
A commit here clears the cache, and an older read finishing after that clear cannot return its snapshot to it.
Active reads and open units keep their snapshots until they finish. The worker owns only the cache, cannot keep
the database itself alive, and is joined on normal close; process-exit cleanup detaches it with bounded locking.
The policy and its measured tradeoff are recorded in [idle snapshot evidence](../evidence/idle-snapshot-retention.md). `db.readSync(reads)` runs on the calling thread. `db.read(reads)` runs on a thread of the pool, in a snapshot of its
own, and resolves with the same answer, so a large read doesn't block the event loop.

A caller's own function can't cross: a builder holding one sends the plan up to that step, and runs the rest itself.

**Answers** are owned bytes: ids as strings, each row as a map of `id`, `entityType` and the fields' stored msgpack
bytes spliced in unchanged, counts as numbers. An answer holding rows says whether any map in it may hold a key
`__proto__` (`plainObjects`), so the caller can decode straight to objects when none does and keep the exact decode for
the rest; decoding is most of a long read's cost (`docs/evidence/phase-1.md`).

## Writes: batches and units

`db.write(batch, durable?)` applies a batch as one unit, on a thread of the pool, answering a promise; `db.writeSync` is
the same on the calling thread. Writes in flight together are applied one at a time, in no fixed order, so a caller
awaits a write before one that depends on it. A database's check, rebuild and backup likewise run on the pool, and their
`…Sync` forms on the calling thread. A refusal made before any work begins (a rebuild naming only a type, a handle that
has panicked) throws at the call in every form; a call on a closed database throws in the blocking forms and rejects the
promise in the others. A close waits for the operations running, which for a large rebuild or backup is the whole of it,
so the event loop is held by a close made during one. An operation still queued when the close takes the database
rejects with `CLOSED` and writes nothing, and a write whose commit has landed is answered as made, not `CLOSED`. A
queued job waits on a thread of the pool, which Node shares with its file system work, so long jobs or several queued
writes can delay that work; a caller with a batch that matters to other work uses the blocking form. A unit can also
stay open across calls, as a caller's transaction running its own code between writes needs: `db.begin()` opens one,
`unit.write(batch)` applies a batch in it, its writes read their own, and `unit.commit(durable?)` or `unit.abort()` ends
it. One unit is open at a time, and while it is a write of the database refuses (`UNIT_OPEN`): its writes go through the
unit. The unit's write transaction belongs to the thread that began it. A unit opening on a map more than half used
grows the map first, since an open unit can't grow it. A background index build waits for an open unit to end, woken
when it does, and goes on from where it stopped; so does any other write but the unit's.

A batch is applied in a savepoint of its unit, so a refused operation keeps none of its batch, and the unit goes on.

The operations:

- **Operations:** create (a whole row; the engine assigns the id unless an import brings one, the code, `createdAt`
  and `updatedAt`), upsert, set, set many, append to a list, merge into an object, delete a field, remove from a list,
  trash, restore, grant and revoke a role, grant exclusively among entities, link (with info and, for a kind declared
  weighted, a weight; merging on the edge's identity, and refusing a cycle in a kind), unlink, unlink all, update a relation (its ends, its
  info, its weight) and remove one, destroy, take a label (the next `<base> <n>` of a type), and embed (a caller's vector
  for a chunk, answered `stale` if its text changed).
- **Savepoints:** `unit.savepoint()` opens one and answers its depth; `unit.rollbackTo(depth)` undoes everything since
  it, and `unit.release(depth)` keeps its writes in the unit around it, as apack's nested `transact` does. A savepoint
  is an undo log of each key's previous value, so rolling one back costs what it wrote. A write that has just read a
  key hands the log what it read, so the log reads it again only where the write didn't: a field write reads what the
  field held once, and the entity's place once for all the indexes covering it (`engine/tests/field_writes.rs` counts
  them; `docs/evidence/next-phase-7.md`).
- **Refusals** are the engine's, each an error code: an undeclared type, kind or role, a missing required field, an id
  in use, an id of another type, an id longer than 128 bytes or a key the store can't hold however it is made
  (`VALUE_INVALID`, naming what the key is made of), a write to an entity of a type the schema no longer declares
  (`UNKNOWN_TYPE`), no such entity or relation, a reserved field, a value that isn't a list, an invalid
  or taken code (a code's number is at most 2^53 - 1, JavaScript's largest exact integer, and a numbering that would
  pass it is refused, `NUMBERS_EXHAUSTED`), a value its field's type doesn't take (`FIELD_TYPE`), a field its type doesn't declare
  (`UNKNOWN_FIELD`), a weight that isn't a number from 0 to 2^960 or on a kind not declared weighted
  (`VALUE_INVALID`), and a link its kind's rules don't allow (`SELF_LINK`, `ENDPOINT`, `CARDINALITY`, `CYCLE`; a cycle
  counts every edge, a trashed entity's included, since restoring it would close the cycle), or an edge
  moved, or its key changed, onto another edge's identity: its ends and key values (`DUPLICATE_EDGE`). A relation's ends are checked at commit, so a unit may link
  to an entity it creates later. A relation moved by `updateRelation` is held to its kind's rules as an edge at its new
  ends, its edge as it was counting for nothing: one turned round closes no cycle through itself. A write of several
  fields of one unique index is held to the values it leaves, not to those the entity passes through field by field.
- **The result** carries each operation's answer (an entity's id, code and whether it was created; a relation's id and
  whether it was linked; a label; `stale`), and the commit's change set.

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
The failed call does not return its touched-token list: callers managing tracked tokens must conservatively invalidate
all of them on `NOT_DURABLE`. Native watches have already been finalized. apack follows that invalidation rule.
`close()` syncs what is owed, and reports a sync that failed.

`syncEachCommit` (`OpenOptions.sync_each_commit`) is the power-loss-safe mode: LMDB syncs each commit as it is made,
its pages before the meta page that names them, so a commit that returned survives the machine losing power, and no
group commit runs. On Apple a full sync (`F_FULLFSYNC`, which LMDB makes of the data file) follows each commit as well, since
LMDB writes the meta page with `O_DSYNC`, which doesn't flush the drive's cache there. A write's `durable` and `durable()`
then answer at once, since each commit was synced. The price is a
sync for every commit: on an Apple M1 Pro at a load of 22 to 25 (2026-11-12, 300 commits of one note, median of three
rounds, after the Apple full sync was added), a commit took 0.03 ms in the grouped default, where the timer merges its syncs;
4.0 ms for a durable write in the grouped mode, which syncs once; and 8.0 ms with `syncEachCommit`, which syncs twice. The default is the grouped mode, where a commit that returned
may be lost to a power loss until its sync, a moment later. More than the last commits can go with it: nothing orders
the pages and meta pages written between two syncs, so a loss can leave a meta page on disk without all the pages it
names, and LMDB opens a file by the newer of its two meta pages, whatever it names. The file is then no commit's, a
`durable` commit's neither: its sync covered the file as it was, not what later commits wrote over. Such a file is
refused at open (`CORRUPTED`) where its trees reach past its end (`store/reach.rs`). Torn within its length it may
read wrongly or end the process that reads it: LMDB reads a page as it finds it, and where it is built with its
assertions on, as a debug build is, aborts on one that is no page of the tree it is in. The remedy is a backup. A test
replays the loss in both modes (`testing.md`, Power loss).

After the atomic reset, a small optional LMDB checkpoint rewrites the sequence value read within its own transaction,
without changing logical sequence, schema, data or notifications. Advancing the physical generation lets a subsequent
bulk load reuse the reset's freed pages: LMDB protects the immediately preceding generation even with no readers.
The checkpoint runs before reset's required sync. If it aborts, reset still syncs its committed mutation; only the
space/performance benefit is lost. Active read snapshots retain LMDB's normal protection. Measured rationale and
failure/page-reuse regressions are recorded in [remediation evidence](../evidence/elmdb-remediation.md).

## Change sets and watch

Every commit produces a change set: the entities created, changed, trashed, restored and destroyed, by type, with the
fields and relation kinds touched, each relation with its ends as they are and, for one the commit moved, the ends it
had (`movedFrom`), and the commit's **sequence**, a number that only increases for as long as the
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
- `Database::hear_builder_commits(listener)` (`hearBuilderCommits` in the binding) tells one listener of each commit the
  index builder makes that wrote into a tracked read, with its sequence and those reads' tokens: a write's result names
  its own tokens, and a builder's commit has no result to read. Without it, a reader tracking a rank index holds what it
  read until something else writes, and one refused while a text index was built (`INDEX_BUILDING`) isn't told the
  build ended: it read the index's marker to be refused, which the build's last chunk writes. A second listener is refused (`ALREADY_LISTENING`). The binding's keeps no process
  alive, and closing the database drops it; a close that leaves the database open, refused for an open unit or given
  up on, leaves it listening.
- `Database::watch(reads, listener)` is a convenience over tracked reads, in Rust: it runs the reads tracked under a
  token of its own, and runs them again after each commit that names it, handing the listener the new answer when it
  differs from the last; after a commit whose writes aren't logged, every watch runs again. The listener is called
  after the commit, outside its transaction, on the thread that committed, which for a commit of the index builder's
  is the builder's own. A schema change and a close on another thread wait for that thread to end, and wait holding nothing its
  delivery takes, the schema's lock least of all: a watch's read, and whatever its listener calls of the database,
  goes on while they wait, and the listener's calls after a close are answered `CLOSED`. An unwinding listener panic removes that
  watch and its tracked reads; the panic hook still runs. Other watches continue, and a committed operation retains
  its ordinary commit/sync outcome. An initial listener panic returns `CALLBACK_PANICKED` without leaving a watch.
  A panic in the watch's read also removes its registration, preserves a committed mutation's sync outcome and does
  not strand other watches. An initial read panic returns `WATCH_PANICKED`, distinct from a listener panic.
  The callback guard also covers destruction of listener captures, including their last database reference.
  `Database::unwatch` ends a watch: once it returns, the listener isn't called again, and it answers what it left.
  `Ended`: the listener has returned and been dropped, a delivery running it on another thread waited for, so whatever
  it uses may be released. Called from inside a listener of the same database, a watch's or the builder's commits',
  it doesn't wait, so a listener
  may end its own watch or another's: it answers `Ending` while a delivery still holds the ended watch's listener,
  which is dropped when that delivery lets go of it, and `Ended` when none does. Its caller mustn't hold anything the
  listener needs.
  This recovery requires unwinding; aborting panics terminate the process. Closing this database from its callback
  returns `CALLBACK_ACTIVE` immediately, including timed close; close it after the callback returns instead.
  The listener of the builder's commits is such a callback, run on the builder's thread, and a close it asks for
  is refused the same way: a close waits for the store that thread holds. So is a wait for the index builds asked
  for on the thread of the builder it would wait for, which would wait for the listener asking; a listener that
  changed the schema waits for the builder started for it. The listener is let go of inside the callback too, so
  the last of a database it held closes nothing that waits for the builder's thread: the environment is let go of
  as that thread ends. One that panics is heard no more, and the builder goes on. A schema change made by a listener on that thread,
  a watch's or the builder's commits', stops the builder without waiting for it, there being no other thread to
  wait: the builder ends once it has delivered that commit, and the one started for the new schema builds what it
  left. A close after such a change, by the same listener, is refused like any it asks for.

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
  the old backup. Unix backup data files are private to the creating user (mode 0600); arrange ownership/permissions
  explicitly before restoring under another account. Unix syncs affected directories; Windows clears the temporary
  file attribute before requesting a write-through replacement. If directory sync
  fails after publication, `IO` reports that the complete new backup is visible and retrying is safe. Filesystem and
  device durability still depend on the platform honoring its sync operations.
- `replaceWith(dir)`: every entry replaced by another elmdb database's primary data, with derived data rebuilt under
  this database's schema, in one durable unit, the database staying open.
- `reset()`: every entry removed but the database's own (its format and schema), in one durable unit.
- `stats()`: entities per type (live and trashed), each field's entities and bytes, relations per kind, entries per
  index, the database's entries, file and map sizes; `indexStatus()` says which value and rank indexes are built, the
  built-in `shortCode` and `role` among them; it does not report text, trigram or vector indexes.
- `check()`: derives every derived key from primary data and compares it with what is stored (a built index's entries
  included, an unbuilt one's left out), and reports each difference as missing, extra or differing; and reports each
  row that breaks a rule of the schema: a missing required field, a value its field doesn't take, a field its type
  doesn't declare, a `ref` naming no entity (including nested lists), duplicate unique values, and relations with
  missing ends, self-links, disallowed endpoint types, invalid/missing info, two edges with one identity, excessive
  cardinality or cycles in acyclic kinds. Constraint auditing reads primary values/relations even when an index is
  damaged or unbuilt; rebuilding cannot hide these violations. Unique values include values an index cannot store,
  and use the write contract's numeric, map, single-field list and compound equality rules. heed exposes no walk of
  LMDB's pages, so damage to the B+tree itself is what LMDB reports as `CORRUPTED` when a read reaches it.
- `rebuild(type?, index?)`: deletes derived data, every index's or one type's index's, and writes what primary data derives, in one
  durable unit. One index is refused when the schema doesn't declare it (`UNKNOWN_INDEX`) or a build of it couldn't
  finish (`UNIQUE` over rows holding the same values, `VALUE_INVALID` for an entry past LMDB's key); a rebuild of
  everything leaves such an index unbuilt, as a build does. A rebuild of everything first grows the map to three
  times what the database has in use: the pages it frees are no room for it until it commits, and in a map with
  less it would run until the map is full, and then again in a larger one.
- Rank indexes (`archive/roadmap/rank-index.md`): every write that changes a rank index's graph — a link, an unlink, a weight
  changed, an entity created, trashed, restored or destroyed — updates the index in its unit by forward push: each
  node holds an estimate and a residual, a change moves the residuals its edges touch, and pushing a node moves its
  residual into its estimate and on along its arcs. A commit that changed the index pushes up to 16 nodes, and one that left it alone
  writes nothing to it; the rest wait in the index's queue for the index thread, which on a read snapshot reads its nodes and their edges and pushes in memory until none
  waits, then writes back each node it moved under the write lock, in one unit that tells the watches it wrote into; any
  write of the index meanwhile has it wait for commits to pause and compute again on a snapshot, up to twice, and then in
  that unit. The edges come through an edge mirror on the
  store that the commits keep, each one handing it the entries it filed and unfiled; a commit that can't, or another
  process's, empties it, and `check()` compares it with the store. A rank index is built by that thread too: its nodes added
  1,000 at a time, then pushed. The scores are within `2R / ((1 − α) s)` of PageRank in L1, `R` the residuals' sum and
  `s` the estimates', which a read reports; once none waits, that is within the index's tolerance. Closing stops the thread between units,
  and the next open goes on.
- Index builds: an index is read once built (its `d/xb` marker). One added over existing data is built by a thread of
  the database's in chunks of 1,000 entities, each a unit of its own, so writes go on between them, and committed as a
  write is, so the chunk that writes the marker tells the tracked reads and the watches that read it; writes keep every
  index all along, so a build fills in only what predates it. A unique index over rows holding the same values is left
  unbuilt, and `indexStatus()` names them. Each chunk seeks after its last entity sequence in both live/trash
  ranges and sorts only their bounded candidates: at most 2 × (chunk size + 1) rows in temporary storage. Cursor
  visits grow linearly at fixed chunk size; sorting costs O(chunk size × log(chunk size)) per chunk. Closing stops
  a build between chunks, and the next open finishes it.
- `purge(what)`: deletes what a removed declaration left: a type's entities, a field of a type, a relation kind's
  relations, a role. It refuses a name the schema still declares. It refuses one of the engine's own fields too
  (`RESERVED_FIELD`): no schema declares `trashedAt` or `shortCode`, and neither is what a removed declaration left.
- `renameType`, `renameField`, `renameRelation`: move stored data to a name the schema declares, in one durable unit. A rename that would make two edges of one kind join the same ends and key is refused (`DUPLICATE_EDGE`), as an update onto another edge's identity is, and nothing moves.
  `renameRelation` holds each edge it moves to the new kind's rules as a link of that kind is held: its info
  (`FIELD_TYPE`, `REQUIRED_FIELD`), no entity joined to itself (`SELF_LINK`), the types it joins (`ENDPOINT`), how
  many far ends an end may have (`CARDINALITY`) and, in a kind declared acyclic, no cycle (`CYCLE`), each against
  the edges the kind holds and those moved before it. A rename the kind refuses moves nothing, and names the
  relation refused. A weight goes with its edge, to a kind that reads none as well, where it stays in the row as
  one does whose kind stops being weighted (`schema.md`, Changing a schema); into a kind declared weighted, a
  weight outside a link's range, which only a load brings, is refused (`VALUE_INVALID`). An edge a looser rule let
  in before the kind was tightened, which `check()` reports where it is, is refused the same way on its way into a
  kind with the rule.
  `renameField` refuses the engine's own fields (`RESERVED_FIELD`), and refuses, with nothing moved, while an entity
  holds a value under both names (`SCHEMA_CHANGE_REFUSED`): the move would replace the one the new name holds.
  Renaming a type rewrites its entities' ids, the relations joining them and the declared `ref` fields naming them, its own included, and moves its embeddings.
- `bulkLoad(loads)`: entities, relations under their own ids, sequences, embeddings and fields taken away, applied in
  one durable unit without a write's checks (no schema, no refusals, a unique index taking duplicates), for imports, a
  dump's load, and specs that need data the checks would refuse. The value spec is no such check: a value outside it,
  a weight or a vector's component that isn't finite among them, is refused (`VALUE_INVALID`) and the unit keeps
  nothing, since every reader of the store takes a stored value to be inside it. The derived keys are kept as a write keeps them, and
  `check()` reports what the rules refuse.
- `primary_loads` (Rust), `elmdb dump` and `elmdb load`: a database's primary data as the loads that make it again, in
  one moment and the order it was made, written to a file and read into a new database (`format.md`, Dumps).

### Recovery resources

Derived-entry sorting, single-index rebuild staging and dump identifier ordering use owned scratch files. Each
sorter spills at 8 MiB of payload capacities and per-row metadata, plus row-buffer spare capacity and the largest row.
Text rebuilding can have two sorters active: memberships and final entries. The threshold is per sorter, not an
operation-wide allowance. A merge keeps one head from each of at most 16 runs plus pending/current rows and I/O buffers.
Equal-sized runs are compacted as they arrive, keeping at most 15 paths per level rather than a path for every spill.
Sorting D emitted rows costs O(D log D) comparisons and temporary disk proportional to their encoded bytes.

A single-index rebuild scans primary records rather than trusting derived type ranges. Value entries spill directly;
unique entries sort by value and holder so conflicts are checked with one adjacent group, without a map of all values.
Text document memberships spill and merge into one term's keys at a time, a block of its places and one place more
held at once, rather than retaining every term's places. Vector identifiers spill; graph updates drain journal batches of at most 1,000 rows, decode one added vector
at a time, and build each affected graph once after its updates. Rebuild deletes batches of at most 1,000 keys without
retaining their values. Every replacement remains one atomic transaction, including failures after partial writes.

These bounds concern intermediate collections. One decoded entity and its index expansion, a block of a term's places,
hannoy's graph builder, and LMDB's dirty/resident pages have separate workload-dependent costs. A list-valued field
can emit an entry per indexable element, and in a compound value index one per tuple of the elements its fields
before the last hold. Full check/rebuild also retain
entity/graph inventories, primary constraint/uniqueness auditing and output reports; the complete operation does not
have an 8 MiB or constant-RSS bound. What it adds per entity is bounded instead: the heap test holds a full rebuild and
a full check to 1 KiB of heap an entity beyond the sorters, and a selected vector rebuild to 768 bytes a vector
(`testing.md`, Heap). Replacement streams primary ranges from one source snapshot. The historical
measurements below concern full rebuilds; later single-index/export observations have their own [dated evidence](../evidence/maintenance-memory-2026-10-27.md).

The recorded operating envelope is the explicit synthetic shapes in the
[remediation evidence](../evidence/elmdb-remediation.md), measured on 2026-10-26 on a 32 GiB macOS host with local
storage, fresh processes and uncontrolled filesystem cache. It is not a row-count guarantee for arbitrary schemas
or a latency SLA. The reference shape has 100,000 entities with 1,024-byte bodies, 199,997 relations, a unique slug
value index, a nonunique state value index, and one title/body text index. Earlier full rebuild/empty-destination replacement measured 14.04/22.73 s
and 879,200/992,400 KiB peak RSS, meeting the preselected 2 GiB process / 30 s writer-occupancy upper-bound budget.
At twice that population, measured rebuild/empty-destination replacement used 1,876,128/1,856,864 KiB and 28.12/47.69 s: replacement
exceeds the reference time budget, so schedule a longer maintenance window. Those replacement figures exclude
clearing a populated incumbent and do not establish its time, memory or disk budget. A private staged load of that larger
dump took 79.64 s and 1,932,001,280 bytes RSS. The recorded 2,000-vector (1,536 dimensions) and 2,000-row/64-KiB
value profiles also pass recovery and independent restoration checks; they do not establish arbitrary vector capacity.

For these measured shapes with an empty replacement destination, reserve at least 2 GiB on the scratch filesystem
and the current data-file size plus 2 GiB of free destination space per active recovery operation; add space for a dump if retaining one. These are
planning allowances over those measured macOS artifacts/scratch, not enforced quotas or universal bounds. They
are not validated allowances for replacing a populated destination: its old pages and incoming contents coexist
until commit, so measure its copy-on-write peak separately with `replace-populated`.
Add both allowances when the filesystems share a volume; check them separately when temporary storage is elsewhere.
Measure Windows disk allocation separately; a mapping's reserved capacity does not establish physical file length.
Use `scripts/measure-recovery.mjs` on an owned representative
copy before using materially larger values, more relations, higher-dimensional/more numerous vectors, different
vocabulary, compound/list indexes or an unmeasured schema. Keep one recovery operation active while measuring.
The `replace` runner creates an empty destination; `replace-populated` first prepares an incumbent copy and then
replaces it atomically. The latter's total elapsed/RSS/disk figures include incumbent preparation, so elapsed time
is a conservative writer-occupancy upper bound rather than an isolated transaction timing.
The runner refuses to adopt an existing workload directory and can sample scratch with `--observe`; sampled peaks
are lower bounds. `TMPDIR`/the platform temporary directory controls scratch placement. On a tmpfs/RAM disk,
scratch consumes memory as well as the process's RSS; choose a disk-backed location for large recovery work.
Normal return/error drops remove scratch. SIGKILL, abort or power loss can leave `elmdb-check-*` directories.
Use a private per-operation scratch parent and remove it only after the owning process has exited; no automatic
age-based cleanup or scratch quota is provided.

Dump export spill-sorts entity and relation identifiers by their original u64 sequence, then reads each complete row
from the same snapshot. Relations' full payloads are not retained in the sorter. Vector and sequence ranges use
fallible cursor visits, stopping on the first output error. The Rust `primary_loads` API delivers owned loads to its
callback; callers choosing to collect them own that additional memory. One entity's fields, one relation payload or
one embedding can still be large. Both ordinary failure and unwinding release owned scratch; hard-exit cleanup has
the limitations above. Export retains the existing dump version and byte ordering (`format.md`, Dumps).

Check and backup take read snapshots rather than the LMDB writer, but long snapshots pin pages and can delay map
growth. Rebuild/replacement hold the destination writer for their atomic transaction; their entire measured operation
time is a conservative occupancy upper bound. Dump loading writes only a private staging database until no-clobber
publication. Linux prefers exclusive rename with an atomic hard-link fallback when exclusive rename is unsupported;
macOS uses exclusive rename and Windows uses no-replace MoveFileExW. The requested publication and synchronization
semantics must be available; unsupported operations return an error. The actual coverage and unvalidated configurations
are described in [filesystems.md](filesystems.md).
A scratch or pre-publication failure aborts/removes owned work and preserves the earlier database or
backup. Retrying after an explicit post-publication sync error must respect whether a complete result is already
visible (`format.md`, Dumps). Filesystem/device sync behavior remains a platform dependency.

## Errors

Every failure elmdb reports carries a code from one enum, the same in Rust and TypeScript (`ErrorCode`): an `Error`
in Rust, and in Node an `Error` whose `code` is it. An argument of the wrong JavaScript type is refused by N-API before
elmdb sees it, with N-API's own code. LMDB's own errors map to
codes too (`MAP_FULL`, `READERS_FULL`, `CORRUPTED`). LMDB's assertions are compiled out of a release build (heed's
`lmdb-master-sys` defines `NDEBUG` outside debug builds), so a release build never aborts on one; damage LMDB doesn't
detect itself is what `check()` is for (`docs/evidence/phase-1.md`).

A panic in elmdb is a bug in elmdb, and in Node it is an error too: the binding runs every export's work where a
panic is caught, and throws it as `PANICKED`, naming the export, where the panic was raised and what it said, or
rejects the promise of a call whose work is on a thread of the pool. Left to unwind into Node, a panic would abort
the process; caught, the process lives. What the panic left in memory is not known, so:

- **The handle refuses calls after it**, with `PANICKED` and the first panic's message, but `close`, `path`,
  `readOnly`, `isOpen` and a unit's `isOpen`. The unit the handle had open is ended by the call that panicked,
  keeping none of its writes, whether the panic was in the unit or in the database: what a panic interrupted may be
  half of a batch. A panic on a thread of the pool is the handle's too, and its unit is ended by the next call on
  the handle or the unit.
- **What was committed is kept.** A transaction a panic unwinds through is aborted, and a unit's place, the write
  transaction and the gate are let go as it unwinds (`Store::begin`, `Database::begin`, `Unit::commit`): the handle closed and the
  database opened again is a database as any other. A panic raised after a commit landed leaves the commit made,
  so the error says the call may not have finished, not that it didn't.
- **A close of such a handle closes it** where the panic was raised at an export's door or in a read or a write,
  which hold none of the locks a close takes. A close that panics itself throws `PANICKED`: where it had taken
  the store, the store is let go as the panic unwinds and the database is closed; where it had not, a close
  asked again closes it, unless the panic was raised holding the store's own lock, where the database stays open
  until its handle is collected or the process ends, a later open of its path refused (`ALREADY_OPEN`), and
  `isOpen` may throw too.
- **A panic on one of elmdb's own threads**, the index builder's, the sync's or the one that lets idle reads go, is not a call's and marks no
  handle: a listener's is caught there (`CALLBACK_PANICKED`), and any other ends the thread, which the close
  that joins it then reports as its own panic.

What V8 collects, an unclosed database or an open unit, and the closes the binding makes as an environment or the
process ends, are dropped where a panic is caught as well. In Rust a panic unwinds to the caller, as any does.

Filesystem support and deployment-specific validation requirements are stated in [filesystems.md](filesystems.md).
