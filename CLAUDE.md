# @apack/ears

EARS, the entity-attribute-relation graph store behind apack's data: the engine, the core `EARS` types, the typed facades, the persistence port, and the LMDB store (`@apack/ears/lmdb`). It's the bottom layer: it imports no `@apack/*` package and knows no packs and no SDK entity names (those are the SDK's, in `@apack/sdk/types`). The SDK, host, api, packs and tests import it. The root `CLAUDE.md` ("SDK packages", "Data layer") has the one-paragraph view; this file covers how the package works.

## Package basics

- Published like `@apack/sdk`: each export in its `package.json` resolves `src/` under the `@apack/source` condition and `dist/` otherwise, and the build stages a derived manifest with no source branches under `publish/`, which is what npm publishes. Exports: `.` (the engine, types and persistence port) and `./lmdb` (the LMDB store). It's in the changesets fixed group (`.changeset/config.json`) and supports TypeScript 5.7 and later (`packages/typescript-floor`).
- `lmdb` is an optional peer dependency (a dev dependency here). Only `src/lmdb/` loads it; the app installs it (`packages/api` depends on it).
- Relative imports name the `.ts` source; `tsc` (`rewriteRelativeImportExtensions`) writes `.js`. `sideEffects: false`, and no module does anything on import.
- It's a shared-instance package (`SHARED_INSTANCE_PACKAGES` in `@apack/host/build/shared-deps`): packs, dependency runtimes, the app and tests load one copy, so the installed engine is the same everywhere. The pack loader's bridge and the harness bridge provide every export except `./lmdb` (`APP_ONLY_EXPORTS`). Pack frontends don't share it: they inline the constants and helpers they import.

## Module map (`src/`)

| File | What it holds |
|---|---|
| `index.ts` | The root entry: `createEarsEngine`, `installEngine`/`installedEngine`, the free functions, the persistence port, `EARS`, the typed facades |
| `engine.ts` | `createEarsEngine({ persistence?, isEntityType })` and the faces' types (`EarsEngine`, `EarsQuery`, `EarsAdmin`). It composes the factories below |
| `installed.ts` | The installed engine: `installEngine(query)` (returns the one it replaced; `undefined` uninstalls), `installedEngine()` (throws `NO_ENGINE_INSTALLED`, naming `bindHost`, `startTestRuntime` and `installEngine`) |
| `attribute-storage.ts` | `createAttributeStorage`: attributes, the entity index, roles, and every write's call to the persistence sink. The free functions exported from here (`getAll`, `grantRole`, `destroyEntity`, …) read the installed engine |
| `relation-index.ts`, `edge-store.ts`, `relations.ts` | The relation index by kind, relation writes over it (`createEdgeStore`), and relation reads (`findRelations`, `getRelationStats`) |
| `query.ts` | `createQx`: the query builder (`qx`, exported untyped as `untypedQx`), `b64Encode`/`b64Decode` |
| `transaction.ts`, `transaction-helpers.ts` | `createTx`: the transaction builder (`tx`); `createEntityWithDefaults`, `updateEntity`, `createRelation` |
| `query-helpers.ts`, `entity-utils.ts` | The finders (`findById`, `findWhere`, …), counters and label helpers |
| `graph.ts`, `blueprint.ts` | Graph walks (`descendants`, `topoSort`, `wouldCreateCycle`, `linkSymmetric`, …) and blueprints (`bp`, `spawn`) |
| `repository.ts`, `repository-errors.ts` | The repository registry (`repository`, `registerRepository`), `RepositoryError`/`RepositoryErrorCode` |
| `entities.ts`, `runtime.ts`, `typed.ts` | The typed contract (change-controlled, see below): the core `EARS` namespace (`Entity = { Relation }`, `RelKind = { Custom }`), `BaseEntity`/`EntityShapes`/`ShapeOf`/`EntityNameArg`; `PersistenceSink`, `noopSink`, `EARSRuntimeDeps`, `QueryBuilder`/`TransactionBuilder`, `isEntityType`; `defineEars` and the `Typed*` facade types |
| `persistence/policy.ts`, `persistence/sharded-router.ts` | The persistence port: `Partition`, `PartitionPolicy`, `makePolicy`; `makeShardedPersistence`, the sink routing each write to its partition's sink |
| `lmdb/` | `@apack/ears/lmdb`: `store.ts` (`openLmdbStore`), `envs.ts` (environments), `adapter.ts` (`makeLmdbAdapter`, the LMDB sink), `hydrate.ts` (`hydrateSharded`), `query.ts` (`LmdbQuery`, direct reads without hydrating) |

**A range is a live cursor, and an environment has 126 reader slots.** `getRange`/`getKeys` hold a read
transaction until the iterator is closed, and the language closes it for you — `for…of` (break or return
included), array destructuring and `.find()` all call `return()`. Taking the iterator by hand
(`it[Symbol.iterator]().next()`) does not, and each call that finds a row leaks a slot: measured, the 127th
throws `MDB_READERS_FULL`, after which every read fails with "No transaction to renew" and every write with
`EINVAL`, for the life of the process. `getAttrLength` did exactly that. Where the body *removes* the keys it
is walking, materialise first (`[...]`) — `removeAttrRange` is the one copy of that rule.

**A reverse range starts at the high bound.** `{ start: prefix, end: prefix + '\xFF', reverse: true }` walks
away from the data and yields nothing; `getAttrLength` returned 0 for every array it was ever asked about
because of it, and had no caller to notice.

**A failed write is dropped, not retried — but only the row that failed.** `makeLmdbAdapter` takes the
pending writes as separate steps and runs them in one transaction; if that throws it retries them one at a
time, so a single bad row no longer aborts and discards every unrelated write buffered in the same microtask.
Each dropped row is reported through `onWriteFailure` (`openLmdbStore`'s port) with the key it was for, and
`errorCount` counts rows, not batches. The app binds the port; the engine has no opinion about what to do.
| `utils.ts` | Private helpers (`isPlainObject`, the id suffix) |

## The engine

- **An instance.** `createEarsEngine({ persistence, isEntityType })` returns a new, empty engine that owns its stores, indexes, caches and repository registry. Each module is a factory closing over its state (`createAttributeStorage`, `createRelationIndex`, `createQx`, `createTx`, …), and `engine.ts` passes each its dependencies (the sink, the relation index, the entity-type checker). No module keeps data at module scope; the one module-level variable is the installed engine, a reference. `persistence` defaults to `noopSink`. `isEntityType` tells an entity type from an id (`tx('Note')` creates a Note). A name it doesn't know is taken for an id, so `qx('Memo')` on an engine without that type finds nothing instead of throwing, and `tx('Memo')` writes to an entity with that id rather than creating one. That's deliberate — an unknown name could equally be a typo, an id with no type prefix, or a type whose pack isn't installed, and the engine can't tell which — and `tests/engine/queries.spec.ts` pins both sides of it. A pack's typed `#generated/ears` rejects an undeclared literal at compile time; a name typed `string` reaches this rule.
- **Two faces; holding the engine is the capability.**
  - `engine.query` (`EarsQuery`): `qx`, `tx`, the finders, attribute and role reads, relation reads, graph walks, `spawn`, and the repository registry. Packs reach it only through the installed engine.
  - `engine.admin` (`EarsAdmin`): `clear`, `bulkLoadAttr`, direct attribute and relation writes (`putAttr`, `addRelation`, …), the relation index and its writes, `edgeStore`, `queryEntitiesByRole`, the entity-type checker and `repositories()`. Only the creator holds it: the api's composition root (hydration through `/lmdb`, and `createHostRuntime`'s `appData` reset, backup and restore), the CLI's flow compiler and decompiler (a private engine per compile), and `@apack/sdk/testing`.
- **Installation.** The free functions (`untypedQx`, `untypedTx`, `find*` through the facades, `repository`, `registerRepository`, `spawn`, `findRelations`, `isEntityType`, …) act on `installedEngine()`. `bindHost` (`@apack/sdk/runtime`) installs the app's `HostRuntime.ears`; `startTestRuntime` (`@apack/sdk/testing`) creates and installs a test engine and `resetTestData()` installs a fresh one, carrying the registered repositories over; tooling installs its own (`installEngine(createEarsEngine({ isEntityType }).query)`) or passes one (`exportFlowsToDSL(dir, { engine })`) and puts the previous one back. With none installed, a use throws `NO_ENGINE_INSTALLED`. There's no default engine.
- **Repositories.** The registry is the engine's (`query.repository`, `query.registerRepository`). A pack's repositories arrive in its registration (`PackRegistration.repositories`), and the host registry's `registerPack` registers them with the installed engine; pack code reads them through `#generated/repository` or `services.repository`, the same object. Reading an unregistered name throws.
- **Typed facades.** `defineEars<Shapes, Names>()` returns `qx`, `tx`, `find*`, `createEntityWithDefaults`, `updateEntity` and `getAttr` typed against a shape map, each acting on the installed engine. Packs don't call it: `apack generate-entries` writes `#generated/ears` with it, over the pack's shapes, its dependencies' and the SDK's.
- The trust model: packs run in-process. The faces define the contract, not a security boundary.

## Persistence

- `PersistenceSink` is the port: the engine calls it on every create, destroy, attribute write and relation write (`onCreateEntity`, with the type when `tx(type)` created it; `onPutAttrArray`, an attribute's whole value list after each write; `onDropAttr`, `onAddRelation`, …). Every method is required: a sink that stores nothing implements it as a no-op (`noopSink`). Bulk loads (`admin.bulkLoadAttr`, `admin.addToIndex`) don't. A destroy comes after the entity's relations were removed (`onRemoveRelation` each), so the LMDB sink deletes only the entity's row and its attributes, reading them per attribute kind rather than scanning the store.
- `makePolicy({ excludedEntityTypes })` says which partition an entity or relation lives in (`primary`, or `volatileBackup` for excluded types and relations touching them). Deleting an entity removes its links, and the engine tells `onRemoveRelation` which entity is being deleted (`destroyed`); an unlink passes nothing. The router uses that to keep run history: a run record (TNode) is saved in the trace partition with its link to the node it ran, and deleting that node (saved in the main partition) removes the link from memory but keeps it in the trace partition. The rule is: a link saved in a different partition from the deleted entity stays. Deleting the run record, or unlinking, removes the link. `makeShardedPersistence` routes each call to its partition's sink. The host's policy is a constant (`appPartitionPolicy()`, `@apack/host/database`): the SDK's volatile types and nothing else, since no pack can ask for a partition.
- `openLmdbStore({ paths, policy, engine, readOnly?, log? })` (`@apack/ears/lmdb`) opens both partitions' environments and returns the store: `sink` (the sharded LMDB sink, which the engine is created with), `envs`, `paths`, `readOnly`, `isOpen()`, `hydrate(options?)` (loads the hydrated partitions into `engine()`, the admin face), `query(partition)` (`LmdbQuery`), `copyTo(partition, targetDir)` (LMDB's own copy of one read transaction into `targetDir/data.mdb`, which is what a backup takes, so a copy made while the app writes is one moment of the database rather than a file read caught mid-commit), `close()` (which returns the writes that failed, its final flush's and those of environments closed by `reopen()`/`reset()` since the last report included), `reopen()` and `reset()` (deletes the files and opens them empty; the engine's memory is the caller's to clear, and writes the engine makes while the reset has the store closed are held and written to the new files). A store closed any other way (`close()`, and `reopen()` while it swaps files) drops the writes it gets. `engine` is a function because the engine is created after the store, with its sink. Nothing opens on import. `readOnly` opens existing environments without writing: LMDB opens them read-only, the sink throws on a write, and `reset()` throws; hydration still fills the sink's relation cache. `log` takes the progress lines (hydration counts, closing), which go to `console.log` by default.
- **Storage format.** `LMDB_FORMAT_VERSION` (`envs.ts`) is how this version encodes rows. Every environment records it in its own `meta` database, and `openEnvAt` checks it: files that record another format are refused, naming it and, when the format is newer, saying to update apack; files that record none (written before this was added, which is format 1, and which have no `meta` database at all) are stamped by the next writable open and never by a read-only one. Recording it is best effort: a write that fails (a full disk) is logged, never a reason to refuse a database that reads fine. The environment is closed again when the check refuses it, and a refused run history (`openShardedEnvs`) says that deleting that partition costs only the run history. Change the encoding and this number goes up, together with the code that upgrades the older format.
- The api's composition (`openAppStore()`, `packages/api/src/runtime/index.ts`) is the one production caller: it opens the store, creates the engine with `store.sink` and binds the app. Host code (`@apack/host/services`, `/backup`) takes the store and the admin face as arguments.

## Typed contract

`entities.ts`, `runtime.ts` and `typed.ts` are part of the typed EARS contract with the SDK's `types/entities.ts` and `types/sdk-entities.ts` and the generated `PackShapes`/`EntityName`. Editor completions and error messages depend on their exact form. Read `packages/apack-sdk/TYPED-EARS.md` and follow its checklist before changing them. The SDK's `EARS` namespace aliases this package's types and adds the SDK's entities and relation kinds.

## Guards

- `check:specifiers` (`scripts/check-import-specifiers.ts`): `findUpwardImports` (this package imports no `@apack/*`, in sources, tests, scripts and its `package.json`), `findLmdbImports` (only `src/lmdb/` imports `lmdb`; host, api, packs and pack tests never import `lmdb`, and packs never import `@apack/ears/lmdb`), `findSharedPackageLists` (the shared-instance list's consumers derive from `SHARED_INSTANCE_PACKAGES`), and the pack-source rule rejecting `registerRepository` from `@apack/ears`.
- `tests/no-module-state.spec.ts`: no module-level `new Map`/`Set`/`WeakMap`/`WeakSet`/`Array`, `[]`, `{}` or `let` in `src/`, except the installed engine.
- The typed-EARS specs live in default-setup (`typed-query-builder`, `branded-entity-id`, `entity-shape-registry`, `sdk-type-safety`) and in the CLI (`facade-typing.integration.spec.ts`, against workspace source and the published packages); `published-exports.integration.spec.ts` and `published-sdk-types.integration.spec.ts` cover the packed package.

## Tests (`tests/`)

- `contract/`: black-box specs of the public API, written before the engine became an instance (changed since only where a pinned result was a bug: `getSchemaStats` counts each kind's relations): `transactions` (`tx` create, update, delete, the ids and fields returned), `relations` (link, unlink, `linksTo`, `relatedTo`, `linkSymmetric`, cycle detection, roles), `queries` (query builder terminals and ordering, blueprints and `spawn`, repository registration) and `persistence` (the exact order and payload of sink calls, hydration through bulk load). They reach the engine only through `engine-under-test.ts` (`freshEngine`); `helpers.ts` has `recordingSink()`. A few specs pin existing quirks; they say so.
- `installed-engine.spec.ts`: the free functions throw with no engine installed; two engines in one process share nothing.
- `is-entity-type.spec.ts`: `isEntityType` follows the installed engine's checker.
- `lmdb/`: `persistence` (the LMDB adapter, the sharded router, `LmdbQuery`) and `store` (`openLmdbStore`: writes reach LMDB through `store.sink`, and a store opened again hydrates them).
- Test files run in parallel (`fileParallelism: true`): each creates its own engines.

## Benchmark

`bench/ears.bench.ts` hydrates an app-sized graph (50k entities, 200k attributes, 100k relations) into an engine's admin face, then times `qx` by type with a `where` and `pickAll`, a relation traversal, and `tx` batches of 1,000 creates. Run it with `npm run bench -w @apack/ears`. The baseline is recorded in `docs/goals/goal-package-boundaries.md` (Phase 6); the tolerance is +10% median per case, and anything beyond it needs investigating before an engine change lands.

## Scripts

- `npm run typecheck -w @apack/ears` (`npm run typecheck:ears` at the root): `tsc --noEmit` over `src`, `tests`, `bench` and `scripts`.
- `npm test -w @apack/ears` (part of `npm run test:unit`), `npm run bench -w @apack/ears`.
- `npm run build:package -w @apack/ears` (the repo's `scripts/build-package.ts`, shared with `@apack/sdk`, part of `npm run packages:build`): `dist/` from `tsconfig.package.json`, checking every export target was built. `npm run packages:check` runs publint and attw on it.
- `npm run api:check` / `npm run api:update` (from `packages/apack-ears`): the API reports `etc/index.api.md` and `etc/lmdb.api.md`. Review their diff with any export change; pack-facing exports have no `any`.
