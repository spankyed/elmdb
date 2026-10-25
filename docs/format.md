# Format

How a database lays out its data, and how a layout changes. Status: designed, not built.

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
| `m/` | the format, the schema, the last synced transaction | meta |
| `d/e`, `d/t` type seq | a type's live and trashed entities, in creation order | derived |
| `d/c` type | a type's count | derived |
| `d/x` type field value seq | a value index | derived |
| `d/o`, `d/n` entity kind seq | an entity's outgoing and incoming edges | derived |
| `d/b` source kind target seq | the edges between two entities | derived |
| `d/f…` | text indexes: terms, postings, positions, statistics | derived |
| `d/q…` | vector indexes, through hannoy, and pending embeddings | derived |

`rebuild()` deletes everything under `d/` and builds it from `p/`. `check()` does the same in a scratch transaction and
compares.

## Values

A value is msgpack bytes, written by the caller and stored as given:

- maps keep their keys as written, and a key named `__proto__` is a key;
- integers, floats, strings, binary, arrays, maps, `nil` and booleans, and only the extension types apack's format 4
  values actually hold (at least msgpack's timestamp, for `Date`), which the goal's first storage phase lists from the
  codec (apack's `packages/apack-ears/src/kv.ts`) and a scan of every stored value;
- the engine decodes a value only where it must: to index it, to check a required field, to filter by it, to extract
  text.

Values above a size are compressed with zstd, marked by a tag byte no msgpack value starts with, later with
dictionaries trained per type for small values such as messages.

## Versions

`m/format` records the format as an integer. An open refuses a database in another format, naming it, and, for a newer
one, saying to update. An upgrade is a Rust function from one format to the next, run by `elmdb upgrade <dir>` on a
copy, a step at a time, each recording its format. Nothing upgrades on open.

## Moving apack's data

apack's last release on lmdb-js converts every database to its format 4. elmdb's first format reads format 4's keys and
values (both are plain bytes in one LMDB database, and its values are msgpack) and its upgrade rewrites them under the
prefixes above and rebuilds the derived data. apack's run history moves from its record store into the `runs` database
through `bulkLoad`.

## LMDB

elmdb runs on heed's LMDB: upstream `mdb.master` plus one patch.

| Patch | What | Upstream |
|---|---|---|
| ITS#10395 | several nested read transactions inside a write transaction | reported, not merged |

Its base trails upstream: on 2026-10-24 heed 0.23.0's LMDB sat on upstream's 2026-04-27 commit, without ten later
fixes, among them ITS#10642 (`mdb_subdb_adjust` alignment) and ITS#10643 (page counts when deleting DUPSORT records;
elmdb uses no DUPSORT database). A CI job lists the upstream commits heed's LMDB lacks, and a fix elmdb needs goes to
Meilisearch's fork as a rebase request or a pull request.
