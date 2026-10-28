# Format

How a database lays out its data, and how a layout changes. Built: `crates/elmdb/src/format/`, `crates/elmdb/src/store/`.

## Keys

One LMDB database per environment holds every key, in an order-preserving binary encoding of tuples (strings, integers,
booleans, null) defined in `format`. Every prefix is declared in one table there, marked primary or derived; the table
below starts from apack's format 4 layout, with the prefixes split by kind.

| Prefix | Holds | Kind |
|---|---|---|
| `p/v` id field | an attribute's value | primary |
| `p/i` id | an entity's type and sequence | primary |
| `p/r` relation | a relation's row | primary |
| `p/s` key | a numbering sequence | primary |
| `p/q` type index id chunk | an embedding: its model, its chunk's hash, its f32 values | primary |
| `m/` | the format, the schema, the last synced transaction, the last commit's sequence | meta |
| `d/e`, `d/t` type seq | a type's live and trashed entities, in creation order | derived |
| `d/c` type | a type's count | derived |
| `d/xb` type index | that a value index is built; a plan reads only a built one | derived |
| `d/x` type field value seq | a value index | derived |
| `d/o`, `d/n` entity kind seq | an entity's outgoing and incoming edges | derived |
| `d/b` source kind target seq | the edges between two entities | derived |
| `d/ft` type index term | a text index's posting list: the places of the entities holding the term | derived |
| `d/fp` type index term seq | where the entity holds the term: each field's positions | derived |
| `d/fd` type index seq | the entity's document: its id, each field's length, its terms, its words as written | derived |
| `d/fs` type index | a text index's statistics: its documents, each field's total length | derived |
| `d/fw` type index word | a stemming text index's word as written: the places of the entities holding it | derived |
| `d/qc`, `d/qp` type index id chunk | a vector index's chunk: its hash and text; and that it waits for an embedding | derived |
| `d/qi` type index item, `d/qm` type index id chunk | a chunk's item in the index's hannoy graph, both ways | derived |
| `d/qj` type index item | a unit's graph changes, applied as it commits | derived |
| `d/qn` type index | the index's graph number and next item | derived |

`rebuild()` deletes everything under `d/` and builds it from `p/`, the hannoy graphs (LMDB databases
`elmdb.graphs.cosine` and `elmdb.graphs.euclidean`) with it. `check()` does the same in a scratch transaction and
compares.

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
as they are. Per-type dictionaries for small values, such as short messages, are a later design
(`docs/roadmap.md`).

## Versions

`m/format` records the format as an integer, and an open refuses a database in another format, naming both: a newer
one says to update elmdb, an older one that it is from before the format was fixed and is made again.

**The format is 1 until a release has users** (apack's `docs/archive/plans/storage-format-one.md`). Until then a change to the
stored layout edits format 1 in place: the number doesn't move, and no upgrade step is written. A change to derived data
needs nothing, `rebuild()` building it from primary data. The databases that exist are their developers', reset or
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
  The data's violations of schema rules remain `check()`'s to report, as after any unchecked load.
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
elmdb doesn't use (`docs/evidence/phase-1.md`). A CI job lists the upstream commits heed's LMDB lacks, and a fix elmdb needs goes to
Meilisearch's fork as a rebase request or a pull request.
