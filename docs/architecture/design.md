# Design

elmdb is an embedded database for Node.js and Electron, written in Rust on LMDB. It holds typed entities, their attributes
and the relations between them, keeps value, text, trigram, vector and rank indexes in the same transaction as the data, runs a read
as one plan and a write as one batch, and reports what every commit changed.

It knows no application. Its first user is apack, which reaches it through EARS, the TypeScript API over a database
that this repository ships beside the binding (`ears.md`); apack's entity types, relation kinds and rules arrive as a
schema.

This document is what every "should elmdb do X?" is checked against. The others:

- [schema.md](schema.md): the schema: types, fields, relations, indexes, and how a schema changes.
- [engine.md](engine.md): the operations and the protocol.
- [search.md](search.md): text, trigram and vector indexes, and search as plan steps.
- [format.md](format.md): the key layout, values, format versions and upgrades.
- [filesystems.md](filesystems.md): the filesystems a database is supported on, and what the tests establish of each.
- [testing.md](testing.md): what proves it correct.
- [ears.md](ears.md): EARS, the fluent TypeScript API over a database.
- [roadmap.md](../roadmap.md): what is built, and what waits on what.
- [background.md](../background.md): why elmdb exists.

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
7. **Formats are explicit.** Every database records its format, and an open refuses one it can't read. The format is
   1, edited in place, until a release has users, and a database crosses an edit by a dump and a load; from then on an
   upgrade is a Rust function from one format to the next, run on a copy, never on open (`format.md`, Versions). A
   move to another LMDB, which no one build holds beside the first, is crossed by a dump from the one and a load into
   the other.
8. **The engine owns collation.** String order is declared once, in the schema's `collation` (icu4x's root collation,
   or a locale's), so a sort means the same wherever it runs.
9. **Reads and writes block; a promise is for work measured to hold the event loop.** A write answers at the call: what
   it made and what it refused are known there, and a read after it sees it, which a caller's code depends on. A write
   of a few operations is short, about 38 µs for a `set` committed alone and about 107 µs for a `create`, through EARS,
   then apack's (`../evidence/phase-10.md`, D17's re-reading, 2026-10-25), and outside a durable write its sync runs
   later on a thread of its own, which `durable()` waits for without holding the event loop. An operation gets a form on
   a thread of the pool, answering a promise, only where its blocking form holds the event loop past 10 ms: a check and
   a rebuild of a large database take seconds (`../evidence/million-notes.md`), a backup copies the whole file, and the
   largest read measured took 266 ms (`../roadmap/javascript-boundary.md`). A read's promise form answers what its
   blocking form would have answered at the call, so a commit made after the call is never in it; what it is refused for
   besides (a close before it runs, a tracked one asked with no listener) and how a tracked one hears of later commits
   are in `engine.md`, Reads. A batch of thousands of operations, a load, a replacement, a reset and a purge can hold it
   as long on a large database, and have no such form yet. One that writes, and whose order among the caller's own
   writes matters (a load, a replacement), would need its turn among them in the order asked, which nothing builds
   today; a rebuild needs none, deriving from whatever is committed when it begins. An everyday write never gets a
   promise form: a caller that needs its writes off the event loop makes all of them on another thread.

## Built on

- **heed** (Meilisearch's Rust binding to LMDB). Its LMDB is upstream `mdb.master` plus one patch, ITS#10395 (several
  nested read transactions inside a write transaction), reported to OpenLDAP and listed in `format.md`. LMDB 1.0
  is released (1.0.1, 2026-08-06), and heed's `heed3` variant is the path to it, but hannoy below doesn't run on heed3
  and heed3's LMDB is a copy from before the release. Format 1 stays on 0.9 (`../roadmap/format-freeze.md`).
- **roaring** bitmaps for sets of entities, as a search joins them and a trigram index keeps them, **charabia** for
  tokenizing, **rust-stemmers** for stems, **levenshtein_automata** for fuzzy terms, **regex** and **regex-syntax**
  for patterns and what a trigram index narrows them by, **hannoy** for approximate nearest neighbours on LMDB,
  **zstd** for stored values and **leiden-rs** for communities.
- **icu4x** (`icu_collator`) for collation, each of its crates pinned to an exact version (`Cargo.toml`;
  `cargo xtask check-exact`): a sort key is the collator's output, so an order of strings is filed under a stamp
  of the collator that made it (`engine.md`, An order of strings). charabia, rust-stemmers and hannoy are pinned the
  same way, since a text index stores the tokenizer's and stemmer's output and a vector index hannoy's graph, and so
  are the crates charabia normalizes, detects a language and segments by (unicode-normalization, whatlang,
  finl_unicode).
- **rmp** for values in Rust, under the value spec in `format.md`; callers encode with any msgpack implementation that
  meets it (EARS uses msgpackr).
- **napi-rs** for the Node binding. N-API is ABI-stable, so an Electron or Node upgrade needs no rebuild.

elmdb doesn't vendor or patch LMDB itself. A fix it needs in LMDB goes upstream to OpenLDAP, and to Meilisearch's fork
until heed has it.

## Packages

One product name, with the host language as the suffix:

| Crate | npm | Role |
|---|---|---|
| `elmdb` | (none) | the engine and its Rust API |
| `elmdb-node` | `elmdb` | the N-API binding |
| `elmdb-cli` | (none) | the `elmdb` command: `inspect`, `check`, `rebuild`, `backup`, `dump`, `load`, `acknowledge-restore` |
| (none: `packages/ears`) | `@elmdb/ears` | EARS, the fluent TypeScript API over the binding (`ears.md`) |

Another host follows the pattern (`elmdb-py`). A new crate needs a reason, such as independent reuse or compile time;
otherwise it is a module. A package that is no binding, written in a host's language over one, has no crate: it lives
under `packages/`, not `crates/`, and takes the product's npm scope rather than a suffix, `@elmdb/<name>`, since a
bare name may be someone else's (`ears` on npm is). It names the binding as an exact peer, so a process holds one
(principle 5). `packages/typescript-floor` is no package: a private workspace of the tooling, holding the oldest
TypeScript EARS supports.

## Vocabulary

Each word means one thing, in the code, the docs and the errors:

| Word | Means |
|---|---|
| **database** | one directory holding one LMDB environment, opened by one `Database` |
| **schema** | what the caller declares: **types**, their **fields**, **relation kinds**, **rank indexes**, and each type's **short code** prefix |
| **entity**, **attribute**, **relation** | a typed row, one field's value on it, an edge of a kind between two entities |
| **role** | a name the schema declares that entities are granted and a plan filters by |
| **place** | an entity's number in the order entities were made, which its type's rows and its indexes' entries are filed by |
| **index** | derived data kept for a type's fields, `value`, `text`, `trigram` or `vector`, or a **rank index** over a graph |
| **plan** | a read, compiled: a start, steps, a terminal |
| **batch** | a write, compiled: one unit's operations |
| **unit**, **savepoint**, **commit** | a write transaction, a rollback point inside one, its end |
| **change set** | what a commit changed: entities, types, fields, relations |
| **tracked read** | a read whose key ranges are filed under a token, which each commit that writes into them names |
| **sequence** | a commit's number, only increasing: what orders a change set against a batch's result |
| **watch** | a plan re-run when a commit changes what it read: a tracked read and its re-run |
| **checkpoint** | a reader's place among the commits, written in the unit that holds what it derived up to there |
| **primary**, **derived** | the truth, and what is rebuilt from it |
| **engine** | the Rust crate (`crates/elmdb`, `engine.md`) that does a database's work |
| **EARS** | the entity-attribute-relation system, the TypeScript API over a database (`@elmdb/ears`, `ears.md`) |

EARS calls what `createEarsEngine` returns an engine (`EarsEngine`), a name it brought from apack and kept through the
move, so that the moved specs proved nothing changed; renaming it is left for after the move
(`../archive/roadmap/ears.md`). Until then a document says **an EARS engine** for that object, and "the engine" alone
is always the crate.

Not used: core, layer, adapter, plugin, store (for a database), record store. "Addon" means only Node's `.node` file.
`store` is the name of one Rust module, the engine's hold on LMDB (its transactions, the gate, the syncs), and of
nothing a caller sees.

## Conventions

- **Rust modules are named by the vocabulary:** `schema`, `format`, `store` (above), and in `engine`: `plan`, `batch`,
  `unit`, `values`, `text`, `trigram`, `vector`, `ranks`, `changes`, `watch`, `admin`.
- **Operation names say what they do.** `get` may answer nothing, `find` searches and may answer an empty list, `read`
  runs a plan; a name never says who may call it or which database it acts on (the caller holds the `Database`).
- **The plain name blocks.** `read`, `write`, `check`, `rebuild` and `backup` answer on the calling thread, and the
  forms that run on a thread of the pool and answer a promise are named `…Async` (principle 9). This is the reverse of
  Node's own `readFile` and `readFileSync`, on purpose: the form a caller should reach for is the one with no suffix,
  and here that is the blocking one, so a caller who writes `db.write(…)` and reads on gets what it wrote.
  `durable()` keeps its plain name though it answers a promise: it is a wait for syncs already owed, with no blocking
  form of its own, and the blocking way to wait is a durable write (`write(batch, true)`, `unit.commit(true)`).
- **Errors are one Rust enum of stable `SCREAMING_CASE` codes**, each with a message naming the fix; the TypeScript is
  generated. A code is never reused for another meaning.
- **Units of measure are in the name:** `syncIdleMs`, `mapSizeBytes`.
- **Every key prefix is declared in one table** (`format.md`), marked primary, derived or meta.

## The boundary with apack

elmdb's public API is its protocol: schemas, plans, batches, results, change sets and error codes, as data
(`engine.md`). Over it this repository ships two things in JavaScript:

- **the thin client** (`elmdb/client.js`): the binding's reads and writes taking and answering the protocol's
  generated types, over a msgpack codec the caller passes in, so decoding values stays the caller's. It decides one
  thing itself, the decode an answer needs: an answer whose maps may hold a key `__proto__` (`plainObjects`,
  `engine.md`) is decoded with maps as `Map`s and made objects with no prototype, which hold that key as their own;
- **EARS** (`@elmdb/ears`, `packages/ears`, `ears.md`): the fluent API a program writes, its query and write
  builders compiling to plans and batches typed by the generated protocol, its watches over tracked reads,
  `EarsError` over elmdb's codes, the typed contract (`EARS`, entity shapes, typed facades) and the specs that hold
  it, the black-box, vocabulary and model specs among them, run against the binding.

The binding's package opens only these paths (its `package.json`'s `exports`): `elmdb`, the loader and its
declarations; `elmdb/client` and `elmdb/client.js`, the thin client; `elmdb/protocol/<Name>` and
`elmdb/protocol/<Name>.js`, the generated types, which are declarations alone and resolve for a type import only;
`elmdb/protocol/schema-1.json`, the schema's JSON Schema; and `elmdb/package.json`. The loader's own files, the
`.node` binaries among them, are reached by the loader and by nothing that imports the package. EARS' are `@elmdb/ears`
and `@elmdb/ears/database` (`ears.md`). The two are released as a pair at one version, the workspace's, which EARS'
peer names exactly (`cargo xtask check-versions`; `testing.md`, Releases).

apack keeps what is its own: the database it binds and the operations over it (its SDK's `ears` entry), the schema
assembled from its packs' manifests, codegen and the entity names it generates, its import and its backup, and the
checks of its codegen over built packs. It consumes EARS as it consumes the binding, once its switch from the copy it held is finished there (phase 2 of
`../archive/roadmap/ears.md`): from a pinned checkout of this repository, or from the two packages CI publishes for each
commit on `main` as the prerelease `pkg-<commit>`, installed by URL (`testing.md`, Each commit's packages).

`badd720` (2026-10-24) drew this line the other way, leaving the builders in apack because they changed with its
vocabulary, until they settled and another program wanted them. The repositories' owner moved them here on
2026-11-14 for other reasons: the builders are the only way anyone uses elmdb from JavaScript, EARS' specs are the
widest test of the engine from outside and ran only in apack, after an engine change had merged here, and a protocol
change and the builder that uses it become one pull request. The move, what it cost and what it settled are in
`../archive/roadmap/ears.md`; EARS arrived in elmdb#207, its history kept. EARS still checks some of what the engine
checks again, and names three refusals of its own in TypeScript, against principle 1 and the rule that errors are
`ErrorCode`s; each is listed there as opened by the move, to be decided on a reading.

## Not in scope

- Calling models. A vector index tracks what needs embedding; the caller embeds (`search.md`).
- A server, a network protocol, or replication.
- Access control: the caller is trusted, as apack's packs run in-process.
- A general-purpose LMDB binding for JavaScript.

## Settled questions

- **Growing the map.** The engine grows it itself, between units (`engine.md`, Opening).
- **Tests without an in-memory backend.** There is none: a test opens a database in a temporary directory with
  `syncs: false`. What that costs apack's suites isn't gated.
