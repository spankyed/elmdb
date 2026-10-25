# Phase 10: apack off lmdb-js

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Node 23.11.1. apack's worktree
`~/Develop/Projects/AgentBuddy-worktrees/ears-on-elmdb`, branch `session/ears-on-elmdb`, at `974faa775`; elmdb at
`2cbcc6c`.

## What was removed

lmdb-js, from `@apackdev/ears` (its optional peer and dev dependency), `@apackdev/cli` and `packages/api`, and with it
the lockfile's six packages; `kv.ts` and `src/kv/` (the JavaScript engine: attribute storage, relations, journal,
numbering, hierarchy, store access, its query and write builders); `lmdb/store.ts`, `kv-backend.ts`, `stamp.ts` and
`convert.ts` (`openLmdbStore`, `openEnvironment`, `LMDB_FORMAT_VERSION`, `COMPACT_COPIES`, the sync constants, the
format conversions); `scripts/ears-convert.ts` and `scripts/short-codes-convert.ts`; ears' integration half (its
config, the crash, close, durability, store, format-stamp, ordered-kv and convert specs) and the model-based spec on
the two JavaScript backends. The API reports lose exactly those exports (`etc/index.api.md`, `etc/lmdb.api.md`).

- **The model harness** (`tests/engine/model-based.spec.ts`) drives the engine on elmdb against the reference model,
  120 runs; the differential half against the JavaScript engine goes with it. The workload's `refuseNext` op goes too:
  only the JavaScript engine's backend could refuse a commit on demand, and elmdb's model harness covers refusals.
- **The guard**: `findLmdbImports` is `findElmdbImports` (`ELMDB_RULES`): host and API code never imports `elmdb`, the
  engine's root imports nothing of `lmdb/`, and packs import neither `elmdb` nor `@apackdev/ears/lmdb` (the
  `elmdb-imports` pack rule, run by `apack pack validate` and `build` for every pack). Each population has a firing
  case (`import-specifiers.integration.spec.ts`, `pack-rules.spec.ts`).
- **The packaged app's native smoke** (`build/prod/smoke-native.mjs`) opens an elmdb database, writes its schema,
  reopens it read-only and reads it back, in the app's own executable; run against the source tree's `packages/api`
  resolution it passes.
- **Docs**: `packages/apack-ears/CLAUDE.md` is rewritten to the module map on elmdb; the root, host and API guides and
  `docs/public-facing/architecture.md` describe the two elmdb databases. The two plans the move superseded
  (`ears-persistence-and-history.md`, `ears-attribute-filtering.md`) are archived.

## Done when

- **`lmdb` in no `package.json`**: `git ls-files '*package.json' | xargs grep -l '"lmdb"'` finds none.
- **`npm run chain -- --no-native` passes**: `chain passed in 165.9s (19 of 31 cached)`. The run before it, on the
  same source but for four specs' fixtures and two archived docs' links, passed the integration pool whole (509 s);
  it failed in the host pool on those, which D25's names had made stale.
- **`apack db` reads a database the app has open**: `db.integration.spec.ts`, "reads a database another process
  holds open in a unit it is writing, as a running app does": a child process opens the database to write and holds
  a unit open, so LMDB's one write transaction is taken, while `apack db query` reads a row.

## Test speed (D10)

The unit pools' in-chain wall times, which the chain records for each passing step: on elmdb, host 81.1 s and pack
45.7 s (the passing chain above); on lmdb-js, in the main checkout, host 69.3 s and 83.7 s, pack 38.4 s and 52.8 s
(2026-10-24 and 2026-10-25). The pools are no faster: a test database is a temporary directory per engine
(`createTemporaryDatabases`) where the memory store was a JavaScript object, which the in-chain variance on lmdb-js
(±10 s) is wide enough to hide either way.

## D17's re-reading

`npm run bench -w @apackdev/ears` (means, ms), after Phase 10, the machine idle:

| Case | Phase 9 | Phase 10 |
|---|---:|---:|
| bulk load 50k entities, 200k attributes, 100k relations | 1,272.63 | 1,286.41 |
| query by type with where and pickAll | 39.08 | 38.78 |
| 1,000 where lookups by a unique value | 8.13 | 8.32 |
| 10 where lookups on an unindexed field | 254.20 | 252.23 |
| 100 withRole lookups | 33.49 | 33.32 |
| 1,000 query.ids(id) lookups | 4.76 | 4.76 |
| query by type with 3 chained steps | 29.81 | 29.84 |
| relation traversal | 0.023 | 0.022 |
| orderBy a string over 40k | 34.69 | 34.98 |
| 1,000 query(type).first() over 40k | 5.21 | 5.13 |
| 1,000 getRow | 5.84 | 6.23 |
| pickAll over 40k | 91.37 | 89.79 |
| 100 linkedWith | 87.24 | 86.43 |
| link 10k tasks to one project, then destroy it | 992.96 | 813.47 |
| 1,000 creates | 114.43 | 107.00 |
| 1,000 short codes | 58.21 | 60.42 |
| transact: 1,000 creates as one unit | 24.55 | 23.89 |

**link 10k, then destroy (+10% on lmdb-js's 737.42), attributed.** Each `link` runs outside a unit, so the case is
10,000 committed units and one destroy. Split by a probe on the built package (four rounds, steady after the first,
while the file grows): the 10,000 links as units take about 600 ms, the same links as one unit about 155 ms, and the
destroy of the 10k-edge hub about 130 ms. So about 45 µs of each link is its unit's commit; a `set` committed alone
costs about 38 µs against 11 µs inside a unit. The lmdb-js engine committed a unit in JavaScript, with no crossing.
Code that links in a loop runs it in a `transact`, which is what an app's hub writes are (a content apply, a flow's
nodes).
