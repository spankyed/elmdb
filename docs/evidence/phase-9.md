# Phase 9: apack parity

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Node 23.11.1, Rust 1.99.1. apack's worktree
`~/Develop/Projects/AgentBuddy-worktrees/ears-on-elmdb`, branch `session/ears-on-elmdb`, from `ears-disk-engine`
`3e0b280ff`.

## What was built

**The link (D11, D21).** `vendor/elmdb` is a gitignored symlink to the elmdb checkout and `vendor/elmdb.rev` the commit
it is pinned to; `npm run link:elmdb` (`scripts/link-elmdb.mjs`, plain Node, run by `session:new` before `npm ci`)
makes the link and refuses a target that isn't an elmdb checkout, doesn't hold the pinned commit in its history, or
hasn't built its binding. `@apackdev/ears` depends on `"elmdb": "file:../../vendor/elmdb/crates/elmdb-node"`; the
lockfile gains 13 lines, the link and its package. That took one change on elmdb's side: the binding's build and test
tooling moved to a private root package (`cfcf6c4`), since npm installs a linked directory's dev dependencies as part of
the linking tree, and the binding's had put `@napi-rs/cli`'s whole tree into apack's lockfile.

- **The Linux runner** (`scripts/linux-runner.ts`) mounts the checkout `vendor/elmdb` points at read-only, copies it into
  a volume of its own (Cargo's target directory and the binding kept there), builds the binding, and links the
  container's `vendor/elmdb` to it before `npm ci`. Its image installs the toolchain elmdb's `rust-toolchain.toml`
  pins, read by the runner and named in the image's tag (D19). Inside it, `require('elmdb')` loads the Linux arm64
  build and opens a database.
- **apack's CI**: `.github/actions/elmdb` clones elmdb at the pinned commit into `vendor/elmdb`, caches Cargo's target
  directory and builds the binding (cargo on Windows, as elmdb's own CI does), before each `npm ci` of the CI,
  strict-chain, compiler and acceptance workflows.
- **Publishing**: a staged publish tree (`stagePublishTree`) publishes a `file:` dependency as the version of the
  package it links, and the packaged-authoring check installs elmdb's tarball through npm's `overrides`, as it installs
  the `@apackdev` packages' tarballs in place of the registry.

**The engine on elmdb.** `@apackdev/ears`' builders compile to elmdb's plans and batches:

| Module | What it is |
|---|---|
| `src/database.ts` | the port to an elmdb `Database` and `Unit` (structural, so only `@apackdev/ears/lmdb` loads elmdb), the msgpack codec (msgpackr, `skipValues: [undefined]`), and the connection every read and write crosses through: to the unit `transact` holds, opened at its first write, or to the database; each refusal elmdb names is an `EarsError` |
| `src/query.ts` | the query builder, a recipe compiled to a plan per terminal. A `where` holding the caller's function splits the plan: the part before it answers the field's values, and the rest starts from the ids the function admits, one at a time for `first`, `exists` and `pickOne` behind steps that keep or walk ids one by one, so a predicate is called no more than the old engine called it |
| `src/write.ts` | the write builder: each call one batch operation, every attribute write of a builder stamped with the time of its first |
| `src/engine.ts` | `createEarsEngine({ database, warn?, freeze? })`, its names read from the database's schema; `admin.setSchema` and `admin.declare`, the second keeping what the database holds besides what it is given |
| `src/schema.ts` | `EngineSchema` (types with their row rules, relation kinds with theirs, roles) and the elmdb schema it becomes |
| `src/lmdb/database.ts` | `openDatabase`, `createTemporaryDatabases` (D10: a temporary directory each, syncs off) and `DATABASE_FORMAT` |
| `src/kv/` | the JavaScript engine, as `createKvEngine`, for the differential harness and the lmdb-js format conversions until Phase 10 |

**D9.** `link`'s `replace` and `acyclicIn` are gone; `apack.json`'s `data.relations` is a map of declarations,
`{ "references": { "unique": true } }`, `{}` for a kind with none, which codegen carries into the pack's registration
(`ears.relationRules`) as it carries row rules. The SDK's `parent_of` is acyclic. Every in-repo caller, prompt and
example is migrated: default-setup's `references` is unique, and its query-editor example and transaction prompt name
the declarations.

**Composition.** `openDatabaseStore` opens the elmdb database and declares the registered packs' schema, and again on
each registration change (`onRegistrationsChanged`, a new registry hook), so a pack registering changes nothing another
pack declared. `openAppDatabase` (`apack db`) holds a tool's writes to the schema the app last declared, which carries
the packs' required fields no manifest does, and refuses a writable open of a database that holds none. The host
services, backup (elmdb's compacted copy and `replaceWith`), `appData.reset` and the SDK's test runtime (each reset a
new temporary database) run on elmdb.

**elmdb changes Phase 9 needed** (each with its specs):

- `Database::set_schema`: a schema change on an open database, as opening would make it, under a lock every write holds
  (`5075d6f`)
- `Load::Unset`: a field removed unchecked, for `admin.deleteAttr` (`5075d6f`)
- a query read's `visits`, answering how many entities its walk reached, for apack's unindexed-walk warning; a value
  outside the spec keeps `VALUE_INVALID`; a missing link end's message names what each operation names (`9aec6d9`)
- a type's walk streams through its range, and a request is checked against the value spec in one pass that allocates
  nothing (`836f432`): `query(type).first()` over 40,000 rows read every id of the type before visiting the first

## The differential harness

`packages/apack-ears/tests/engine/differential.spec.ts`: one random workload (the model harness's, without the store
refusing a commit, which only the JavaScript engine's backend can inject; elmdb's own model harness covers refusals)
drives the JavaScript engine on its memory store, the elmdb engine and the reference model. After every op both engines
refuse what the model refuses, hold what it holds, answer every derived read as it does, and agree with each other:
**120 runs pass**. Mutating the elmdb write builder's `unlink` to drop its target is caught.

## Behaviour that changed

- **A stored field holding `undefined` is absent**, as apack's `sameValue` already said; the memory store kept the key.
- **A number that isn't finite is refused** (`VALUE_INVALID`), as elmdb's value spec holds; apack's stores kept `NaN`.
- **A `delete`, `removeAt` or `removeValue` that changes nothing doesn't stamp `updatedAt`**; a chain of a builder's
  writes is stamped once, with its first write's time, as before.
- **A tool's write is held to the app's rules** (required fields included), which a tool reading manifests didn't know.
- **A reset closes the database the replaced engine was on**: the memory engine answered after its replacement.

## Readings

D17's reading, `npm run bench -w @apackdev/ears` on the built package (means, ms), 2026-10-25, the machine idle by
`npm run check:idle`: before Phase 9, the JavaScript engine on lmdb-js; after it, the engine on elmdb, with the
streaming walk (`836f432`) and POSIX semaphores (`2cbcc6c`).

| Case | lmdb-js | elmdb | Change |
|---|---:|---:|---:|
| bulk load 50k entities, 200k attributes, 100k relations | 2,754.67 | 1,272.63 | −54% |
| query by type with where and pickAll | 121.40 | 39.08 | −68% |
| 1,000 where lookups by a unique value | 6.96 | 8.13 | **+17%** |
| 10 where lookups on an unindexed field | 645.94 | 254.20 | −61% |
| 100 withRole lookups | 115.73 | 33.49 | −71% |
| 1,000 query.ids(id) lookups | 1.47 | 4.76 | **+224%** |
| query by type with 3 chained steps | 69.95 | 29.81 | −57% |
| relation traversal (a project: its tasks, then their assignees) | 0.118 | 0.023 | −80% |
| orderBy a string over 40k | 71.89 | 34.69 | −52% |
| 1,000 query(type).first() over 40k | 4.11 | 5.21 | **+27%** |
| 1,000 getRow | 7.91 | 5.84 | −26% |
| pickAll over 40k | 316.35 | 91.37 | −71% |
| 100 linkedWith | 423.74 | 87.24 | −79% |
| link 10k tasks to one project, then destroy it | 737.42 | 992.96 | **+35%** |
| 1,000 creates | 111.30 | 114.43 | +3% |
| 1,000 short codes | 79.64 | 58.21 | −27% |
| transact: 1,000 creates as one unit | 22.83 | 24.55 | +8% |

The causes of the four past +10%:

- **`query.ids`, unique `where` and `first()`** answer one entity per call, so they cost what a call crossing to the
  engine costs: about 2.8 µs native, of which N-API is about 1.1 µs and opening a read transaction 0.64 µs (heed
  renews no transaction), where lmdb-js's engine answered from memory. A query reading many entities pays it once,
  which is why every walk is faster.
- **link 10k, then destroy**: not yet attributed. Phase 10 re-reads the bench (D17) with lmdb-js gone and profiles
  this case.

## Test speed

Recorded with Phase 10's chain, the first that passes whole in the worktree: `docs/evidence/phase-10.md`, Test
speed. link-then-destroy's +35% is attributed there too (D17's re-reading).
