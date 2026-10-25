# Design

elmdb is an embedded graph database for JavaScript, written in Rust on LMDB. It holds typed entities, their attributes
and the relations between them, keeps value, text and vector indexes in the same transaction as the data, runs a read
as one plan and a write as one batch, and reports what every commit changed.

It knows no application. Its first user is apack, whose EARS engine it replaces underneath an unchanged API; apack's
entity types, relation kinds and rules arrive as a schema.

This document is what every "should elmdb do X?" is checked against. The others:

- [schema.md](schema.md): the schema: types, fields, relations, indexes, and how a schema changes.
- [engine.md](engine.md): the operations and the protocol.
- [search.md](search.md): text and vector indexes, and search as plan steps.
- [format.md](format.md): the key layout, values, format versions and upgrades.
- [testing.md](testing.md): what proves it correct.
- [roadmap.md](roadmap.md): the order of work, starting with apack's move off lmdb-js.
- [background.md](background.md): why elmdb exists.

## Principles

1. **One implementation.** Rust does the work, the caller describes it. No logic exists in two languages: storage,
   indexes, refusals, numbering, units and durability are the engine's alone.
2. **Primary data is the only truth, and everything derived can be thrown away.** Entities, attributes and relations
   are primary. Indexes, counts, change feeds and search are derived, kept in the same transaction, and rebuildable
   from the primary data: `check()` rebuilds and compares, `rebuild(index)` replaces. An index bug is a repair, never
   a loss.
3. **One crossing per operation.** A plan or a batch crosses in, a compact result crosses out.
4. **No borrowed memory crosses.** Results are owned bytes. JavaScript never holds a view into LMDB's map, so reading
   freed memory isn't something a caller can do.
5. **One LMDB per process, by construction.** The Rust dependency tree holds exactly one LMDB `-sys` crate (a CI
   check over `cargo tree`), and the binding refuses to load twice into one process.
6. **Each cross-language contract is defined once.** Schemas, plans, batches, results, change sets and error codes are
   Rust types; their TypeScript types are generated from them and never written by hand, so drift fails the build.
7. **Formats are explicit.** Every database records its format, and an upgrade is a Rust function from one format to
   the next, run by `elmdb upgrade` on a copy, never on open.
8. **The engine owns collation.** String order is declared once (icu4x's root collation), so a sort means the same
   wherever it runs.

## Built on

- **heed** (Meilisearch's Rust binding to LMDB). Its LMDB is upstream `mdb.master` plus one patch, ITS#10395 (several
  nested read transactions inside a write transaction), reported to OpenLDAP and listed in `format.md`. heed's
  `heed3` variant, on stock `mdb.master3` (LMDB 1.0), is the path to LMDB 1.0 once it is released and the crates below
  support it.
- **roaring** bitmaps for posting lists and sets of entities, **charabia** for tokenizing, **levenshtein_automata**
  for fuzzy terms, **hannoy** for approximate nearest neighbours on LMDB.
- **rmp** for values in Rust, under the value spec in `format.md`; callers encode with any msgpack implementation that
  meets it (apack uses msgpackr).
- **napi-rs** for the Node binding. N-API is ABI-stable, so an Electron or Node upgrade needs no rebuild.

elmdb doesn't vendor or patch LMDB itself. A fix it needs in LMDB goes upstream to OpenLDAP, and to Meilisearch's fork
until heed has it.

## Packages

One product name, with the host language as the suffix:

| Crate | npm | Role |
|---|---|---|
| `elmdb` | (none) | the engine and its Rust API |
| `elmdb-node` | `elmdb` | the N-API binding |
| `elmdb-cli`, later | (none) | `inspect`, `check`, `backup`, `upgrade` |

Another host follows the pattern (`elmdb-py`). A new crate needs a reason, such as independent reuse or compile time;
otherwise it is a module.

## Vocabulary

Each word means one thing, in the code, the docs and the errors:

| Word | Means |
|---|---|
| **database** | one directory holding one LMDB environment, opened by one `Database` |
| **schema** | what the caller declares: **types**, their **fields**, **relation kinds**, and each type's **code** prefix |
| **entity**, **attribute**, **relation** | a typed row, one field's value on it, an edge of a kind between two entities |
| **index** | derived data kept for a field: `value`, `text` or `vector` |
| **plan** | a read, compiled: a start, steps, a terminal |
| **batch** | a write, compiled: one unit's operations |
| **unit**, **savepoint**, **commit** | a write transaction, a rollback point inside one, its end |
| **change set** | what a commit changed: entities, types, fields, relations |
| **watch** | a plan re-run when a commit changes what it read |
| **primary**, **derived** | the truth, and what is rebuilt from it |

Not used: core, layer, adapter, plugin, store (for a database), record store. "Addon" means only Node's `.node` file.

## Conventions

- **Rust modules are named by the vocabulary:** `schema`, `plan`, `batch`, `index::{value, text, vector}`, `change`,
  `admin`, `format`.
- **Operation names say what they do.** `get` may answer nothing, `find` searches and may answer an empty list, `read`
  runs a plan; a name never says who may call it or which database it acts on (the caller holds the `Database`).
- **Errors are one Rust enum of stable `SCREAMING_CASE` codes**, each with a message naming the fix; the TypeScript is
  generated. A code is never reused for another meaning.
- **Units of measure are in the name:** `syncIdleMs`, `mapSizeBytes`.
- **Every key prefix is declared in one table** (`format.md`), marked primary or derived.

## The boundary with apack

elmdb's public API is its protocol: schemas, plans, batches, results, change sets and error codes, as data
(`engine.md`). The fluent API pack code writes stays in apack's `@apackdev/ears`:

- the typed contract (`EARS`, entity shapes, typed facades) and its editor completions;
- the query and write builders, which compile to plans and batches;
- `EarsError`, wrapping elmdb's codes;
- value decoding, and the black-box, vocabulary and model specs, run against elmdb.

The builders change with apack's vocabulary and the protocol changes far less, so each lives where it changes. If the
builders settle and another program wants them, they move into elmdb then. apack's side of the move is
`docs/plans/ears-on-elmdb.md` in apack.

## Not in scope

- Calling models. A vector index tracks what needs embedding; the caller embeds (`search.md`).
- A server, a network protocol, or replication.
- Access control: the caller is trusted, as apack's packs run in-process.
- A general-purpose LMDB binding for JavaScript.

## Open questions

- **Growing the map.** LMDB refuses a write past the map size (`MDB_MAP_FULL`). A large map costs only address space
  on macOS and Linux, but on Windows LMDB sizes the file to the map. Options: a large map everywhere but Windows, or
  the engine growing the map between units when one fails, and retrying that unit, which it can do because a batch is
  data rather than a caller's function.
- **Tests without an in-memory backend.** Every test opens a database in a temporary directory. Whether that is fast
  enough for apack's suites is measured during parity (`roadmap.md`); the fallback is an in-memory backend inside the
  engine, which is still one implementation.
