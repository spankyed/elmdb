# Format

How a database lays out its data, and how a layout changes. Built: `crates/elmdb/src/format/`, `crates/elmdb/src/store/`.

## Keys

One LMDB database per environment holds every key, in an order-preserving binary encoding of tuples (strings, integers,
booleans, null) defined in `format`. Every prefix is declared in one table there, marked primary or derived; the table
below starts from apack's format 4 layout, with the prefixes split by kind.

| Prefix | Holds | Kind |
|---|---|---|
| `p/v` id field | an attribute's value | primary |
| `p/i` id | that the entity exists: its place, whether it is trashed, and the sequence of the commit that last wrote it (its type is in its id) | primary |
| `p/r` relation | a relation's row: its kind, ends, info, sequence, and its weight when it isn't 1 | primary |
| `p/s` key | a numbering sequence | primary |
| `p/q` type index id chunk | an embedding: its model, its chunk's hash, its f32 values | primary |
| `m/` | the format, the kind, the schema and its hash, the last commit's sequence, how many times every row has been replaced, whether the change order is built, and how far tombstones are trimmed and each type's newest destroy | meta |
| `d/e`, `d/t` type seq | a type's live and trashed entities, in creation order | derived |
| `d/c` type | a type's count | derived |
| `d/h` type changed place | a type's entities in the order of the commits that last wrote them, for a type whose schema sets `changeOrder` | derived |
| `d/rc` kind | a relation kind's count | derived |
| `d/f` type field | that a type's entities have held the field: put by its first write, kept when its last value goes, pruned only by a rebuild | derived |
| `d/xb` type index | that a type's index (value, text, trigram or vector) is built; a plan reads only a built one | derived |
| `d/x` type index values | a value index: the places of the entities holding the values, each as its distance from the one before, when they are no more than 256; that they are in blocks, when more. A place is an entity's through `d/e` or `d/t` | derived |
| `d/xp` type index values start | a block of the places of the entities holding values more than 256 hold: those of a range of places that starts at `start`, the largest range of a power of two places starting at a multiple of its size that holds no more than 256, as a text index's `d/fb` has them, with no counts | derived |
| `d/xw` type index values | the places, held as `d/x` holds them, of the entities of the type without a value in a value index's last field, which is missing or null, under what its fields before that one hold: none for an index on one field, and for one on several each a value an equality can ask for, once for each tuple of them when lists are among them | derived |
| `d/xwp` type index values start | a block of those places, as `d/xp` is of a value's | derived |
| `d/xu` type index values | the places, held as `d/x` holds them, of the entities of the type the index doesn't order, under what its fields before the last hold as `d/xw` has them: its last field holds a list or a value the index can't file but a long string, or the index is unique and a field before the last holds a list | derived |
| `d/xup` type index values start | a block of those places, as `d/xp` is of a value's | derived |
| `d/xl` type index values | the places, held as `d/x` holds them, of the entities of the type whose last field in the index is a long string, one of more than 256 bytes, which the index has no entry for, under what its fields before the last hold as `d/xw` has them | derived |
| `d/xlp` type index values start | a block of those places, as `d/xp` is of a value's | derived |
| `d/xo` type index build values sortkey cut string | nothing: that the value index has an entry for the string under these values, filed where the schema's collation orders it (`../roadmap/string-order.md`). `build` is the sequence of the commit that began the keys' build; `sortkey` is the collator's sort key of the string, its first bytes when the key hasn't room for them all, and `cut` says which. The room is what a key's 511 bytes leave after what comes before the sort key, its tag and end, `cut`, and the 258 bytes the longest string an entry holds takes with no zero byte in it: one room for every string under the values, and no order is kept under values that leave it less than 64 bytes. What is under a build is its stamp's to lay out, and this is the layout of stamp number 1. The families are declared and their keys laid out; no write files them yet | derived |
| `d/xo` type index build values sortkey cut place | nothing: that the entity at the place holds, under these values, a string with no key of the kind above: a long one, or one of fewer bytes whose zero bytes, two bytes each in a key, make it take more than those 258 | derived |
| `d/xob` type index | the marker of an index's `d/xo` keys: the stamp of the collator that made them, their build, and whether the build is whole | derived |
| `d/o`, `d/n` entity kind seq | an entity's outgoing and incoming edges: `[relation, far end]`, and the row's weight third when it holds one | derived |
| `d/b` source kind target seq | the edges between two entities, among which a link finds the one with its identity | derived |
| `d/ft` type index term | a text index's term: the places of the entities holding it, each with the term's count in each field, when they are no more than 256; that they are in blocks, when more | derived |
| `d/fb` type index term start | a block of the places of the entities holding a term more than 256 hold, each with the term's count in each field: those of a range of places that starts at `start`, the largest range of a power of two places starting at a multiple of its size that holds no more than 256 | derived |
| `d/fd` type index seq | the entity's document: its terms, each with its positions in each field, and its words as written | derived |
| `d/fl` type index block | each field's length in the entities of one block of 512 places: every place's, or those of the places a bit each names | derived |
| `d/fs` type index | a text index's statistics: its documents, each field's total length | derived |
| `d/fw` type index word | a stemming text index's word as written: how many entities hold it | derived |
| `d/g` type index trigram block | a trigram index: the places of the entities holding the three characters, case folded, in one block of 2^16 places, as a roaring bitmap | derived |
| `d/qc`, `d/qp` type index id chunk | a vector index's chunk: its hash and text; and that it waits for an embedding | derived |
| `d/qi` type index item, `d/qm` type index id chunk | a chunk's item in the index's hannoy graph, both ways | derived |
| `d/qj` type index item | a unit's graph changes, applied as it commits | derived |
| `d/qn` type index | the index's graph number and next item | derived |
| `d/ra` index entity | a rank index's node: its estimate, residual and out-weight, three f64, and its place | derived |
| `d/rk` index score type seq | a rank index's nodes in score order, highest first, the estimate kept to as many significant bits as a cell of 256 times the tolerance takes, 4 to 52 | derived |
| `d/rq` index type seq | a rank index's nodes whose residual is over its threshold, waiting to be pushed | derived |
| `d/rs` index | a rank index's totals: its nodes, their estimates and residuals summed, the changes to a node since those were counted, the store's sequence as the last commit to write the index read it, how far it is built, where its sweep is | derived |
| `l/d` sequence type id | an entity a commit destroyed, which leaves no `p/i`; those of the last 4096 commits (`TOMBSTONE_COMMITS`) | log |
| `l/k` name | a reader's checkpoint, the bytes it gave, written in the unit whose work it records (`Unit::set_checkpoint`) | log |

A tombstone or a checkpoint is neither primary nor derived: a dump leaves it out, `rebuild()` leaves it alone, and
`check()` doesn't read it. A backup keeps it with the rows it describes, and `reset` and `replace_with` drop it with
them.

`rebuild()` deletes everything under `d/` and builds it from `p/`, the hannoy graphs (LMDB databases
`elmdb.graphs.cosine` and `elmdb.graphs.euclidean`) with it. `check()` derives the same from a read snapshot into scratch files and
compares. A rank index's stored bits depend on the order of the commits and pushes that made it, so `check()` holds
it to what it promises instead: its scores within the bound it reports of PageRank over the same graph, each residual
and out-weight what the estimates and the edges make them, and its score order, queue and totals what its nodes make
them.

## Values

A value is msgpack bytes, written by the caller and stored as given:

- maps keep their keys as written, including repeated keys, and a key named `__proto__` is a key. Deep equality
  ignores entry order but preserves each entry's multiplicity;
- `nil`, booleans, integers, floats, strings, binary, arrays and maps whose keys are strings, and of the extension
  types only msgpack's timestamp (type −1, how a JavaScript `Date` encodes); a write holding any other extension is
  refused (`VALUE_INVALID`). apack's stored values hold no extension at all (`docs/evidence/phase-1.md`);
- the engine decodes a value only where it must: to index it, to check a required field, to filter by it, to extract
  text.

A field's value of `COMPRESS_FROM` bytes of msgpack or more (`crates/elmdb/src/engine/values.rs`) is stored compressed
when that is smaller: the byte `0xc1`, which no msgpack value starts with and which a write's value therefore never
does (a write holding it is no msgpack, and refused), followed by a zstd frame (level 3) of the value's msgpack. Every
other value is its msgpack as given. A read decompresses where it reads a field — a row, a field, a filter, an index's
or a text index's derivation, `check()`, a dump — so nothing past the store sees the stored form, and a dump holds the
value. Keys are never compressed, their bytes being their order; relation info and other primary records are stored
as they are. Per-type dictionaries for small values are designed and not built: G8's values under 256 bytes average
14 bytes, which no dictionary makes smaller than they are stored (`docs/roadmap/value-dictionaries.md`).

## Versions

`m/format` records the format as an integer, and an open refuses a database in another format, naming both: a newer
one says to update elmdb, an older one that it is from before the format was fixed and is made again.

**The format is 1 until a release has users** (apack's `docs/archive/plans/storage-format-one.md`). Until then a change to the
stored layout edits format 1 in place: the number doesn't move, and no upgrade step is written. A change to derived data
needs nothing, `rebuild()` building it from primary data. A derived tag a layout stops writing under is retired
(`Prefix::RETIRED`, `d/fp` so far): no prefix takes it again, and a `rebuild()` of everything clears what a database
still holds under it. The databases that exist are their developers', reset or
converted once on a copy (Dumps, below). The accepted cost is that a build may misread a database from before an
in-place change rather than refuse it; the answer is the same reset, or a dump made before the change.

**From the first release with users** (its tag is the condition), format 1 is locked: every later change to the stored
layout bumps the format and ships an upgrade, a Rust function from one format to the next, run on a copy and never on
open. This section then names the tag.

## Dumps

`elmdb dump <dir> <file>` writes one moment of a database to a new file, and `elmdb load <file> <dir>` makes a new
database of it (`crates/elmdb/src/dump.rs`). The file names no key or stored value: it holds the schema and the primary
data as loads (`Load`), so a database crosses a change to format 1's layout by a dump from the build before the change
and a load by the build after it. That is how our databases are converted once, on a copy, while the format is 1.

- The file: the 8 bytes `ELMDBDMP`, a u32 dump version (1), a u32 length and the schema's canonical JSON (length 0:
  none), then each load as a u32 length and its msgpack with named fields, then a u32 `0xFFFFFFFF` and a u64 load
  count, all little-endian.
- The loads, in the order a load applies them: each entity in the order it was made, with every field it holds, the
  engine's own (`shortCode`, `trashedAt`, ...) among them; each relation in the order it was made, under its id; each
  embedding (`p/q`); each sequence (`p/s`), last, so a numbering goes on where it was.
- Export spill-sorts only entity/relation identifiers, then emits one complete load at a time from the same snapshot.
  Embeddings and sequences are visited directly. Scratch has a per-sorter 8 MiB spill threshold plus row/buffer
  overhead; one load's decoded fields or embedding still needs its own memory. Original u64 creation order, field
  order and dump bytes are preserved. Export scratch needs writable temporary storage and follows the cleanup
  limitations in `engine.md`, Recovery resources.
- A load applies unchecked batches in a private database in the destination directory, using at most 1,000 loads
  or 8 MiB of encoded frames per batch plus the largest frame. Decoded values and rebuild state have separate
  workload-dependent costs. It validates the footer/count and refuses trailing bytes, rebuilds derived data, then
  closes and publishes the complete database without replacing any existing file. The destination is reserved
  against this process's opens and publishers; coordinate its ownership across processes. Before publication,
  failure removes only private staging files, leaving unrelated destination files intact and permitting retry.
  A publication-sync error reports that the complete database is visible; never delete or overwrite it blindly.
  Unix syncs affected directories; Windows requests write-through publication. A dump that fails removes its file.
  The data's violations of schema rules remain `check()`'s to report, as after any unchecked load; a value outside the
  value spec is no such violation, and refuses the load (`VALUE_INVALID`).
  An abruptly killed process can leave an unpublished `.elmdb-load-*` directory. A retry starts new private staging
  and never adopts the partial database. Remove abandoned staging only after confirming its owner has stopped and
  coordinating exclusive destination ownership; preserve unrelated files.
- A load of a dump, dumped again, is byte-identical to it.

The dump version is the file's, not the stored format's: it moves only when `Load` does.

## LMDB

elmdb runs on heed's LMDB: upstream `mdb.master` plus one patch.

| Patch | What | Upstream |
|---|---|---|
| ITS#10395 | several nested read transactions inside a write transaction | reported, not merged |

Its base trails upstream: on 2026-10-24 heed 0.23.0's LMDB sat on upstream's 2026-04-27 commit, without ten later
fixes, among them ITS#10642 (`mdb_subdb_adjust` alignment) and ITS#10643 (page counts when deleting DUPSORT records).
Neither reaches elmdb: `mdb_subdb_adjust` isn't in heed's LMDB at all, and both concern DUPSORT sub-databases, which
elmdb doesn't use (`docs/evidence/phase-1.md`). No CI job tracks the upstream commits heed's LMDB lacks: they are checked by hand, and a fix elmdb needs goes to
Meilisearch's fork as a rebase request or a pull request.
