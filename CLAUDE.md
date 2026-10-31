# @apackdev/ears

EARS, the entity-attribute-relation graph store behind apack's data: the engine, the core `EARS` types and the typed facades, over an [elmdb](https://github.com/spankyed/elmdb) database, which `@apackdev/ears/lmdb` opens. It's the bottom layer: it imports no `@apackdev/*` package and knows no packs and no SDK entity names (those are the SDK's, in `@apackdev/sdk/types`). The SDK, host, api, packs and tests import it. The root `CLAUDE.md` ("SDK packages", "Data layer") has the one-paragraph view; this file covers how the package works. What the database does — storage, its value indexes, the checks it holds a write to, durability — is elmdb's, and `elmdb/docs/` (`design.md`, `schema.md`, `engine.md`, `format.md`) is where it is written down; this package is the engine's API over it.

## Package basics

- Published like `@apackdev/sdk`: each export in its `package.json` resolves `src/` under the `@apackdev/source` condition and `dist/` otherwise, and the build stages a derived manifest with no source branches under `publish/`, which is what npm publishes. Exports: `.` (the engine and its types) and `./lmdb` (opening elmdb databases). It's in the changesets fixed group (`.changeset/config.json`) and supports TypeScript 5.7 and later (`packages/typescript-floor`).
- `elmdb` is a dependency, linked from the checkout `vendor/elmdb` points at (`npm run link:elmdb`, `vendor/elmdb.rev`); `msgpackr` encodes what crosses to it. Only `src/lmdb/` loads elmdb: the engine's port to a database is structural (`database.ts`), so the root entry never does.
- Relative imports name the `.ts` source; `tsc` (`rewriteRelativeImportExtensions`) writes `.js`. `sideEffects: false`, and no module does anything on import.
- It's a shared-instance package (`SHARED_INSTANCE_PACKAGES` in `@apackdev/host/build/shared-deps`): packs, dependency runtimes, the app and tests load one copy, so an engine created by one is usable by all. The pack loader's bridge and the harness bridge provide every export except `./lmdb` (`APP_ONLY_EXPORTS`). Pack frontends don't share it: they inline the constants and helpers they import.

## Module map (`src/`)

| File | What it holds |
|---|---|
| `index.ts` | The root entry: `createEarsEngine`, its types, the database port's types, `EngineSchema`, the pure helpers, `EARS`, `defineEars` and the typed facade types. Nothing it exports acts on an engine it wasn't given |
| `engine.ts` | `createEarsEngine({ database, warn?, freeze? })` and the faces' types (`EarsEngine`, `Ears`, `EarsAdmin`). It composes the modules below over one connection, and reads the names it accepts off the database's schema, again after `admin.setSchema` or `admin.declare` |
| `database.ts` | The port (`EarsDatabase`, `EarsUnit`: what the engine needs of elmdb's `Database` and `Unit`), the codec (`msgpackr`, `undefined` skipped; a read decoded with maps where a row isn't plain objects, so a field named `__proto__` is a field) and `createConnection`, the one place a read or a write crosses: to the unit `transact` holds open, or to the database outside one. A refusal elmdb names comes back as an `EarsError` with the matching code |
| `query.ts` | `createQuery`: the query entry points (`query(type)`, `query.ids`, `query.all`, `query.withTrashed`, `query.trashed`) and the builder, a recipe each terminal compiles to one of elmdb's plans (`elmdb/docs/architecture/engine.md`, Reads: plans); `b64Encode`/`b64Decode` |
| `write.ts` | `createWrites`: `create`, `upsert`, `write` and the write builder, each call one of elmdb's batch operations (`elmdb/docs/architecture/engine.md`, Writes: batches and units) |
| `schema.ts` | `EngineSchema` (each type's `RowRules`, each relation kind's `RelationRules`, the roles) and `toElmdbSchema`, the elmdb schema it becomes: each field the rules type declared as its type, required and indexed fields declared, every other field allowed; a value index per indexed field, and the text (`search`), vector (`embedding`) and trigram (`substrings`) indexes, a field they read held to text or anything; `withDeclarations`, which `admin.declare` merges with |
| `errors.ts` | `EarsError` and its codes, `RESERVED_ATTRIBUTES`, which every attribute write refuses, and `RESERVED_ENTITY_TYPES`, the names the engine's own ids are written with (`Relation-…`, `Sequence-…`), which the SDK's manifest schema refuses as a pack's entity type |
| `names.ts` | `createNameChecks`: the refusal of a relation kind (`UNKNOWN_RELATION_KIND`) or a role (`UNKNOWN_ROLE`) the schema doesn't declare, which links, role writes, reads and walks call |
| `ids.ts`, `utils.ts` | `newId`, `entityTypeOf` (the one reader of an id's type), `setField` (a field written as an own property, `__proto__` included); `isPlainObject`, `idSuffix`, `sameValue` |
| `entities.ts`, `runtime.ts`, `typed.ts` | The typed contract (change-controlled, see below): the core `EARS` namespace (`Entity`, a name the engine declares none of, `RelKind = { Custom }` and `RoleKind = { Custom }`, the engine's own for tooling and never in a pack's facade), `BaseEntity`/`EntityShapes`/`ShapeOf`/`EntityNameArg`; `EARSRuntimeDeps`, `QueryBuilder`/`WriteBuilder`, `RowRules`; `defineEars` and the `Typed*` facade types |
| `watch.ts` | `createWatches`: `watch(read, observer, { label? })` over elmdb's tracked reads — each run's reads made under the watch's token (`Connection.track`), each commit's `touched` tokens marking the watches to run again, and a result equal to the last delivered not delivered (`sameValue`) — and the scheduler that bounds re-runs under any write rate (`WATCH_SCHEDULE`, on an injectable clock): at most one run pending, a gap after each run of four times its cost and at least a frame, an 8 ms slice a flush with the leftovers first next time, and a `warn` naming a watch that keeps exceeding the slice |
| `calls.ts` | What the connection counts while `admin.calls` is started: each read, each write outside a unit, and each unit with its writes, with their time and the code that made them (the first stack frames outside the engine, a source build naming source files), and a watch's runs. Off, it costs one check per call. A development app hands both engines' counts to `HostRuntime.diagnostics`, which console code reads as `engineCalls` (`@apackdev/sdk/database-console`) |
| `lmdb/` | `@apackdev/ears/lmdb` (`database.ts`): `openDatabase({ path, schema?, readOnly?, syncs? })`, `createTemporaryDatabases()` (a database per test or tool run, in a temporary directory, syncs off, each removed when disposed or at exit) and `DATABASE_FORMAT`, the format a backup records |

**A call is a crossing, and a crossing costs about 3 µs.** Every read and write goes to the database through N-API,
and a read opens a read transaction (`elmdb/docs/evidence/phase-9.md`, Readings). A query answering many entities
pays that once, which is why every walk is faster than the in-memory engine was; a loop of one-entity reads pays it
per call, so a loop over `getRow` is a query with `pickAll` waiting to be written.

## Naming: a name carries the operation, structure carries the rest

Four facts describe an engine operation, and only one of them belongs in its name
([`ears-naming-axes.md`](../../docs/archive/plans/ears-naming-axes.md) has why each other place is the wrong one):

| fact | carried by | not by |
|---|---|---|
| **operation** — what it does | the name: `create`, `write`, `transact` | — |
| **capability** — anyone, or the engine's creator | the `admin` sub-object beside the operations | a second face with a name of its own |
| **binding** — which engine | who you ask: `engine.create(...)`, or the SDK's bound engine | a free function per operation, or a twin of one |
| **checkedness** — checked against a pack's shapes | the `untyped` prefix, on the SDK's pack-facing surface only | anything in this package |

**Give a new operation a name that says what it does, and nothing else.** A name that also says who may call
it, which engine it acts on or whether it was checked is a fact welded into the wrong place, and the next one
is welded beside it. **That holds where people type by hand too**: the Database console injects the library's
own names (`READ_HELPER_NAMES`, `@apackdev/sdk/database-console`), so console code reads like pack code and
there is no second name for one operation to learn. `packages/apack-host/tests/query-vocabulary.spec.ts` holds
the console's vocabulary to the same rule as the exports.

## Vocabulary

**A member is held to rules it is classified by on its own — its type and its name — not to a list of members.**
Name it by the convention and it is checked the moment it compiles; break the convention and
`tests/vocabulary.spec.ts` names it. The only list there is the exceptions, each with its reason, and an exception
that no longer excuses anything fails as stale. What the surface *is* is reviewed in the API reports (`etc/*.api.md`).

Checked off the types (compile time, in `typecheck`):

- **`is*` answers a boolean; `find*` answers a list**, empty when nothing matches.
- **No member takes a boolean flag.** A direction or a mode is a name: `out`/`in`, `linkedTo`/`linkedFrom`,
  `trash`/`restore`.
- **Writes chain.** Every write builder member returns the builder, except what ends a chain: `destroy`,
  and `id` and `row`, which read the builder.

Checked off the names, on a real engine:

- **Pairs are complete.** A negation has its positive (`unlinkedTo`/`linkedTo`, `unlink`/`link`), a `…To` its
  `…From`, an `out…` its `in…`, and `ancestors` its `descendants`.
- **Absence is `undefined`.** A `get*` asked about an id no entity has never answers `null`, on the engine or its
  `admin` face; `null` is only ever a stored value.

Held by the types and the engine specs, which say more than a rule over names could:

- **One entity row**, `{ id, entityType, ...attributes }`, wherever a read returns one.
- **One relation row**, `{ id, kind, source, target, info }`, wherever a read returns one; a match and a patch use
  its field names.
- **The subject comes first**: the entity, type or relation an operation is about is its first argument.

## The engine

- **An instance.** `createEarsEngine({ database, warn, freeze })` returns a new engine over an elmdb database, which holds everything it knows: it keeps no copy of the data, so a read reads the database. Each module is a factory closing over the connection (`createQuery`, `createWrites`, `createNameChecks`), and no module keeps anything at module scope, not even a reference to an engine. The database's schema says which names are entity types, relation kinds and roles, and what each type's rows are held to (`RowRules`): its `required` fields, which `create` refuses a row without and `delete` one of (`REQUIRED_FIELD`), and its `shortCode` prefix, which `create` numbers every row's `shortCode` with, `<prefix>-<n>`, once every check has passed, so a refused create takes no number. Whoever creates the engine gives the database its schema (`openDatabase({ schema })`, `admin.setSchema`, `admin.declare`): the app the registered packs' (`@apackdev/host/database`), a test its own names. `warn` hears what will be slow with more data: an unindexed `where` whose walk visited more than `UNINDEXED_WALK_WARNING` entities (elmdb answers a query's `visits`), once per call site. `freeze` freezes what a read hands out, for the SDK's test runtime, so code that mutates a read fails there rather than appearing to work. The short code is the engine's, like the id: `shortCode` is a reserved attribute, so no write sets or deletes it; a created row may bring one only as identity (`ShortCodeIdentity`, an import keeping its code), held to the type's `<prefix>-<n>` (`INVALID_SHORT_CODE`) and to no other entity holding it (`SHORT_CODE_IN_USE`), and an upsert's row may repeat the entity's own and no other. A code is unique, so `query(type).where('shortCode', code)` finds its entity, answered by the value index; which type a bare code belongs to is the registered packs' answer (`entityTypeOfShortCode`, `@apackdev/sdk/services`), since the prefixes are theirs and the engine knows none. The admin face's loads take any row. Codegen writes the rules: the required fields read off a pack's shapes, the prefix from its manifest entry's `shortCode`; the shape declares no `shortCode`, which a pack's `#generated/ears` adds to the shape from that same prefix (`ShortCodeOf<E>`).
- **One argument, one meaning.** Each entry point makes one claim about its argument and is held to it, throwing an `EarsError` (`errors.ts`) with a code otherwise: `query(type)` and `query([types])` take registered types and refuse anything else, `undefined` included (`UNKNOWN_ENTITY_TYPE`); `query.ids(ids)` takes ids and drops absent ones; `query.all()` is every entity but the engine's numbering sequences; `create(type, row)` takes the whole row in one object — its fields, and its `id`, `createdAt` and `updatedAt`, which no field can be named, so nothing can land in the wrong argument — and refuses an unregistered type, an id in use (`ID_IN_USE`), an id naming another type (`TYPE_MISMATCH`) and any other of the engine's own fields, before the entity exists; `write(id)` refuses an id no entity has (`NO_SUCH_ENTITY`) — a relation's included, which the compiler refuses there too (`WriteTarget`: an `EARS.RelationId` carries a marker `write`'s parameter can't take), the runtime refusal naming `updateRelation` and `removeRelation` for an id given untyped; a relation is read by `getRelation` and `findRelations`, changed by `updateRelation` and removed by `removeRelation`, each refusing an id no relation has (`NO_SUCH_RELATION`); `upsert(type, { id, ...row })` writes the row to the entity with its id or creates the entity with it — `create`'s row, the id always given; a link, and every read or walk that names a relation kind, refuse one the creator's `isRelationKind` doesn't know (`UNKNOWN_RELATION_KIND`), and `grant`, `revoke`, `grantExclusive` and `withRole` a role its `isRole` doesn't (`UNKNOWN_ROLE`), since a read naming a name nothing declared would answer as if nothing held it — the admin face's direct writes and loads take any, which is how data a removed pack left is read. An engine that took one string for a type or an id had to read an unknown name as an id, so a typo found nothing and a write to it made an orphan; separate entry points ask the caller which was meant, which is what lets each refuse. The untyped twins' name parameters are `TypeName<E>` (`write.ts`), so a dashed literal or an `EntityId` passed as a type fails to compile.
- **One value per attribute.** An attribute holds exactly one value, and a list is one array value, so a read (`getRow`, `getAttr`, `pick`, `pickAll`, `pickOne`, a write's `row()`) gives back exactly what was written, a one-element list as a list. Roles are the `role` attribute's list. A value is encoded as it is written and decoded as it is read, so a value a read handed out is the reader's own copy, and changing it changes nothing stored. A value elmdb's value spec doesn't hold — a number that isn't finite, a map key that isn't a string — is refused (`VALUE_INVALID`), and a field holding `undefined` is absent. `where(k, v)` matches a value equal to `v` or a list holding it (`holds`); a predicate is given the whole value.
- **The write builder** (`WriteBuilder`, `runtime.ts`): `set`, `setMany`, `append`, `merge`, `delete`, `removeAt`, `removeValue`, `link(kind, target, { info })`, `unlink(kind, target?)`, `unlinkAll`, `grant`, `revoke`, `grantExclusive(role, among)`, `trash(at?)`, `restore()`, `destroy()`, `id`. Every attribute write refuses `RESERVED_ATTRIBUTES` (`id`, `entityType`, `shortCode`, `createdAt`, `updatedAt`, `role`, `trashedAt`), which only the engine writes; `setMany` refuses before writing any field. `append`, `removeAt` and `removeValue` work on the attribute's list and refuse a value that isn't one (`NOT_A_LIST`); `merge` merges into an object value. `link` is idempotent on the whole edge: kind, target and `info` (no `info` matches any edge between the ends), so linking an edge again keeps its id and its place; `link` refuses an options object with keys it doesn't know, which is what `info` passed positionally looks like. A kind's declaration holds the rest (`data.relations`): an `acyclic` kind refuses a link that would close a cycle (`CYCLE`), a `unique` kind keeps one edge from an entity to another, linking the pair again replacing its info, and no kind links an entity to itself (`SELF_LINK`). **A relation has two ends when it is committed** (`NO_SUCH_ENTITY`): outside a unit `link` refuses a target that isn't there before writing anything; inside one the check is deferred to the unit's commit, so a unit may link to an entity it creates later — a forward reference — and a link whose target it destroyed meanwhile went with it. `updateRelation` is held to the same rule for the ends it moves a relation to. A relation loaded from files can still have a missing end (`admin.addRelation` and `admin.updateRelation` take any), and the readers step around one.
- **The engine owns `updatedAt`.** `create` stamps it with the creation time (a row's own `updatedAt` keeps an import's), and a builder's attribute writes (`set`, `setMany`, `append`, `merge`, `delete`, `removeAt`, `removeValue`) stamp it once per builder; links, roles, the trash and the admin's direct writes don't. `write(id, { touch: false })` writes without stamping it — a view, a position kept for later. `null` is a stored value; `setMany` leaves a field whose value is `undefined` alone, `set(k, undefined)` refuses (`NO_VALUE`), as do `append`, `merge` and `removeValue` given `undefined`, and `delete(k)` is the one way to remove a field.
- **One verb per job in the query builder.** A filter keeps the current set — `where`, `has`/`lacks`, `withRole`, `ofType`, `within(ids)`, and by an edge `linkedTo(kind, other?)` (X → other, or X → any entity the query could reach), `linkedFrom(kind, other?)` (other → X, or any → X), their negations `unlinkedTo`/`unlinkedFrom` (a tree's roots are `unlinkedFrom(parentKind)`), and `linkedWith(other)` (any kind, either way). A walk moves to an edge's far end — `out(kinds, type?)`, `in(kinds, type?)` and `both(kinds, type?)`, typed to `type` — or any number of edges deep: `descendants(kinds, type?, { depth? })` and `ancestors(kinds, type?, { depth? })`, nearest first, each entity once, so a cycle ends the walk; `pathTo(to, kinds, { direction?, depth? })` is a shortest path from the set to `to`. `outEdges`/`inEdges` return each edge as a relation row, and `degrees(kinds, type?, { direction? })` how many each entity has. Two graph algorithms run over the query's own entities and the edges between them (`elmdb/docs/roadmap/graph-algorithms.md`): `rank(kinds, { direction?, seeds? })` orders them by personalized PageRank from `seeds` (every entity, without), its scores answered by `hits()` and the highest kept by `limit`, `weakComponents(kinds)` answers the groups the edges join whichever way they point, largest first, `strongComponents(kinds)` the groups whose entities each reach every other following the edges' direction (the cycles), and `communities(kinds, { resolution?, seed? })` the Leiden communities they form, each connected, largest first. No step takes a boolean direction flag: `pathTo`, `degrees` and `rank` name one, `'out'`, `'in'` or `'both'` (`EdgeDirection`). **A builder is a recipe**: chaining a step runs nothing, and each terminal compiles the whole query to one plan and sends it, so a builder kept across writes answers as one made after them. A `where` given a function can't cross: the plan is split there, the part before it answering the field's values and the rest starting from the ids the function admits, one at a time behind `first`, `exists` and `pickOne`, so a predicate is called no more than a walk would call it. elmdb streams the steps that keep or walk ids, so `first`, `exists`, `count` and `limit` stop early; `orderBy`, `reverse`, `distinct`, `descendants`, `ancestors` and `rank` collect the set before them, except a `reverse` straight after `out` or `in` from one entity over one kind, which reads that entity's edges from the last, so `reverse().limit(n)` there reads `n`. A type start's leading filters are read from the first indexed `where` or `withRole` among them, its matches as it reads them, and `orderBy` then `limit` holds only the first `limit` (`elmdb/docs/architecture/engine.md`, Reads: plans). `explain()` is the terminal that says so: the query's ids, with each stage of the plan elmdb ran — the index it read, what it visited and kept, its time (`QueryExplanation`) — which the Database console shows as a table; a query with a `where` given a function is refused (`NOT_EXPLAINABLE`), the function running outside the plan.
- **Search.** A type whose row rules declare `search` (a manifest entry's, `{ fields: { field: weight }, stem? }`) has
  an elmdb text index named `search` over those fields (`schema.ts`). `search(text, options?)` is a plan step: after a
  type start it reads the index, after other steps it searches among what they reached, and it orders by relevance
  (`elmdb/docs/architecture/search.md`, Text search as built); `after(id)` pages past a page's last id; `hits({ fields?,
  highlights? })` is its terminal, refused (`NO_SEARCH`) for a query with no search or with a `where` given a function
  after it, since a function splits the plan and the search would not be in the plan the terminal ends. A type with
  no text index refuses a search (`UNKNOWN_INDEX`).
- **Vector search.** A type whose row rules declare `embed` (`{ from, chunk?, model, dims }`) has an elmdb vector index
  named `embedding` over those fields, cosine over f32 (`schema.ts`). A write to them leaves the entity's changed chunks
  pending; `pendingEmbeddings(type)` answers them with the model and dimensions, and `embed(embeddings)` stores vectors,
  counting `stale` each whose chunk changed since. `near(vector, { k })`, `like(id, { k })` and `hybrid(text, vector,
  { k, ...searchOptions })` are plan steps that `hits()` ends as it does `search`; a pending chunk is found by none of
  them. The engine never calls a model: the SDK's `embedPending` does (`@apackdev/sdk/services`).
- **Substrings and patterns.** A type whose row rules declare `substrings` (fields) has an elmdb trigram index named
  `substrings` over them (`schema.ts`). `containing(text, { caseInsensitive? })` and `matching(pattern)` (Rust's
  `regex` syntax; `PATTERN_INVALID` for look-around or backreferences) are plan steps that filter without ranking:
  after a type start they read the index's candidates and check each, after other steps they filter what was reached
  (`elmdb/docs/architecture/search.md`, Substrings and patterns as built). A type with none refuses them
  (`UNKNOWN_INDEX`).
- **Watches.** `watch(read, observer)` runs `read` and delivers its result, then runs it again after every commit that
  wrote into what it read, delivering each result that differs from the last, with the commit's sequence
  (`lastCommit()`: elmdb's, one per commit, increasing for as long as the database exists). Tracking is elmdb's
  (`elmdb/docs/architecture/engine.md`, Change sets and watch): each run drops what the watch's token held and makes
  its reads tracked under it, so elmdb files each key they got and each range they scanned, as far as the scan went;
  a commit's change set names the tokens it wrote into, found through elmdb's interval index, so a write's cost
  doesn't grow with the watches open. **However fast anything commits, a watch's cost is bounded by the scheduler,
  not by its writers** (G14 of elmdb's `goal-elmdb-next.md`): a watch has at most one run pending and the states
  between are skipped; after a run it waits four times that run's cost, and at least a frame, so a cheap view hears a
  burst at most a frame late and an expensive one takes at most a fifth of a core; a watch idle when a commit lands
  runs at once; and a flush starts no run past an 8 ms slice, the watches left over running first in the next, a
  flush always running one. A commit the connection makes past elmdb's key log — `admin.clear`,
  `admin.replaceWith`, `admin.setSchema`, `admin.declare`, a load — is heard as touching every watch. Deliveries
  never come inside a write: re-runs wait for a microtask, so a burst of commits is one, and a commit a re-run makes
  itself runs on a later turn. A unit that throws notifies nothing. A read that throws delivers to `observer.error`
  and stays watched. **A watched read is a pure read of the engine**: what it reads elsewhere isn't tracked. Writes
  another process makes to the database are not seen. `admin.watchesSettled()` resolves once every commit before it
  has been delivered, and `admin.watchStats()` counts the watches and what elmdb has filed. The contract is
  live-queries' (`docs/archive/plans/live-queries.md`), which the model-based harness checks: every watch, after every op,
  holds what its read answers afresh.
- **One row shape.** A row is `{ id, entityType, ...attributes }` (`EntityRow`) wherever a read returns one: `getRow(id)`, which reads through the trash and is `undefined` for an id no entity has, and `pickAll`, `pickOne()` and a write builder's `row()`, which are rows of `getRow`.
- **One relation shape.** A relation is `{ id, kind, source, target, info }` (`RelationRow`) wherever a read returns one — `findRelations`, `outEdges`, `inEdges` — and `findRelations` and `updateRelation` take its field names (`{ kind?, source?, target? }`, `{ source?, target?, info? }`).
- **Declared indexes.** A type indexes the fields its row rules declare (`RowRules.indexes`, written by codegen from a manifest entry's `indexes`), each an elmdb value index of one field (`schema.ts`), and every type indexes `role` and `shortCode`. An index holds `null`, booleans, finite numbers and strings up to 256 characters, a list's elements each; elmdb keeps it in the unit of every write, and builds one a schema change adds in the background, a query answering from a walk until it is ready. A `where(k, v)` or `withRole` on a type start reads the index where the type declares the field; where it doesn't, it filters the type's rows, finding the same matches, and warns through `warn` past `UNINDEXED_WALK_WARNING` visits.
- **The trash is the engine's.** `write(id).trash(at?)` writes `trashedAt` (kept the first time), and the entity leaves its type's live entities in the same unit. No query reaches a trashed entity — `query(type)`, `query.ids`, `query.all`, a link's far end (`out`, `in`, `outEdges`, `inEdges`) — and a query kept across a trash no longer reaches it, since every terminal runs against the data as it is then. `query.withTrashed` is the same entry points over the trash too, and `query.trashed` those entry points starting from only the trashed entities (`trashed(type)`, `.ids(ids)`, `.all()`). A write reaches a trashed entity, so `restore()` works; relations and attribute reads by id see it, and the numbering counts it, so a trashed row's short code is never handed out again.
- **Operations, and `admin` beside them; holding the engine is the capability.**
  - The engine's own members (`Ears`): `query`, `create`, `upsert`, `write`, `transact`, attribute and role reads, the relation operations (`getRelation`, `findRelations`, `updateRelation`, `removeRelation`) and `wouldCreateCycle`. A program is handed the engine typed as `Ears`, which has no `admin`; a pack reaches the app's through the SDK (`@apackdev/sdk/ears`).
  - `engine.admin` (`EarsAdmin`): `clear`, the schema (`setSchema`, and `declare`, which keeps every declaration it isn't given: what the app applies as each pack registers), the loads an import makes (`bulkLoad`, one unit in which `bulkLoadAttr` and `bulkLoadRelation` skip the checks a write makes), `countEntities` (the rows per type), unchecked direct writes (`putAttr`, `deleteAttr`, `addRelation`, `updateRelation` — any kind between any ids, either end missing included, which is how a spec makes the data a check would refuse) and the raw listings (`getAllEntities`, `getEntitiesOfType`: every id, the trash included). Only the creator holds it: the api's composition root, a backup's read (`readBackup`), the CLI's flow compiler and decompiler (a private engine per compile), and `@apackdev/sdk/testing`.
- **Binding is the SDK's.** This package has no free function that acts on an engine: an operation is a member of the engine you hold. Which engine the app's code acts on is `@apackdev/sdk/ears`: `boundEars()`, and the operations over it pack code calls without holding an engine (`untypedQuery`, `untypedCreate`, `findRelations`, `wouldCreateCycle`, …). `bindHost` binds the app's engine there, `startTestRuntime` a test's, and tooling binds its own with the host-only `_bindEars` or passes one (`exportFlowsToDSL(dir, { engine })`).
- **Units.** `transact(fn, { durable? })` (`engine.transact`, and the SDK's `transact` over the bound engine) runs `fn` as one unit of the database: one elmdb `Unit`, one LMDB write transaction, opened at its first write, so a reader sees all of it or none and a unit that writes nothing commits nothing. Writes inside it read their own writes. If `fn` throws, the unit is aborted and keeps nothing; if it returns, it commits. A unit inside another is a savepoint: it releases into it and rolls back only its own writes. `durable: true` commits with a sync rather than leaving it to group commit (see Persistence). An `async fn` is refused, since an `await` would split the unit — but only once it has started, so what it writes after its first `await` lands outside any unit; its rejection is handled, so it is not an unhandled one. `admin.clear()` inside a unit is refused too. Outside a unit each write is its own unit, committed as it returns.
- **Repositories are not the engine's.** A pack's repositories arrive in its registration (`PackRegistration.repositories`) and are kept by the host's pack registry with its other contributions; pack code reads them through `#generated/repository` or `services.repository`, which read the bound app's packs (`@apackdev/sdk/services`' `repository`). An engine knows nothing of them.
- **Typed facades.** `defineEars<Schema>(engine)` returns `query`, `create`, `upsert`, `write`, `transact`, `newId` and `getAttr` typed against a schema (`EarsSchema`: its entity shapes, and the names, relation kinds and roles they take; `UntypedSchema` checks nothing), each acting on the engine the `engine` getter returns when called. Packs don't call it: `apack pack generate-entries` writes `#generated/ears` with it, over the pack's shapes, its dependencies' and the SDK's, bound to the SDK's `boundEars`, declaring the pack's `PackSchema`.
- The trust model: packs run in-process. The faces define the contract, not a security boundary.

## Persistence

- **The database is elmdb's.** `openDatabase({ path, schema?, readOnly?, syncs? })` (`@apackdev/ears/lmdb`) opens the
  database at `path`, giving it `schema` (an `EngineSchema`; a schema that differs from the one it holds changes it,
  as `setSchema` would) or keeping the one it holds. `readOnly` opens existing files without writing: every write is
  refused. Reading still requires access to LMDB's lock file for reader registration; a permission failure refuses the
  open rather than disabling locking. Nothing opens on import.
- **A unit is visible as it commits, and durable within the window.** elmdb commits each unit without a sync and
  syncs on group commit, so a commit is on disk within a second; `durable()` resolves once every unit committed before
  it is, and `transact(fn, { durable: true })` commits with the sync (`elmdb/docs/architecture/engine.md`, Durability). `syncs:
  false` turns syncing off, for a temporary database a crash can't matter to.
- **`NOT_DURABLE` means committed.** A durable unit or admin/schema operation whose sync fails leaves its committed
  data in place. The connection notifies views and refreshes schema declarations before rethrowing this code. A caller
  must not retry it as an uncommitted refusal; `lastCommit()` reports the current sequence without parsing an error
  message. A failure before commit leaves data/declarations unchanged and emits no commit notification.
- **`SCHEMA_CHANGED` is an uncommitted refusal.** Another process changed the stored declaration after this handle
  opened. The code propagates without commit notifications or adopting the refused declaration. Close the database,
  reopen it and create a new engine to use the stored schema; `schema()` on the earlier handle is its cached one.
- **A backup is elmdb's compacted copy** (`backup`), and a restore its `replaceWith`, the database staying open: both
  run through `@apackdev/host/backup`'s `databaseBackupStore`. `DATABASE_FORMAT` (`elmdb/1`) is what a backup records
  of where its files came from; opening them is what decides whether they are readable. The format stays 1, changed in
  place, until a release has users (root `CLAUDE.md`, Backward compatibility).
- **Two databases**, each with its own engine: the app's (`openDatabaseStore`, `@apackdev/host/database`, which
  declares the registered packs' schema and again as each registers) and the run history's (`openRunHistory`, held to
  the SDK's `_RUN_HISTORY_SCHEMA`), so no `transact` on the app's engine reaches a run. The api's composition
  (`packages/api/src/runtime/index.ts`) opens both and binds the app; host code (`@apackdev/host/services`,
  `/backup`) takes each database as an argument.
- **A database crosses an in-place change to format 1** by `elmdb dump` from the build before it and `elmdb load` by
  the build after, on a copy (`elmdb/docs/architecture/format.md`, Dumps).

## Typed contract

`entities.ts`, `runtime.ts` and `typed.ts` are part of the typed EARS contract with the SDK's `types/entities.ts` and `types/sdk-entities.ts` and the generated `PackShapes`/`EntityName`. Editor completions and error messages depend on their exact form. Read `packages/apack-sdk/TYPED-EARS.md` and follow its checklist before changing them. The SDK's `EARS` namespace aliases this package's types and adds the SDK's entities and relation kinds.

## Guards

- `check:specifiers` (`scripts/check-import-specifiers.ts`): `findUpwardImports` (this package imports no `@apackdev/*`, in sources, tests, scripts and its `package.json`), `findElmdbImports` (only `src/lmdb/` imports `elmdb`; the engine's root imports nothing of `lmdb/`, host and api code never imports `elmdb`, and packs and pack tests import neither, the `elmdb-imports` pack rule), `findSharedPackageLists` (the shared-instance list's consumers derive from `SHARED_INSTANCE_PACKAGES`).
- `tests/no-module-state.spec.ts`: no module-level `new Map`/`Set`/`WeakMap`/`WeakSet`/`Array`, `[]`, `{}` or `let` in `src/`, with no exception.
- The typed-EARS specs live in default-setup (`typed-query-builder`, `branded-entity-id`, `entity-shape-registry`, `sdk-type-safety`) and in the CLI (`facade-typing.integration.spec.ts`, against workspace source and the published packages); `published-exports.integration.spec.ts` and `published-sdk-types.integration.spec.ts` cover the packed package.

## Tests (`tests/`)

- `engine/`: black-box specs of the public API: `writes` (`create`, `upsert` and `write`, each entry point's refusals, the builder's attribute writes, the reserved names, the ids and fields returned), `relations` (link, unlink, `out`/`in`, `linkedTo`/`linkedFrom`/`linkedWith`, the hierarchy walks, a kind's rules, roles), `queries` (the entry points and their refusals, filters, terminals and ordering), `transact` (every write undone by a unit that throws, one commit per unit, nesting) and `bulk-load` (the admin face's loads). They reach the engine only through `engine-under-test.ts` (`freshEngine`), each engine on a temporary database of its own. A few specs pin existing quirks; they say so.
- The correctness harness (`_harness/`: a reference model and random workloads from `fast-check`): `engine/model-based.spec.ts` checks the engine against the model after every op — the rows, every refusal, a unit and a savepoint rolling back, and the derived reads (a type's entities, count and trash, an indexed and an unindexed `where`, `withRole`, each kind's `out`/`in`). Each run creates every id first and mixes in an edge linked, unlinked and linked again, since a workload over an empty database refuses nearly everything; and a watch on each of those reads, every row and the whole state delivers what the read answers afresh after every op. `engine/watch.spec.ts` holds the watch contract's edges. A commit the database refuses, and a process killed mid-write, are elmdb's own harnesses' (`elmdb/docs/architecture/testing.md`).
- `vocabulary.spec.ts`: the rules in Vocabulary, over every member of the engine, `admin`, a query builder and a write builder — off the types for `is*`, `find*`, flags and chaining, off a real engine for pairs and absence — each with a case showing it names a member that breaks it.
- `engines.spec.ts`: a typed facade acts on whichever engine its getter returns; two engines in one process share nothing. The bound engine's operations are the SDK's, and its spec is `apack-sdk/tests/ears/bound-ears.spec.ts`.
- One half: the package has no integration config. Test files run in parallel (`fileParallelism: true`): each creates its own engines.

## Benchmark

`bench/ears.bench.ts` loads an app-sized graph (50k entities, 200k attributes, 100k relations) through the admin face's bulk load, on an elmdb database in a temp dir synced as the app's is, then times `query` by type with a `where` and `pickAll`, lookups by a unique value and by role (the value index), a relation traversal, `orderBy`, `first()` over a type, `getRow`, `pickAll`, `linkedWith`, a 10k-edge hub, short codes, and 1,000 creates, alone and as one unit (`transact`). It times the engine in `dist`, the build that ships: `npm run bench -w @apackdev/ears` builds the package first. `npm run bench:heap -w @apackdev/ears` (`bench/heap.ts`) reads the heap with the same graph loaded, then what opening its database again costs (the app's boot) and `getSchemaStats`. The readings are in elmdb's `docs/evidence/phase-9.md`; the tolerance is +10% median per case, and anything beyond it needs a cause recorded before an engine change lands. `npm run bench:watch -w @apackdev/ears [-- <seconds> [none]]` (`bench/watch-stress.ts`) is G14's stress reading: a writer committing on every turn into 20,000 tasks under a one-row view, the indexed `where` with a `limit` of 50 and an ordered read of every open task, printing commits a second, the share of a core, the event loop's delay and how late each view hears a commit (`none`: the writer alone); its readings are in elmdb's `docs/evidence/next-phase-3.md`.

## Scripts

- `npm run typecheck -w @apackdev/ears` (`npm run typecheck:ears` at the root): `tsc --noEmit` over `src`, `tests` and `bench`.
- `npm test -w @apackdev/ears` (part of `npm run test:unit`), `npm run bench -w @apackdev/ears`.
- `npm run build:package -w @apackdev/ears` (the repo's `scripts/build-package.ts`, shared with `@apackdev/sdk`, part of `npm run packages:build`): `dist/` from `tsconfig.package.json`, checking every export target was built. `npm run packages:check` runs publint and attw on it.
- `npm run api:check` / `npm run api:update` (from `packages/apack-ears`): the API reports `etc/index.api.md` and `etc/lmdb.api.md`. Review their diff with any export change; pack-facing exports have no `any`.
