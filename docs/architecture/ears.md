# EARS

EARS, the entity-attribute-relation system, is the fluent TypeScript API over an elmdb database: what a program holds
(`createEarsEngine({ database })`), its query and write builders, which compile to plans and batches (`engine.md`),
its watches, and its typed facades. It is the npm package `@elmdb/ears`, in `packages/ears`, beside the binding it
runs on. What the database does, its storage, indexes, the checks it holds a write to and its durability, is the rest
of this directory's; this document covers the API over it.

In this repository "the engine" is the Rust crate. `createEarsEngine` and `EarsEngine` keep the name EARS gave them
before it moved here, so below **an EARS engine** is what `createEarsEngine` returns, and "the engine" alone is still
the crate. The names are one of the decisions left for after the move (`../archive/roadmap/ears.md`, and `design.md`,
Vocabulary).

## The package

- **Two entries.** `@elmdb/ears` is the builders and their types: `createEarsEngine` and its types, the database
  port's types, `EngineSchema`, the pure helpers, `EARS`, `defineEars` and the typed facade types. It loads no native
  code and no Node module, so a browser bundle can hold its constants and helpers, and nothing it exports acts on an
  EARS engine it wasn't given. `@elmdb/ears/database` opens databases: `openDatabase`, `createTemporaryDatabases` and
  `DATABASE_FORMAT`; its source is `src/lmdb/`, and it is the one module that loads `elmdb`.
- **What crosses is typed by elmdb's generated protocol.** The root entry imports its types
  (`import type … from 'elmdb/protocol/…'`), which erase, and no value of `elmdb` and no Node module. Each op, step
  and terminal is written whole as a literal of the generated type, so a field elmdb renames fails the build where it
  is written; `src/lmdb/port.test-d.ts` holds each member of the port to the binding's answer, `Buffer` read as
  `Uint8Array`, and the error table in `database.ts` names every generated `ErrorCode`, mapped or passed through.
- **`elmdb` is a peer, at exactly the version EARS was built with** (`0.1.0`, the workspace's, which `cargo xtask
  check-versions` holds both manifests to), since one
  process holds one binding (`design.md`, principle 5) and a peer is installed once, beside its dependents. In this
  repository the workspace links it to `crates/elmdb-node`. `msgpackr`, which encodes what crosses to the database,
  is EARS' one dependency; the manifest names no tooling, which is the root `package.json`'s.
- **Relative imports name the `.ts` source**, and `tsc` (`rewriteRelativeImportExtensions`) writes `.js` in the
  emitted JavaScript. `sideEffects: false`, and no module does anything on import.
- **The build** (`scripts/build.mjs`, `npm run build:ears` at the root) emits `dist/` from `tsconfig.package.json`
  into a staging directory, rewrites the declarations' relative `.ts` imports to `.js` (which `tsc` rewrites in
  JavaScript only), refuses a bare import a shipped module makes of a package the manifest doesn't declare, checks
  every export target was built, and only then renames the staged tree over `dist`, so a reader never finds a
  half-built one. It runs on the TypeScript the root installs and supports TypeScript 5.7 and later as a consumer's
  compiler.
- **Whether `dist` is current** is `scripts/dist-current.mjs`'s answer. The build writes a stamp when it succeeds
  (the tree id of `packages/ears` at `HEAD`, whether `git status -- packages/ears` was clean, and the TypeScript
  version) and clears it before it touches `dist`; `node packages/ears/scripts/dist-current.mjs` exits 0 when the
  stamp matches the checkout now and 1, saying why, when it doesn't. A build of a tree with uncommitted changes is
  never current, since nothing names that tree. A program linking this checkout (apack does) asks it before using
  `dist`, and builds when the answer is no. `tests/dist-current.spec.ts` moves a checkout of its own each way away
  from its stamp.

## Module map (`src/`)

| File | What it holds |
|---|---|
| `index.ts` | The root entry |
| `engine.ts` | `createEarsEngine({ database, warn?, freeze?, callSites?, clock?, schedule? })` and the faces' types (`EarsEngine`, `Ears`, `EarsAdmin`). It composes the modules below over one connection, and reads the names it accepts off the database's schema, again after `admin.setSchema` or `admin.declare` |
| `database.ts` | The port (`EarsDatabase`, `EarsUnit`: what an EARS engine needs of the binding's `Database` and `Unit`), the codec (`msgpackr`, `undefined` skipped; a read decoded with maps where a row isn't plain objects, so a field named `__proto__` is a field) and `createConnection`, the one place a read or a write crosses: to the unit `transact` holds open, or to the database outside one. A refusal the database names comes back as an `EarsError` with the matching code |
| `query.ts` | `createQuery`: the query entry points (`query(type)`, `query.ids`, `query.all`, `query.withTrashed`, `query.trashed`) and the builder, a recipe each terminal compiles to one plan (`engine.md`, Reads: plans); `b64Encode`/`b64Decode` |
| `write.ts` | `createWrites`: `create`, `upsert`, `write` and the write builder, each call one of the batch's operations (`engine.md`, Writes: batches and units) |
| `schema.ts` | `EngineSchema` (each type's `RowRules`, each relation kind's `RelationRules`, the roles) and `toElmdbSchema`, the database schema it becomes: each field the rules type declared as its type, required and indexed fields declared, every other field allowed; a value index per indexed field, and the text (`search`), vector (`embedding`) and trigram (`substrings`) indexes, a field they read held to text or anything; `withDeclarations`, which `admin.declare` merges with |
| `errors.ts` | `EarsError` and its codes, `RESERVED_ATTRIBUTES`, which every attribute write refuses, and `RESERVED_ENTITY_TYPES`, the names an EARS engine's own ids are written with (`Relation-…`, `Sequence-…`), which a program's schema must not use as a type |
| `names.ts` | `createNameChecks`: the refusal of a relation kind (`UNKNOWN_RELATION_KIND`) or a role (`UNKNOWN_ROLE`) the schema doesn't declare, which links, role writes, reads and walks call |
| `ids.ts` | `newId`, `idSuffix`, `entityTypeOf` (the one reader of an id's type), `setField` (a field written as an own property, `__proto__` included), `getTimestamp` |
| `entities.ts`, `runtime.ts`, `typed.ts` | The typed contract (below): the `EARS` namespace (`Entity`, a name EARS declares none of, `RelKind = { Custom }` and `RoleKind = { Custom }`, its own for tooling and never in a program's facade), `BaseEntity`/`EntityShapes`/`ShapeOf`/`EntityNameArg`; `EARSRuntimeDeps`, `QueryBuilder`/`WriteBuilder`, `RowRules`; `defineEars` and the `Typed*` facade types |
| `watch.ts` | `createWatches`: `watch(read, observer, { label? })` over tracked reads (each run's reads made under the watch's token, `Connection.track`; each commit's `touched` tokens marking the watches to run again; a result equal to the last delivered not delivered, `sameValue`) and the scheduler that bounds re-runs under any write rate (`WATCH_SCHEDULE`, on an injectable clock): at most one run pending, a gap after each run of four times its cost and at least a frame, an 8 ms slice a flush with the leftovers first next time, and a `warn` naming a watch that keeps exceeding the slice |
| `calls.ts` | What the connection counts while `admin.calls` is started: each read, each write outside a unit, and each unit with its writes, with their time and the code that made them (the first stack frames outside EARS, as the creator's `CallSiteNaming` finds them), and a watch's runs. Off, it costs one check per call |
| `observation.ts` | What `admin.observe` hands an observer of each operation crossing to the database (`EarsOperation`, `EarsOperationResult`): its kind and size, never its parameters or values |
| `lmdb/` | `@elmdb/ears/database` (`database.ts`): `openDatabase({ path, schema?, readOnly?, syncs?, syncEachCommit?, syncIdleMs?, syncIntervalMs?, mapSizeBytes? })`, `createTemporaryDatabases()` (a database per test or tool run, in a temporary directory, syncs off, each removed when disposed or at exit) and `DATABASE_FORMAT`, the format a backup records |

**A call is a crossing, and a crossing costs about 3 µs.** Every read and write goes to the database through N-API,
and a read opens a read transaction (`../evidence/phase-9.md`, Readings). A query answering many entities pays that
once; a loop of one-entity reads pays it per call, so a loop over `getRow` is a query with `pickAll` waiting to be
written.

## Naming: a name carries the operation, structure carries the rest

Four facts describe an operation, and only one of them belongs in its name:

| fact | carried by | not by |
|---|---|---|
| **operation**: what it does | the name: `create`, `write`, `transact` | (nothing else) |
| **capability**: anyone, or the EARS engine's creator | the `admin` sub-object beside the operations | a second face with a name of its own |
| **binding**: which EARS engine | who you ask: `engine.create(...)`, or a program's bound one | a free function per operation, or a twin of one |
| **checkedness**: checked against a program's shapes | a program's own surface (apack's `untyped` prefix) | anything in this package |

**Give a new operation a name that says what it does, and nothing else.** A name that also says who may call it, which
EARS engine it acts on or whether it was checked is a fact welded into the wrong place, and the next one is welded
beside it (`design.md`, Conventions: a name never says who may call it or which database it acts on).

## Vocabulary of the API

**A member is held to rules it is classified by on its own, its type and its name, not to a list of members.** Name
it by the convention and it is checked the moment it compiles; break the convention and `tests/vocabulary.spec.ts`
names it. The only list there is the exceptions, each with its reason, and an exception that no longer excuses
anything fails as stale. What the surface *is* is reviewed in the API reports (`packages/ears/etc/*.api.md`).

Checked off the types (compile time, in `typecheck`):

- **`is*` answers a boolean; `find*` answers a list**, empty when nothing matches.
- **No member takes a boolean flag.** A direction or a mode is a name: `out`/`in`, `linkedTo`/`linkedFrom`,
  `trash`/`restore`.
- **Writes chain.** Every write builder member returns the builder, except what ends a chain: `destroy`, and `id` and
  `row`, which read the builder.

Checked off the names, on a real EARS engine:

- **Pairs are complete.** A negation has its positive (`unlinkedTo`/`linkedTo`, `unlink`/`link`), a `…To` its
  `…From`, an `out…` its `in…`, and `ancestors` its `descendants`.
- **Absence is `undefined`.** A `get*` asked about an id no entity has never answers `null`, on the EARS engine or its
  `admin` face; `null` is only ever a stored value.

Held by the types and the specs, which say more than a rule over names could:

- **One entity row**, `{ id, entityType, ...attributes }`, wherever a read returns one.
- **One relation row**, `{ id, kind, source, target, info }`, and `weight` for a weighted kind, wherever a read
  returns one; a match and a patch use its field names.
- **The subject comes first**: the entity, type or relation an operation is about is its first argument.

## An EARS engine

- **An instance.** `createEarsEngine({ database, warn, freeze, callSites, clock, schedule })` returns a new EARS engine over a
  database, which holds everything it knows: it keeps no copy of the data, so a read reads the database. Each module
  is a factory closing over the connection (`createQuery`, `createWrites`, `createNameChecks`), and no module keeps
  anything at module scope, not even a reference to an EARS engine. The database's schema says which names are types,
  relation kinds and roles, and what each type's rows are held to (`RowRules`): its `required` fields, which
  `create` refuses a row without and `delete` one of (`REQUIRED_FIELD`), and its `shortCode` prefix, which `create`
  numbers every row's `shortCode` with, `<prefix>-<n>`, once every check has passed, so a refused create takes no
  number. Whoever creates the EARS engine gives the database its schema (`openDatabase({ schema })`,
  `admin.setSchema`, `admin.declare`). `warn` hears what will be slow with more data, as a message and as data
  (`EarsWarning`): an unindexed `where` whose walk visited more than `UNINDEXED_WALK_WARNING` entities (the plan
  answers its `visits`), once per call site, and a watch that keeps costing more than a flush's slice; the data lets
  a creator say where its own declarations make the fix. `callSites` (`CallSiteNaming`) is how a counted call and an
  unlabelled watch name the code that made them: which stack frames are EARS' own, and where a path is cut. `clock`
  (`EarsClock`: `{ now(), after(ms, run) }`) injects wall time and monotonic timers across writes (`createdAt`,
  `updatedAt`, `trash`), generated ids, call tallies, omitted `recency.now` on `search`/`hybrid` reads, and watch
  scheduling (with `schedule` overriding `WATCH_SCHEDULE`), so deterministic specs run without wall-clock or real-timer
  dependence. `freeze`
  freezes what a read hands out, so code that mutates a read fails rather than appearing to work. The short code is
  EARS', like the id: `shortCode` is a reserved attribute, so no write sets or deletes it; a created row may bring one
  only as identity (`ShortCodeIdentity`, an import keeping its code), held to the type's `<prefix>-<n>`
  (`INVALID_SHORT_CODE`) and to no other entity holding it (`SHORT_CODE_IN_USE`), and an upsert's row may repeat the
  entity's own and no other. A code is unique, so `query(type).where('shortCode', code)` finds its entity, answered
  by the value index; which type a bare code belongs to is the program's answer, since the prefixes are its
  declarations. The admin face's loads take any row.
- **One argument, one meaning.** Each entry point makes one claim about its argument and is held to it, throwing an
  `EarsError` (`errors.ts`) with a code otherwise: `query(type)` and `query([types])` take declared types and refuse
  anything else, `undefined` included (`UNKNOWN_ENTITY_TYPE`); `query.ids(ids)` takes ids and drops absent ones;
  `query.all()` is every entity but the numbering sequences; `create(type, row)` takes the whole row in one object,
  its fields and its `id`, `createdAt` and `updatedAt`, which no field can be named, and refuses an undeclared type,
  an id in use (`ID_IN_USE`), an id naming another type (`TYPE_MISMATCH`) and any other of EARS' own fields, before
  the entity exists; `write(id)` refuses an id no entity has (`NO_SUCH_ENTITY`), a relation's included, which the
  compiler refuses there too (`WriteTarget`: an `EARS.RelationId` carries a marker `write`'s parameter can't take);
  a relation is read by `getRelation` and `findRelations`, changed by `updateRelation` and removed by
  `removeRelation`, each refusing an id no relation has (`NO_SUCH_RELATION`); `upsert(type, { id, ...row })` writes
  the row to the entity with its id or creates the entity with it; a link, and every read or walk that names a
  relation kind, refuse one the schema doesn't declare (`UNKNOWN_RELATION_KIND`), and `grant`, `revoke`,
  `grantExclusive` and `withRole` a role it doesn't (`UNKNOWN_ROLE`), since a read naming a name nothing declared
  would answer as if nothing held it. The admin face's direct writes and loads take any, which is how data whose
  declarations have gone is read. The untyped name parameters are `TypeName<E>` (`write.ts`), so a dashed literal or
  an `EntityId` passed as a type fails to compile.
- **One value per attribute.** An attribute holds exactly one value, and a list is one array value, so a read
  (`getRow`, `getAttr`, `pick`, `pickAll`, `pickOne`, a write's `row()`) gives back exactly what was written, a
  one-element list as a list. Roles are the `role` attribute's list. A value is encoded as it is written and decoded
  as it is read, so a value a read handed out is the reader's own copy. A value the value spec doesn't hold (a number
  that isn't finite, a map key that isn't a string) is refused (`VALUE_INVALID`), and a field holding `undefined` is
  absent. `where(k, v)` matches a value equal to `v` or a list holding it (`holds`); a predicate is given the whole
  value.
- **The write builder** (`WriteBuilder`, `runtime.ts`): `set`, `setMany`, `append`, `merge`, `delete`, `removeAt`,
  `removeValue`, `link(kind, target, { info, weight })`, `unlink(kind, target?)`, `unlinkAll`, `grant`, `revoke`,
  `grantExclusive(role, among)`, `trash(at?)`, `restore()`, `destroy()`, `id`. Every attribute write refuses
  `RESERVED_ATTRIBUTES` (`id`, `entityType`, `shortCode`, `createdAt`, `updatedAt`, `role`, `trashedAt`), which only
  EARS writes; `setMany` refuses before writing any field. `append`, `removeAt` and `removeValue` work on the
  attribute's list and refuse a value that isn't one (`NOT_A_LIST`); `merge` merges into an object value. `link`
  merges on the edge's identity: kind, target and the values of the kind's `key` fields in `info`
  (`RelationRules.key`; none, the ends alone), so linking an identity again updates that edge's info and weight and
  keeps its id and its place, and `updateRelation` onto another edge's identity is refused (`DUPLICATE_EDGE`). A
  kind's declaration holds the rest: an `acyclic` kind refuses a link that would close a cycle (`CYCLE`), and no kind
  links an entity to itself (`SELF_LINK`). A `weighted` kind's edge takes a `weight` (`RelationRules.weighted`): from
  0 to 2^960, 1 when none is given, and refused on any other kind (`VALUE_INVALID`). **A relation has two ends from when it
  is written** (`NO_SUCH_ENTITY`): `link` refuses a target that isn't there before writing anything, in a unit as
  outside one, so an entity is written before the links to it and a unit's commit has nothing left to refuse.
  `updateRelation` is held to the same rule for the ends it moves a relation to. A relation loaded from files can
  still have a missing end (`admin.addRelation` and `admin.updateRelation` take any), and the readers step around one.
- **EARS owns `updatedAt`.** `create` stamps it with the creation time (a row's own `updatedAt` keeps an import's),
  and a builder's attribute writes stamp it once per builder; links, roles, the trash and the admin's direct writes
  don't. `write(id, { touch: false })` writes without stamping it. `null` is a stored value; `setMany` leaves a field
  whose value is `undefined` alone, `set(k, undefined)` refuses (`NO_VALUE`), as do `append`, `merge` and
  `removeValue` given `undefined`, and `delete(k)` is the one way to remove a field.
- **One verb per job in the query builder.** A filter keeps the current set: `where`, `has`/`lacks`, `withRole`,
  `ofType`, `within(ids)`, and by an edge `linkedTo(kind, other?)`, `linkedFrom(kind, other?)`, their negations
  `unlinkedTo`/`unlinkedFrom`, and `linkedWith(other)` (any kind, either way). A walk moves to an edge's far end:
  `out(kinds, type?)`, `in(kinds, type?)` and `both(kinds, type?)`, or any number of edges deep:
  `descendants(kinds, type?, { depth? })` and `ancestors(kinds, type?, { depth? })`, nearest first, each entity once;
  `pathTo(to, kinds, { direction?, depth? })` is a shortest path. `outEdges`/`inEdges` return each edge as a relation
  row, and `degrees(kinds, type?, { direction? })` how many each entity has. The graph steps
  (`../evidence/graph-algorithms.md`): `rank`, `weakComponents`, `strongComponents`, `communities`, `topological` and
  `similar`. No step takes a boolean direction flag: `'out'`, `'in'` or `'both'` (`EdgeDirection`). **A builder is a
  recipe**: chaining a step runs nothing, and each terminal compiles the whole query to one plan and sends it, so a
  builder kept across writes answers as one made after them. A `where` given a function can't cross: the plan is
  split there, the part before it answering the field's values and the rest starting from the ids the function
  admits. `explain()` is the terminal that says what the plan did: the query's ids, with each stage the database ran
  (`QueryExplanation`); a query with a `where` given a function is refused (`NOT_EXPLAINABLE`).
- **Rank indexes.** `EngineSchema.ranks` declares PageRank over a graph of types and kinds, which the database keeps
  current by every commit, in the background past each commit's budget. `ranked(index)` is a plan step like `rank`.
  A read answers scores within a bound, which `rankedHits()` answers beside them; a test that needs them settled calls
  `admin.waitForIndexBuilds()` first. `whileBuilding: 'refuse'` throws `INDEX_BUILDING` rather than answer from a
  `rank` while the index builds.
- **Search.** A type whose row rules declare `search` (`{ fields: { field: weight }, stem? }`) has a text index named
  `search` over those fields. `search(text, options?)` is a plan step ordering by relevance (`search.md`);
  `after(id)` pages; `hits({ fields?, highlights? })` is its terminal, refused (`NO_SEARCH`) for a query with no
  search or with a `where` given a function after it. A type with no text index refuses a search (`UNKNOWN_INDEX`).
- **Vector search.** A type whose row rules declare `embed` (`{ from, chunk?, model, dims }`) has a vector index named
  `embedding`, cosine over f32. A write to its fields leaves the entity's changed chunks pending;
  `pendingEmbeddings(type)` answers them, and `embed(embeddings)` writes vectors, counting `stale` each whose chunk
  changed since. `near`, `like` and `hybrid` are plan steps that `hits()` ends. Neither EARS nor the database calls a
  model: the program that computes the vectors does.
- **Substrings and patterns.** A type whose row rules declare `substrings` has a trigram index named `substrings`.
  `containing(text, { caseInsensitive? })` and `matching(pattern)` filter without ranking (`search.md`, Substrings and
  patterns as built); a type with none refuses them (`UNKNOWN_INDEX`).
- **Watches.** `watch(read, observer)` runs `read` and delivers its result, then runs it again after every commit that
  wrote into what it read, delivering each result that differs from the last it delivered, with the commit's sequence
  (`lastCommit()`). A result differs unless it is the same object or `sameValue` finds it equal: arrays and plain
  objects by their contents, a plain object's being its own enumerable string keys, so a change only to a symbol-keyed
  or non-enumerable property isn't delivered; a map by the values under its keys and a set by its members, keys and
  members matched by identity, so a map or set keyed by objects each run makes afresh differs every time; and dates,
  regular expressions, URLs and bytes by their value. An instance of a class extending any of these (a Node `Buffer`,
  say) is compared as what it extends. Any other object, such as an instance of a class extending none of them or a
  plain object from another realm, differs unless it is the same object, since what it holds may not be its own
  enumerable keys: a watch whose result holds a new one is delivered after every commit that wrote into what it read.
  Tracking is the database's (`engine.md`, Change sets and watch): each run drops what the watch's token held and makes
  its reads tracked under it, and a commit's change set names the tokens it wrote into, so a write's cost doesn't grow
  with the watches open. **However fast anything commits, a watch's cost is bounded by the scheduler, not by its
  writers** (G14 of `../archive/goals/goal-elmdb-next.md`): a watch has at most one run pending and the states between
  are skipped; after a run it waits four times that run's cost, and at least a frame; a watch idle when a commit lands
  runs at once; and a flush starts no run past an 8 ms slice. A commit the connection can't name the touched tokens of
  (`admin.clear`, `admin.replaceWith`, `admin.setSchema`, `admin.declare`, a load) is heard as touching every watch.
  Deliveries never come inside a write: re-runs wait for a microtask, so a burst of commits is one. A unit that throws
  notifies nothing. A read that throws delivers to `observer.error` and stays watched. **A watched read is a pure read
  of the EARS engine**: what it reads elsewhere isn't tracked. A watch's own `caughtUp()` resolves once that watch has
  been handed everything this engine committed before the call (another process's commit is heard later), by the run it
  is owed, or by its ending: it waits for the one watch, so it resolves under a stream of commits that keeps others
  behind. `caughtUp({ soon: true })` says something is blocked on the answer: the run that watch is owed then waits four
  times its last run's cost and not the frame (a millisecond at least, a timer's grain), ahead of the others in its
  flush, and the run after it has its whole gap again. It lifts nothing for a watch that is owed no run, nor for any
  other watch. `admin.watchesSettled()` resolves once no watch at all is waiting to run, a moment such a stream may not
  leave, so it is for a spec or a tool that has stopped writing. `admin.watchStats()` counts the watches and what the
  database has filed, and `admin.watchDiagnostics()` returns a payload-free snapshot of the scheduler's queue/timers
  and every open watch's label, owed/soon/running flags, cost, due time, waiter count and last delivered commit. With
  `recordCauses`, each run's `WatchReadContext` carries bounded invalidating commit `sequences` (`dropped` past the
  cap), their `sources` (`'local'`, `'builder'`, `'elsewhere'`) and whether any was a `wildcard` invalidation. The model-based harness checks the contract: every watch, after every op, holds what its read
  answers afresh.
- **One row shape.** A row is `{ id, entityType, ...attributes }` (`EntityRow`) wherever a read returns one.
- **One relation shape.** A relation is `{ id, kind, source, target, info }` (`RelationRow`), with `weight` for a
  weighted kind, wherever a read returns one.
- **Declared indexes.** A type indexes the fields its row rules declare (`RowRules.indexes`), each a value index of one
  field, and every type indexes `role` and `shortCode`. A `where(k, v)` or `withRole` on a type start reads the index
  where the type declares the field; where it doesn't, it filters the type's rows, finding the same matches, and warns
  through `warn` past `UNINDEXED_WALK_WARNING` visits.
- **The trash is EARS'.** `write(id).trash(at?)` writes `trashedAt` (kept the first time), and the entity leaves its
  type's live entities in the same unit. No query reaches a trashed entity; `query.withTrashed` is the same entry
  points over the trash too, and `query.trashed` those starting from only the trashed entities. A write reaches a
  trashed entity, so `restore()` works, and the numbering counts it, so a trashed row's short code is never handed out
  again.
- **Operations, and `admin` beside them; holding the EARS engine is the capability.** The members of `Ears`:
  `query`, `create`, `upsert`, `write`, `transact`, attribute and role reads, the relation operations and
  `wouldCreateCycle`. `engine.admin` (`EarsAdmin`): `clear`, the schema (`setSchema`, and `declare`, which keeps every
  declaration it isn't given), the loads an import makes (`bulkLoad`, one unit in which `bulkLoadAttr` and
  `bulkLoadRelation` skip the checks a write makes), `countEntities`, unchecked direct writes (`putAttr`,
  `deleteAttr`, `addRelation`, `updateRelation`) and the raw listings (`getAllEntities`, `getEntitiesOfType`). A
  program hands code the EARS engine typed as `Ears`, which has no `admin`, and keeps `admin` for its creator.
- **Binding is a program's.** This package has no free function that acts on an EARS engine: an operation is a member
  of the one you hold. A program that wants operations over one it binds (apack's SDK does) writes them over that.
- **Units.** `transact(fn, { durable? })` runs `fn` as one unit of the database, opened at its first write, so a
  reader sees all of it or none and a unit that writes nothing commits nothing. Writes inside it read their own
  writes. If `fn` throws, the unit is aborted and keeps nothing; if it returns, it commits. A unit inside another is a
  savepoint, and a nested `{ durable: true }` propagates durability to the outermost unit when it returns (discarded if
  the nested unit throws and rolls back). `durable: true` commits with a sync rather than leaving it to group commit;
  `admin.durable()` waits for the database's group commit to sync pending commits, and `admin.transactWithoutHook(fn,
  { durable? })` takes the same unit options. An `async fn` is refused,
  since an `await` would split the unit. `admin.clear()` inside a unit is refused too. Outside a unit each write is its
  own unit, committed as it returns.
- **Typed facades.** `defineEars<Schema>(engine)` returns `query`, `create`, `upsert`, `write`, `transact`, `newId`
  and `getAttr` typed against a schema (`EarsSchema`: its entity shapes, and the names, relation kinds and roles they
  take; `UntypedSchema` checks nothing), each acting on the EARS engine the `engine` getter returns when called, so a
  facade can be defined before its EARS engine exists.
- The trust model: code handed an EARS engine runs in-process (`design.md`, Not in scope). The faces define the
  contract, not a security boundary.

## Persistence

- **The database is the binding's.** `openDatabase({ path, schema?, readOnly?, syncs?, syncEachCommit?, syncIdleMs?, syncIntervalMs?, mapSizeBytes? })` (`@elmdb/ears/database`)
  opens the database at `path`, giving it `schema` (an `EngineSchema`; a schema that differs from the one it holds
  changes it, as `setSchema` would) or keeping the one it holds. `readOnly` opens existing files without writing.
  Nothing opens on import.
- **A unit is visible as it commits, and durable within the window** (`engine.md`, Durability). `syncs: false`
  disables periodic group syncing, for temporary databases.
- **`NOT_DURABLE` means committed.** A durable unit or admin operation whose sync fails leaves its committed data in
  place; the connection notifies watches and refreshes the schema's declarations before rethrowing it. A caller must
  not retry it as an uncommitted refusal.
- **`SCHEMA_CHANGED` is an uncommitted refusal.** Another process changed the stored schema after this handle opened:
  close the database, reopen it and create a new EARS engine to use the stored schema.
- **A backup is the database's compacted copy** (`backup`), and a restore its `replaceWith`. `DATABASE_FORMAT`
  (`elmdb/1`) is what a backup records of where its files came from; opening them is what decides whether they are
  readable (`format.md`, Versions).

## Typed contract

`entities.ts`, `runtime.ts` and `typed.ts` are the typed contract: editor completions and error messages depend on
their exact form, so a change to them is checked by the typed specs below as well as by `typecheck`. apack's SDK
aliases these types and adds its own entities; its codegen and its checks over built programs
(`facade-typing.integration.spec.ts`) are apack's, and meet a change here when its pin moves.

## Guards

- `tests/no-module-state.spec.ts`: no module-level `new Map`/`Set`/`WeakMap`/`WeakSet`/`Array`, `[]`, `{}` or `let` in
  `src/`, with no exception.
- The typed contract's own specs, over a schema of two made-up types (`tests/_support/typed-schema.ts`):
  `tests/typed/typed-query-builder.spec.ts` and `branded-entity-id.spec.ts` (type assertions, failed by
  `typecheck`), and `tests/typed/completions.integration.spec.ts` (a language service's completions and typo errors,
  over the source and the packed package).
- `tests/published-package.integration.spec.ts` packs the built package into a consumer with `elmdb` beside it and no
  Node types: what it ships, its exports compiled on TypeScript 5.7 and the current one, each loaded by Node, and no
  `any` in any export.
- The API reports, `packages/ears/etc/index.api.md` and `database.api.md`, from API Extractor over the declarations:
  `npm run api:check -w @elmdb/ears` fails when one is out of date, and `npm run api:update -w @elmdb/ears` writes
  them. Review their diff with any export change.

## Tests (`packages/ears/tests/`)

- `engine/`: black-box specs of the public API: `writes`, `relations`, `queries`, `transact`, `durability`, `clock`,
  `bulk-load`, `search`, `vector`, `rank-index`, `checkpoints`, `schema-catalog`, `calls` and `observation`. They reach an EARS engine only
  through `engine-under-test.ts` (`freshEngine`), each on a temporary database of its own. A few specs pin existing
  quirks; they say so.
- The correctness harness (`_harness/`: a reference model and random workloads from `fast-check`):
  `engine/model-based.spec.ts` checks an EARS engine against the model after every op: the rows, every refusal, a
  unit and a savepoint rolling back, and the derived reads, with a watch on each of those reads, every row and the
  whole state, which delivers what the read answers afresh after every op. `engine/watch.spec.ts`,
  `watch-schedule.spec.ts` and `watch-causes.spec.ts` hold the watch contract's edges, and
  `writers-converge.spec.ts` a watch here ending up showing what its read answers afresh while a second process,
  killed part way or not, commits a random history into the same database. A commit the database refuses, and a
  process killed mid-write, are the engine's own harnesses' (`testing.md`).
- `vocabulary.spec.ts`: the rules in Vocabulary of the API, each with a case showing it names a member that breaks
  it. `engines.spec.ts`: a typed facade acts on whichever EARS engine its getter returns; two in one process share
  nothing.
- Two halves. The fast half (`vitest.config.ts`) is everything but `*.integration.spec.ts`; the integration half
  (`vitest.integration.config.ts`, which extends it) packs the built package, so it needs `dist` built first, and is
  given TypeScript 5.7 as `EARS_TYPESCRIPT_FLOOR`, from `packages/typescript-floor`, a private workspace whose
  only dependency is that compiler, outside `packages/ears` so that npm installs it in its own `node_modules` and
  neither the workspace's `tsc` nor the `typescript` EARS resolves is it. Test files run in parallel: each
  creates its own EARS engines. A test or hook is bounded by a minute's clock.

## Benchmarks

`bench/ears.bench.ts` loads an app-sized graph (50k entities, 200k attributes, 100k relations) through the admin
face's bulk load, on a database in a temporary directory synced as an app's is, then times the reads and writes a
program makes most. It times the build in `dist`. `bench/text.bench.ts` times search and its ranking. `bench/heap.ts`
reads the heap with the same graph loaded, then what opening its database again costs. `bench/watch-stress.ts` is
G14's stress reading: a writer committing on every turn into 20,000 tasks under three views, printing commits a
second, the share of a CPU, the event loop's delay and how late each view hears a commit (`none`: the writer alone).
The readings are in `../evidence/phase-9.md` and `../evidence/next-phase-3.md`, each dated with its conditions.

## Commands

From the repository's root, after `npm ci`:

```bash
npm run build                      # the binding, then EARS' dist (npm run build:ears for EARS alone)
npm run typecheck                  # the binding's declarations, then EARS' src, tests and bench
npm test                           # the binding's node --test, then EARS' fast half and integration half
npm run api:check -w @elmdb/ears   # the API reports; api:update writes them
npm run bench -w @elmdb/ears       # the benchmarks, over dist (bench:heap, and bench:watch [-- <seconds> [none]])
node packages/ears/scripts/dist-current.mjs   # whether dist is the build of the checkout as it is now
```
