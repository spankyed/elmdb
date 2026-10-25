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
| `d/fd` type index seq | the entity's document: its id, each field's length, its terms | derived |
| `d/fs` type index | a text index's statistics: its documents, each field's total length | derived |
| `d/qc`, `d/qp` type index id chunk | a vector index's chunk: its hash and text; and that it waits for an embedding | derived |
| `d/qi` type index item, `d/qm` type index id chunk | a chunk's item in the index's hannoy graph, both ways | derived |
| `d/qj` type index item | a unit's graph changes, applied as it commits | derived |
| `d/qn` type index | the index's graph number and next item | derived |

`rebuild()` deletes everything under `d/` and builds it from `p/`, the hannoy graphs (LMDB databases
`elmdb.graphs.cosine` and `elmdb.graphs.euclidean`) with it. `check()` does the same in a scratch transaction and
compares.

## Values

A value is msgpack bytes, written by the caller and stored as given:

- maps keep their keys as written, and a key named `__proto__` is a key;
- `nil`, booleans, integers, floats, strings, binary, arrays and maps whose keys are strings, and of the extension
  types only msgpack's timestamp (type −1, how a JavaScript `Date` encodes); a write holding any other extension is
  refused (`VALUE_INVALID`). apack's stored values hold no extension at all (`docs/evidence/phase-1.md`);
- the engine decodes a value only where it must: to index it, to check a required field, to filter by it, to extract
  text.

Values above a size are compressed with zstd, marked by a tag byte no msgpack value starts with, later with
dictionaries trained per type for small values such as messages.

## Versions

`m/format` records the format as an integer. An open refuses a database in another format, naming it, and, for a newer
one, saying to update. An upgrade is a Rust function from one format to the next, run by `elmdb upgrade <dir>` on a
copy, a step at a time, each recording its format. Nothing upgrades on open.

## Moving apack's data

lmdb-js writes LMDB files in its own data version, 2. Stock LMDB 0.9 (`mdb.master`, the one heed runs) writes and reads
version 1, and LMDB 1.0 (`mdb.master3`) version 3, so no stock LMDB opens a file lmdb-js wrote: heed refuses one with
`MDB_INVALID` (`docs/evidence/phase-1.md`). The move therefore has two steps, and only the first needs lmdb-js:

1. `tools/lmdb-js-export` (a Node script whose one dependency is lmdb-js, part of neither package) copies an apack
   format 4 database's `kv` entries, byte for byte and in key order, into a dump: the 8 bytes `ELMDBF4\0`, a u32 dump
   version (1), each entry as a u32 key length, the key, a u32 value length and the value, then a u32 `0xFFFFFFFF` and
   a u64 entry count, all little-endian. It opens the source read-only and refuses anything but an EARS database in
   format 4.
2. `elmdb upgrade <dump> <dir> [--schema <file>]` reads the dump into a new database: format 4's keys decode with
   elmdb's codec (its strings hold no NUL), `v`, `i`, `r` and `s` map onto `p/v`, `p/i`, `p/r` and `p/s` (a short
   code's sequence `<Type>:<PREFIX>` becomes `shortCode:<Type>`, and `m seq` the store's `seq`), attribute values are
   held to the value spec, format 4's own rows are read with msgpackr's `undefined` (extension type 0) as absent, and
   its derived keys are left behind and rebuilt. It loads unchecked, so `check()` afterwards reports what the data
   holds that the schema's rules refuse (`docs/evidence/phase-7.md`).

apack's last release on lmdb-js converts every database to its format 4 first. apack's run history moves from its
record store into the `runs` database through `bulkLoad`.

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
