# Goal: elmdb, built end to end and running apack

Status: planned, 2026-10-24. Not started.

```text
# Goal: elmdb, built end to end and running apack

Implement elmdb/docs/goals/goal-elmdb.md. Read Scope, Current handoff, Decisions, Phases,
Acceptance and Constraints before continuing. The design it builds is elmdb/docs/ (design.md,
schema.md, engine.md, search.md, format.md, testing.md); apack's side is
AgentBuddy/docs/plans/ears-on-elmdb.md.

Work in two repositories, both under ~/Develop/Projects:
- elmdb: follow elmdb/CLAUDE.md once Phase 0 has written it, and this goal's Constraints.
- AgentBuddy (apack): follow its CLAUDE.md, its docs/AGENTS.md and its user memory, in the
  worktree this goal names (never the main checkout).

The survey was made at elmdb be66370 (with the design docs uncommitted) and AgentBuddy
fe3785fb0 on ears-disk-engine. Inspect current code before relying on a path or a figure here.

Finished when:
- Every row of Acceptance is met on current source, with the commits and the evidence recorded.
- apack runs on elmdb with lmdb-js removed, and its final validation in Phase 15 passes.
- elmdb's docs and apack's reference docs describe what was built.
- Outcome is written, the goal and apack's plan are archived, and closure is committed.

Commit usable checkpoints as you go. Keep Current handoff current. Do not expand or reduce scope
without the user's authorization.
```

## Scope

**Outcome.** elmdb, an embedded graph database in Rust on LMDB with a Node binding, built to its design, and apack's
EARS engine running on it underneath an unchanged pack-facing API, with lmdb-js gone from apack.

**Deliverables:**

1. The elmdb repository: a Cargo workspace (`crates/elmdb`, `crates/elmdb-node`, `crates/elmdb-cli`), its own
   `CLAUDE.md`, local checks, and generated TypeScript types and JSON Schema.
2. The engine: schema, format, storage, durability, writes, reads, value indexes, derived-data maintenance, admin,
   change sets, tracked reads and watch, text search, vector and hybrid search.
3. The test suites in `testing.md`: the model harness, the crash and failed-sync harnesses, `check()` after every case,
   binding fuzzing.
4. apack on elmdb: `@apackdev/ears` compiling to plans and batches, the schema from codegen, the `runs` database,
   text search and vector search exposed to packs, `engine.watch` on elmdb under live-queries' contract, packaging, and lmdb-js, the memory backend, the record store
   and the format conversions removed.
5. The documentation updated to the implemented contract in both repositories.

**Not in scope:** publishing to npm or crates.io, reserving names, buying a domain, pushing beyond D16 and D20, releases, a server or
replication, value compression, LMDB 1.0 (`heed3`), substring and regex search. They're listed under Deferred.

**Entry points at the survey:**

- elmdb: `README.md`, `docs/` (the design), nothing else.
- apack: `packages/apack-ears/` (the engine, 4,308 lines in `src/`, its `CLAUDE.md`), `packages/api/src/runtime/index.ts`
  (`openAppStore`), `packages/apack-host/src/database/open.ts`, `src/backup/`, `src/services/{app-data,run-history,index}.ts`,
  `packages/apack-sdk/src/testing/` (`startTestRuntime`, `memory-run-history.ts`), the CLI's codegen
  (`apack pack generate-entries`, which writes `RowRules`), `scripts/ears-convert.ts`, `scripts/short-codes-convert.ts`.

## Current handoff

Closed 2026-10-25: every Acceptance row is met, and Outcome below says what was built and what remains. Nothing is in
flight. apack's branch `session/ears-on-elmdb` is pushed as draft PR #276, for the user to review and merge; elmdb's
`main` is pushed.

## Outcome

**Built.** elmdb, an embedded graph database in Rust on LMDB with a Node binding, is built to its design and runs apack:
the schema and its changes, the format and its upgrade from apack's format 4, durability with group commit, writes in
batches and units with savepoints, reads as plans, value, text and vector indexes kept in the same transaction, derived
data with `check()` and `rebuild()`, admin operations and the CLI, a second database for the run history, change sets
with a commit sequence, and tracked reads. It is proven by a model harness, a crash harness, a failed-sync harness and
binding fuzzing, in CI on macOS, Linux (glibc and musl) and Windows. apack's `@apackdev/ears` runs on it underneath an
unchanged pack-facing API, with lmdb-js removed; packs gained text search (the threads panel's message search), vector
and hybrid search (`embedPending` through `services.inference`; the library index ported onto it and left off, D15),
and `engine.watch` under live-queries' contract. The packaged app ships elmdb as real files. Every phase's evidence is
in `docs/evidence/`.

**What the run changed from the plan.** The decisions table records each: the protocol cost measured before building
(D4), the lock mode on macOS (D24) and closing what Node leaves open (D33), the manifest names (D25), text indexes over
`any` fields (D26, D28), vector metrics (D29), one embedding model for the library (D31), watch as live-queries'
contract (D30) and tracked reads that add to a token (D32).

**Left, each the user's or a later goal's:**
- **Merging PR #276**, and publishing elmdb (Deferred).
- **The 17 duplicate short codes** in the user's real data, to resolve when it is converted (D12).
- **One benchmark case past D17's +10%**: "link 10k tasks to one project, then destroy it", 813 ms against lmdb-js's
  737 ms, because each `link` outside a unit commits a unit of its own (`docs/evidence/phase-10.md`); code linking in a
  loop runs it in a `transact`.
- **A SIGKILLed process still leaves its LMDB semaphores** (the exit-with-parent guard's orphan case, a spec's
  deliberate kill); a reopen at the same path clears them, a temporary path's stay until a restart (D33).
- **lmdb-js's System V sets** on the user's Mac, left by the main checkout, which still runs it until #276 merges.
- **The once-seen silent API exit** after a pack disable under the full chain's load; it never recurred.
- **Views, and the work after v1**: `goal-elmdb-next.md` and apack's live-queries branch (D30).

## Decisions

Final unless marked open. Each cites the document that owns it; change the owner and this table together.

| # | Decision | Owner |
|---|---|---|
| D1 | The engine is Rust. apack keeps `@apackdev/ears`'s API, typed contract and builders, which compile to elmdb's plans and batches | `design.md`, The boundary with apack |
| D2 | elmdb builds on heed (`lmdb-master-sys`), with no LMDB `-sys` crate of its own. Exactly one LMDB `-sys` crate in the dependency tree, checked | `design.md`, Built on, principle 5 |
| D3 | The schema is the canonical JSON in `schema.md`, validated completely, stored and hashed, and compared at open | `schema.md` |
| D4 | Plans and batches cross the binding as one msgpack buffer each, and a result comes back as one buffer that apack's facade decodes. Their Rust types are the definition; the TypeScript is generated (ts-rs), and the JSON Schema of the schema (schemars) | this goal; `design.md`, principle 6 |
| D5 | The key layout is `format.md`'s, primary under `p/` and derived under `d/`, one LMDB database per environment | `format.md` |
| D6 | Durability keeps apack's semantics: a commit survives a process crash; group commit syncs after 100 ms idle and within 1 s; `durable()` waits for the sync it needs and rejects when that fails, leaving its commits owed | `engine.md`, Durability; apack's `tests/lmdb/durability.spec.ts` |
| D7 | The engine grows the map: on `MAP_FULL` it aborts the unit, doubles the map with no transaction of the process open, and runs the unit again, up to 16 times, which it can since a unit is data. The map starts at twice the data file and 64 MiB at least, and grows in 64 MiB steps; a growth costs about 0.3 ms over the unit it retries (`docs/evidence/phase-3.md`) | this goal (closes `design.md`'s open question) |
| D8 | Strings sort by icu4x's root collation. apack accepts that `orderBy` order may change once, at the switch | `design.md`, principle 8; apack's plan, String order |
| D9 | `link`'s `acyclicIn` and `replace` leave apack's write builder; relation kinds declare `acyclic` and `unique` | `schema.md`, Relations |
| D10 | Every test opens a database in a temporary directory; there is no memory backend. Tests open with `syncs: false`, which skips group commit's syncs (a crash of the machine can't matter to a temporary directory). Phase 9 records what this costs apack's unit pools; the reading is reported, not gated | this goal (closes `design.md`'s open question) |
| D11 | apack depends on the local elmdb checkout through a symlink: each apack checkout holds a gitignored `vendor/elmdb` pointing at `~/Develop/Projects/elmdb`, and the dependency is `"elmdb": "file:vendor/elmdb/crates/elmdb-node"`, which npm links. The path is the same in every checkout, so the lockfile doesn't change between them, and an elmdb rebuild takes effect at once. apack's Linux runner mounts the checkout `vendor/elmdb` points at, read-only, and builds the binding for Linux in its own volume. CI fetches the pinned commit (D21). A published release is deferred | this goal; apack's plan, Developing across two repositories |
| D12 | Stored data moves from apack's format 4 in two steps, run on a copy: `tools/lmdb-js-export` dumps the format 4 entries (stock LMDB can't open lmdb-js's files, which are in its own data version), and `elmdb upgrade` reads the dump. Converting the lead developer's real data is the user's operational step, never done by the goal | `format.md`, Moving apack's data; `docs/evidence/phase-1.md`; apack `CLAUDE.md`, Backward compatibility |
| D13 | The run history is entity types in a second database, `runs` | `engine.md`, Opening |
| D14 | Search indexes are named indexes on types; the engine never calls a model, and pending embeddings are its queue | `search.md` |
| D15 | apack's library search index is ported onto a vector index and tested, and ships switched off (its `[SEARCH_INDEX_FF]` call sites stay commented out) | the user, 2026-10-24 |
| D17 | apack's engine benchmark (`npm run bench -w @apackdev/ears`) is read before Phase 9 and after Phase 10. A case past `packages/apack-ears/CLAUDE.md`'s +10% tolerance is investigated and fixed when the cause is elmdb's; one that remains is recorded in Outcome with its cause and doesn't block completion | this goal |
| D18 | A finding that a decision here is wrong is acted on: the goal changes the decision and its owning document in one commit, with the evidence linked, and continues. Only removing a deliverable needs the user | this goal |
| D16 | Once Phase 15's local validation passes, the goal pushes `session/ears-on-elmdb`, opens a draft PR in apack, and dispatches apack's strict chain and its macOS compiler cases against it | the user, 2026-10-24 |
| D19 | Rust comes from rustup, installed by Phase 0 for the user (`rustup-init -y --profile minimal --default-toolchain none`), and elmdb's `rust-toolchain.toml` pins one stable version with `rustfmt` and `clippy`, the latest stable when Phase 0 runs. apack's Linux image installs the same version, read from that file. The pinned version is also the minimum supported | this goal |
| D20 | elmdb lives at `github.com/spankyed/elmdb`, public, under MIT OR Apache-2.0 (the Rust ecosystem's convention), pushed at each checkpoint. Its CI is napi-rs's standard matrix (macOS arm64 and x64, Linux x64 and arm64 with glibc and musl, Windows x64): format, clippy and tests on every push to Linux, the full matrix with the model and crash harnesses on pull requests and on dispatch, which the goal runs after Phases 8 and 15. Platform coverage is CI's, so the Windows VM isn't needed: it is unreachable unattended (its Azure CLI sign-in is blocked by security defaults and its SSH port didn't answer on 2026-10-25) | the user (public), 2026-10-25; this goal |
| D22 | No crash record. LMDB's assertions are compiled out of heed's release builds (`NDEBUG`), so a release build never aborts on one, and heed doesn't expose the `MDB_env` pointer `mdb_env_set_assert` needs; `check()` and `CORRUPTED` are what report damage. Phase 3's crash-record deliverable has no subject and is dropped with this evidence | `docs/evidence/phase-1.md`; `engine.md`, Errors |
| D23 | Phase 9's "the pack-facing API reports change only where D9 says" covers what packs program against — the query and write builders, the typed contract (`TYPED-EARS.md`) and the SDK's pack entries. `@apackdev/ears`' engine construction changes with the engine (D1): `createEarsEngine({ database })`, the `EarsDatabase` port, the schema types and `@apackdev/ears/lmdb`'s `openDatabase`, which only the app's composition, tools and tests call. `EarsErrorCode` gains `SELF_LINK`, `CYCLE` (D9) and `VALUE_INVALID`, refusals a pack's write can now meet | `docs/evidence/phase-9.md` (D18) |
| D24 | On macOS elmdb builds LMDB with POSIX semaphores (heed `posix-sem`), not System V sets. A SysV key is `ftok` of the lock file — 16 bits of its inode — and lmdb-js creates 3-semaphore sets where mdb.master creates 2, so a colliding set left by either refuses the other with `EINVAL` ("Attempting to setup locks"): apack's specs met it twice while both engines were installed. POSIX names hash the whole device and inode. With 20 databases open no SysV set is created (2026-10-25). Linux and Windows keep robust mutexes | `crates/elmdb/Cargo.toml`; `docs/evidence/phase-9.md` |
| D25 | apack's manifest holds elmdb's names (`schema.md`, Names): an entity type is PascalCase and an indexed field lowerCamelCase (`apack.schema.json` and `parseManifest`), and codegen refuses a shape requiring a field of another case; a field a shape leaves optional reaches elmdb as an additional field, which any name may be. Relation kinds and roles were already snake_case. Without it a pack validated and built with a name the app's database then refused at boot: the own-declarations spec declared an entity `__proto__` | `docs/evidence/phase-9.md`; apack `docs/public-facing/manifest.md` |
| D26 | A text index reads `string` fields, lists of strings, and `any` fields, an `any` field's value being text when it is one of those. apack declares the fields it doesn't require as additional and the required ones `any` (D9's schema mapping), and a manifest carries no field types, so restating each indexed field's type would be a second declaration a shape already makes; elmdb holds the value's kind at read instead | `docs/architecture/schema.md`, Indexes |
| D27 | charabia is built with its default features off and only `latin-camelcase` and `latin-snakecase` on: the defaults embed segmentation dictionaries for Chinese, Japanese and Korean (jieba, lindera) that make the binding many megabytes larger for text apack doesn't hold today. Latin, Cyrillic, Greek and the rest still segment and normalize; turning on a language's feature is the change when a user's text needs it | `crates/elmdb/Cargo.toml` (workspace) |
| D28 | Document's text index reads its `name` (weight 3) and `tags`, not its `content`: a section is an object holding its text under `text`, `items` or `fields[].value` beside a `type` such as `markdown`, so reading every string of the field would match each section's type. Indexing content needs a declared projection of a field's text (a path, or a writer-kept text field), which is a design change of its own and not this goal's | `docs/evidence/phase-12.md` |
| D29 | Vector indexes are hannoy's graphs, which measure cosine and euclidean distance over f32 vectors, so a vector index's `metric` is `cosine` or `l2` and its `precision` `f32`; `dot`, `f16` and `i8` are refused at schema time, saying why. A normalized embedding, which every model apack runs produces, ranks by dot product as by cosine. A text index's rule on `any` fields (D26) holds for a vector index's `from` too | `docs/architecture/schema.md`, Indexes; `docs/evidence/phase-13.md` |
| D30 | **Watch is live-queries' contract, and the views are live-queries'.** apack's live-queries work (branch `live-queries`, plan `docs/plans/live-queries.md`) built `engine.watch` on the JavaScript engine (`517900b52`) and views over it in the SDK, host, API and renderer (its Step 2), and asked elmdb for tracked reads and a commit sequence (`78f9fb0`). So Phase 14 builds elmdb's side and `engine.watch` on it under that contract, with live-queries' specs as acceptance, and builds no watched query or adopting system of its own: views, and the features moved onto them, are live-queries' Steps 2–5. **Landing order:** `goal-elmdb-next.md` takes it up once this goal closes (its G7 and Phase 2): a branch cut from this goal's branch, with `live-queries` merged into it — `517900b52`'s JavaScript-engine internals dropped for Phase 14's `engine.watch`, its plan, views and migrated features kept — and one PR, stacked on this goal's. Neither goal edits live-queries' plan or its views beyond what a conflict makes it | the user, 2026-10-25 |
| D31 | **The library's vector search has one model, and nothing embeds on a timer.** An elmdb vector index names one model, so the library's search indexes share `IndexedDoc`'s, declared in default-setup's manifest (`openai:text-embedding-3-small`, 1536 dims); an index's per-index model, metric and connectivity, and their controls in the dormant editor, are dropped with `fastembed`, `usearch` and the index files. The engine never calls a model: `embedPending` (`@apackdev/sdk/services`) embeds what a vector index holds pending, run by whoever wrote the text — the library after indexing — rather than by a timer or a commit hook; Phase 14 builds no adopter (D30), so no watch drives it | `docs/evidence/phase-13.md` |
| D32 | **A tracked read adds to its token, and a change set is the commit's answer, not a listener's.** apack's watched reads are functions that make several read calls, so a read tracked under a token adds what it touches to what the token holds, and a caller re-running its reads drops the token first (`untrack`); `engine.md` had a read replace its token's ranges, which would keep only a run's last call. Change sets, their sequence and `touched` are what each write and unit commit answers, which is every commit a caller makes; no `db.changes(listener)` is built, since apack hears the commits it makes and a background index build changes no answer. Admin writes that aren't logged key by key are heard as touching everything | `docs/evidence/phase-14.md`; `docs/architecture/engine.md`, Change sets and watch |
| D33 | **elmdb closes what a Node environment leaves open.** LMDB removes a database's lock semaphores (named POSIX semaphores on macOS, D24) as its last process closes it, and Node runs no destructor at `process.exit`, so each database a process left open there kept its pair until the machine restarted; test workers, which vitest stops with SIGTERM and so never ran their exit handlers, filled the 10,000-entry table on 2026-10-25 and every open on the Mac failed (`ENOSPC`). The binding keeps the databases each environment opened and closes those still open at `process.on('exit')` and at the environment's cleanup hook, with a bounded close; apack's test workers exit through `process.exit` on SIGTERM (`cee3cd229`). A natural end and a terminated worker thread were already released by Node's teardown; `process.exit` was not. A SIGKILL still leaves a pair, which a reopen at the same path clears. Lock-free databases were rejected (no reader table under a concurrent read pool), and so were System V sets (they leak alike, and collided with lmdb-js's) | `crates/elmdb-node/src/exit.rs`, `test/exit.test.mjs`; CI runs 40240131963 (passed) and 40240133958 (the mutant, failing the exit case); `docs/evidence/phase-15.md` |
| D21 | apack pins the elmdb commit it builds against in a committed `vendor/elmdb.rev`. Locally the `vendor/elmdb` symlink stays live, and a check refuses when that checkout doesn't contain the pinned commit. In apack's CI, a step clones elmdb at that commit into `vendor/elmdb` and builds the binding, with the Cargo target directory cached | this goal |

## Phases

Each phase lists what it delivers, what it consumes, and when it's done. Checks in elmdb are `cargo fmt --check`,
`cargo clippy -- -D warnings`, `cargo test` and the binding's `npm test`, run on what the phase touched; checks in apack
follow its `CLAUDE.md` (`npm run spec` while working, `npm run chain -- --no-native` at a checkpoint).

### Phase 0: the elmdb repository

- **Deliverable:**
  - Rust installed and pinned (D19).
  - CI for the GitHub repository (D20), and `LICENSE-MIT` and `LICENSE-APACHE`.
  - The Cargo workspace, with `crates/elmdb` (the engine), `crates/elmdb-node` (napi-rs, npm name `elmdb`) and an empty
    `crates/elmdb-cli`.
  - `elmdb/CLAUDE.md`: this repository's rules (the vocabulary and conventions in `design.md`, how to run the checks,
    commit rules matching apack's, and that a figure cited in prose carries its date and conditions).
  - The error enum skeleton, generated into TypeScript.
  - A check that fails when the generated TypeScript or JSON Schema is stale.
  - The one-LMDB check over `cargo tree`.
  - A binding smoke test that opens and closes a database from Node.
- **Dependencies:** none.
- **Done when:**
  - The checks pass locally and in CI's Linux job.
  - The stale-types check fails when a Rust type changes without regenerating.
  - The one-LMDB check fails with a second LMDB `-sys` crate added.
  - Each failure is shown once and recorded.

### Phase 1: spike, what the protocol costs

- **Deliverable:**
  - Throwaway code under `spike/`, deleted at the end of the phase: a read-only path over apack's format 4 keys that
    answers the cases in `roadmap.md` step 1 as a plan, with the result encoded in Rust and decoded in Node.
  - Its readings, against apack's engine today, in evidence.
- **Dependencies:** Phase 0.
- **Data:** the production backup of 2026-10-17, `~/Develop/backups/abuddy-backups/full-db/agentbuddy-backup-2026-10-17T23-06-18-232Z`
  (format 1, the one apack's `docs/plans/ears-storage-layout.md` Step 5 measured). Copy it into the scratch area and
  convert the copy to format 4 with apack's `scripts/ears-convert.ts` (22,816 entities, 178,131 attributes, 45,798
  relations expected). The backup itself is never opened for writing, and the live data directory is never touched.
- **Measuring:**
  - apack's `npm run measure` on a machine `npm run check:idle` passes.
  - Warm and cold, the cold runs from a disk image detached and attached again.
  - Medians with their conditions.
- **Done when:**
  - The readings are recorded, and `spike/` is removed.
  - If plan and decode is slower than today on the long `Message` read, the result envelope is redesigned and measured
    again, and building rows as JavaScript objects in Rust (napi) is measured beside it. The faster crossing is
    chosen, D4 updated to it with the readings, and work continues: the move off lmdb-js and search don't depend on
    this result.

### Phase 2: schema

- **Deliverable (`schema.md` in full):**
  - The Rust schema types.
  - Validation that reports every problem with its path and `declaredBy`.
  - The naming patterns and reserved names.
  - The canonical form and its hash.
  - The diff between a stored and a given schema, with the change report.
  - The generated TypeScript and JSON Schema.
- **Dependencies:** Phase 0.
- **Done when:**
  - Specs cover each row of `schema.md`'s change table and each refusal.
  - Two schemas that mean the same thing hash alike, and a reordered document hashes the same.
  - The `SCHEMA_INVALID` message shown in `schema.md` is reproduced by a spec.

### Phase 3: format, storage and durability

- **Deliverable:**
  - **Keys and values:**
    - the order-preserving key codec;
    - the prefix table in one module, each prefix marked primary, derived or meta;
    - the value spec, derived from apack's codec (`packages/apack-ears/src/kv.ts`) and from a scan of every value in
      the Phase 1 copy that lists each msgpack type and extension present, recorded in evidence and in `format.md`.
  - **Opening:**
    - `Database.open` with `readOnly`;
    - the format stamp and its refusals;
    - the schema stored and compared through Phase 2.
  - **Writing to LMDB:**
    - group commit, `durable()` and close per D6;
    - map growth per D7.
  - **Concurrency:**
    - reads on reader threads (verify heed's read transactions without thread-local storage);
    - the binding refusing a second load into one process.
  - **ITS#10642:** whether this layout can reach `mdb_subdb_adjust`, recorded in `format.md`.
- **Dependencies:** Phase 2.
- **Done when:**
  - Ports of apack's `tests/lmdb/store.spec.ts`, `durability.spec.ts` and `format-stamp.spec.ts` pass in Rust: a sync
    is finished or failed by the test, and a writer is killed with `SIGKILL` once its write returns.
  - A unit that grows the map passes.
  - A second binary load throws, naming both.

### Phase 4: writes, and the model harness

- **Deliverable (`engine.md`, Writes):**
  - Every batch operation, savepoints, and every refusal as an error code.
  - Numbering and short codes; field type validation per `schema.md`.
  - Relation rules: cardinality, `acyclic`, `unique`, `endpoints`, `info`.
  - Ends checked at commit; `updatedAt`, the trash, and the change set in each result.
  - The model harness in Rust (proptest), ported from apack's `tests/_harness/`, growing with every later phase. It
    includes the prelude, relink chunks and small id pools apack's has, so workloads collide.
- **Dependencies:** Phase 3.
- **Done when:**
  - Ports of apack's `tests/engine/{writes,relations,transact}.spec.ts` cases pass, each translated to batches.
  - The model harness passes with every operation in its workload.
  - A dropped index update and a skipped refusal, each mutated in a copy, are caught by it.

### Phase 5: reads and value indexes

- **Deliverable (`engine.md`, Reads):**
  - Every start, filter, walk, shaping step and terminal.
  - Value indexes, compound and unique; the planner's index choice; `readSync` and `read` on the reader pool.
  - Collation per D8.
  - A plan ending at the first step that holds a caller's function, per the protocol.
- **Dependencies:** Phase 4.
- **Done when:**
  - Ports of apack's `tests/engine/queries.spec.ts` pass.
  - The model harness checks every derived read `testing.md` names.
  - `first`, `exists`, `count` and `limit` are shown to stop early by a count of rows read.

### Phase 6: derived data and schema changes

- **Deliverable:**
  - Background index builds, with plans walking rows until built.
  - `check()` and `rebuild(index?)`.
  - Every row of `schema.md`'s change table applied at open, in one unit, with its report.
  - `admin.purge`, `renameType`, `renameField` and `renameRelation`.
- **Dependencies:** Phase 5.
- **Done when:**
  - `check()` runs after every model-harness case.
  - A spec corrupts derived keys and sees `check()` report them and `rebuild()` repair them.
  - Each change-table row has a spec, including a tightened rule reported and not rewritten.

### Phase 7: admin, the upgrade and the CLI

- **Deliverable:**
  - `backup` (compacted: verify heed's compacting copy), `replaceWith`, `reset`, `stats`, `bulkLoad`, raw writes.
  - `elmdb upgrade` from apack's format 4 to elmdb's first format, step by step.
  - `elmdb-cli` with `inspect`, `check`, `backup` and `upgrade`.
- **Dependencies:** Phase 6.
- **Done when:**
  - The sequence that aborted lmdb-js passes as a spec: a compacted copy of a database holding multi-page values,
    reopened and emptied in one transaction (apack's `docs/references/evidence/lmdb-compacted-copy-crash.md`).
  - `elmdb upgrade` on the dump of the Phase 1 data gives a database that `check()` passes and whose entity, attribute
    and relation counts equal apack's own reading of the original, recorded in evidence.

### Phase 8: crash, failure and fuzz suites

- **Deliverable (`testing.md`):**
  - The crash harness: a child killed mid-workload, the reopened database equal to the model after the acknowledged
    writes or one more.
  - The failed-sync harness.
  - Binding fuzzing: bad arguments, closed databases, malformed plans and batches each throw an `ElmdbError`.
- **Dependencies:** Phases 4–7.
- **Done when:** each suite passes locally and in CI's full matrix, linked in evidence, and a deliberately broken
  durability path (a mutated copy) is caught by the crash harness.

### Phase 9: apack parity

- **Deliverable (apack's plan, step 2), in the `ears-on-elmdb` worktree:**
  - **The link (D11, D21):**
    - `vendor/elmdb` gitignored, and `vendor/elmdb.rev` committed;
    - a setup step that creates the symlink and refuses when its target isn't an elmdb checkout holding the pinned
      commit;
    - the `file:` dependency;
    - the Linux runner mounting the elmdb checkout and building the binding (Rust in `scripts/linux/Dockerfile`, per
      D19), so `test:linux` and the chain's native step load a Linux build;
    - apack's CI workflows cloning elmdb at the pinned commit and building it.
  - **The engine on elmdb:**
    - `@apackdev/ears`'s builders compile to plans and batches;
    - `createEarsEngine` runs over an elmdb `Database`;
    - `EarsError` carries elmdb's codes.
  - **The schema:** codegen writes each pack's fragment (`schema.md`, apack's schema) and the host merges them.
  - **Composition:** `openAppStore`, `openDatabaseStore`, the host services and the SDK's test runtime move onto
    elmdb.
  - **Differential testing:** apack's model harness drives today's engine and elmdb with the same workloads and
    compares both with the model and with each other.
  - **Write-builder changes:** `acyclicIn` and `replace` are removed per D9, with every in-repo caller, prompt and
    example migrated.
  - **Test speed:** the change in apack's unit pools' wall time is measured and recorded (D10).
- **Dependencies:** Phases 5–8.
- **Done when:**
  - The differential harness agrees on every case.
  - apack's ears, sdk, host, api, cli and default-setup suites pass on elmdb.
  - `npm run chain -- --no-native` passes.
  - The pack-facing API reports change only where D9 says.
  - The test-speed reading is recorded.

### Phase 10: apack off lmdb-js

- **Deliverable:**
  - **Removed:** lmdb-js, `kv.ts`, `store-access.ts`, the memory backend, `lmdb/store.ts`, `kv-backend.ts`,
    `stamp.ts`, `convert.ts`, `COMPACT_COPIES`, `scripts/ears-convert.ts` and `scripts/short-codes-convert.ts`.
  - **Guards:** `findLmdbImports` becomes a rule over `elmdb` imports.
  - **Docs:** `packages/apack-ears/CLAUDE.md` is rewritten to the new module map.
- **Dependencies:** Phases 9 and 11: the run history's record store is an lmdb-js file too, so it moves first.
- **Done when:**
  - `lmdb` appears in no `package.json`, and the import guard has a firing case.
  - `npm run chain -- --no-native` passes.
  - `apack db` reads a database the app has open, read-only.

### Phase 11: the `runs` database

- **Deliverable (apack's plan, step 3):**
  - The run history becomes entity types in `runs`, its indexes value indexes.
  - `RecordStore`, `openRecordStore` and `memory-run-history.ts` are removed.
  - Backup, reset and import cover both databases.
  - Existing run history moves too: `tools/lmdb-js-export` gains a mode that dumps a record store's records, and apack
    an import of that dump into `runs`, both run on a copy (D12).
- **Dependencies:** Phase 9.
- **Done when:**
  - apack's run-history specs pass on `runs`.
  - A backup restored into a fresh data directory holds both databases.

### Phase 12: text search

- **Deliverable (`search.md`, order of work 1–2):**
  - **In elmdb:** text indexes (BM25 with field weights, phrases, prefix, fuzzy terms, the analyzers), suggestions,
    highlights, the `search` plan step combined with filters, facets and paging, and fuzzy labels.
  - **In apack:** the manifest's text index, the query builder's `search`, and default-setup declaring text indexes on
    Message, Note and Document. One consumer: a search field in the threads surface's panel that lists matching
    messages, with highlights, newest-weighted, and opens the thread at the message.
- **Dependencies:** Phases 6, 10.
- **Done when:**
  - Ranking is checked against fixed corpora, and the model harness checks match sets.
  - An apack E2E driving script (`drive/`) shows a search returning messages.
  - apack PR #148 is reported to the user as superseded. Closing it is theirs.

### Phase 13: vector and hybrid search

- **Deliverable (`search.md`, order of work 3):**
  - **In elmdb:** hannoy vector indexes, chunking, pending embeddings, `near`, `hybrid` and more-like-this.
  - **In apack:** an embedding worker over `services.inference`, the query builder's `near` and `hybrid`, and the
    library search index ported onto a vector index. The port keeps only the API models `services.inference` runs;
    the local FastEmbed models and the `fastembed` and `usearch` dependencies are dropped from it.
- **Dependencies:** Phase 12.
- **Done when:**
  - A spec embeds through `fakeInference`, searches, then changes a source field and sees the vector go pending.
  - Hybrid results are checked on a fixed corpus.
  - The library port passes its specs: indexing a folder's documents, searching it, and an edited document going
    pending. The feature stays off (D15).

### Phase 14: change sets, tracked reads and `engine.watch`

- **Deliverable (`engine.md`, Change sets and watch; apack's `docs/plans/live-queries.md`, The contract and The move
  to elmdb), D30:**
  - **In elmdb:** change sets with the commit sequence on every change set and batch result; tracked reads — a read
    call tracked under a token records the key ranges it touched, held in an interval index in Rust, a commit's result
    and change set naming the tokens it wrote into (`touched`), and `untrack`; `watch(plan)` as a convenience over them.
  - **In apack:** `@apackdev/ears`' `engine.watch(read, observer)`, `lastCommit()`, `watchesSettled()` and
    `watchStats()`, keeping live-queries' contract exactly, implemented over tracked reads: a watch runs its read
    tracked under a token of its own, a commit's `touched` tokens mark watches to run again, and the re-runs are
    coalesced, asynchronous, compared for equality and delivered with the commit they reflect.
  - **The acceptance is live-queries' own,** ported from branch `live-queries` (`517900b52`): `watch.spec.ts`, the
    model harness's watch property (each watch's last delivered result equals its read run afresh, after every op), and
    the bench's watch cases. Its read recorder, write log and read-set index are the JavaScript engine's and are not
    ported; elmdb's interval index gets its own spec against a brute-force scan.
- **Dependencies:** Phases 5, 10.
- **Done when:**
  - A spec in elmdb shows a tracked read's token named by a commit that wrote into what it read, and not by an
    unrelated one; the interval index agrees with a brute-force scan.
  - live-queries' ported specs and harness property pass on elmdb, and their mutation checks — a write not reported,
    a range recorded short, a refused unit notifying — each fail one.
  - The bench's watch readings are recorded: a create's commit cost with 0, 10, 100 and 1,000 watches open stays flat.

### Phase 15: platforms, packaging and final validation

- **Deliverable:**
  - The binding built for every target in D20's matrix, by elmdb's CI.
  - The packaged apack app shipping elmdb as real files, the way it ships `@napi-rs/keyring`. The packaging copies the
    linked package rather than its symlink.
- **Dependencies:** Phases 10–14.
- **Done when:**
  - elmdb's full CI matrix passes at the pinned commit.
  - A packaged macOS build (`npm run build:preview`) holds no symlink leading out of the bundle, and boots to
    `connected` against an isolated data directory.
  - apack's full `npm run chain` and `npm run chain -- --e2e` pass on the branch.
  - The branch is pushed, a draft PR opened, and apack's strict chain
    (`gh workflow run ci.yml --ref session/ears-on-elmdb -f strict_chain=true`) and macOS compiler cases
    (`-f compiler_macos=true`) pass, their runs linked in evidence (D16).

### Phase 16: documentation and closure

- **Deliverable:**
  - elmdb's `docs/` describing what was built, with each "designed, not built" status replaced.
  - apack's root `CLAUDE.md` (Data layer, SDK packages) and `packages/apack-ears/CLAUDE.md` describing the engine on
    elmdb.
  - apack's `docs/plans/ears-on-elmdb.md` and `ears-value-compression.md` archived, with their status and successors.
  - Outcome written here, and this goal moved to `elmdb/docs/archive/goals/`.
- **Dependencies:** Phase 15.
- **Done when:** every row of Acceptance is met, the links are repaired in both repositories, and closure is committed
  in each.

## Acceptance

| Requirement | Implementation / source | Evidence | Remaining work |
|---|---|---|---|
| elmdb repository with checks, generated types, one-LMDB guard (P0) | done, `aff9050` | `docs/evidence/phase-0.md`; CI run 40159534326; the full matrix, run 40170026515 | none |
| Protocol cost measured, D4 confirmed (P1) | done | `docs/evidence/phase-1.md` | none |
| Schema per `schema.md` (P2) | done, `9300b19` | 22 specs; two mutations caught (`cargo test -p elmdb schema`) | none |
| Format, storage, durability per D5–D7 (P3) | done | `docs/evidence/phase-3.md` | none |
| Writes and model harness (P4) | done | `docs/evidence/phase-4.md` | none |
| Reads and value indexes (P5) | done | `docs/evidence/phase-5.md` | none |
| Derived data, `check`, `rebuild`, schema changes (P6) | done | `docs/evidence/phase-6.md` | none |
| Admin, upgrade from format 4, CLI (P7) | done | `docs/evidence/phase-7.md` | none; the 17 duplicate short codes are the user's to resolve when converting the real data (D12) |
| Crash, failed-sync and fuzz suites (P8) | done, `68f6a96`, `a740ddd` | `docs/evidence/phase-8.md`; full matrix runs 40169607512 and 40170026515 | none |
| apack parity, differential harness agreeing (P9) | done, apack `fe5258646`, `9c7f96003` | `docs/evidence/phase-9.md`; the chain and test speed in `phase-10.md` | none |
| apack without lmdb-js (P10) | done, apack `974faa775` | `docs/evidence/phase-10.md` | none |
| `runs` database (P11) | done, apack `9c7f96003`, elmdb `6d337f6` | host `run-history.spec` (20), CLI `db.integration` (71, `import-runs` included), the backup round trip of both databases; the chain in `phase-10.md` | none |
| Text search, in elmdb and in apack (P12) | done, elmdb `c1a1cb1`; apack `a402744fb`, `ea09dbf84` | `docs/evidence/phase-12.md`; apack's whole `chain --no-native` passed cold on `d21514bdb` (elmdb `a0da984`) | none; PR #148 is reported to the user as superseded |
| Vector and hybrid search, library port tested and off (P13) | done, elmdb `90207fd`; apack `794249720` | `docs/evidence/phase-13.md`; apack's `chain --no-native` passed on it | none |
| Change sets, tracked reads and `engine.watch` (P14) | done, elmdb `fd0c0d7`; apack `cf810c91d` | `docs/evidence/phase-14.md`; apack's `chain --no-native` passed on it | none |
| Platforms in elmdb's CI, packaged app, apack's full chain, draft PR, strict chain (P15) | done, elmdb `907af80`; apack `e35176874`, `63bad6b9e` | `docs/evidence/phase-15.md`; elmdb runs 40202507275, 40240131963; apack's `chain --e2e` on `6100f8d64`; PR #276; strict chain 40247866928, macOS compiler 40244728980 | none |
| Docs and closure (P16) | done, elmdb `0062e93` and this closure; apack `55cb18256`, `16fb19661` | elmdb's `docs/architecture/` and roadmap say what was built; apack's reference guides describe the engine on elmdb; links repaired in both | none |

## Deferred

- **Publishing** `elmdb` to npm and crates.io, and a pinned release apack depends on: the user's step, after the goal.
- **Reserving the names, and the schema URL** (`schema.md`'s `$schema` is a placeholder on a domain nobody owns):
  decide before publishing.
- **Value compression** with per-type dictionaries: `roadmap.md`, Later.
- **LMDB 1.0** through `heed3`: when it's released and hannoy supports it.
- **Substring and regex search:** if text search leaves a gap.
- **Converting the user's real data:** D12.
- **Enabling the library search index:** D15.
- **Views, and moving systems' data broadcasts onto them:** apack's live-queries plan (`docs/plans/live-queries.md`),
  on its own branch, merged into a branch from this goal's by `goal-elmdb-next.md`'s Phase 2 (D30).

## Constraints

- **Commits:** commit as you go in logical checkpoints, in each repository separately, with conventional messages and
  no attribution lines. Stage only your own files and read `git diff --cached` first. Never tag, publish or create
  releases. Pushes, PRs and workflow dispatches are only D16's (apack) and D20's (elmdb).
- **apack's rules apply to apack's work:** its `CLAUDE.md`, `docs/AGENTS.md` and the user's memory. These include:
  - native and repeated binary execution runs in the Linux container, not on the Mac;
  - never kill processes by pattern;
  - never use `git stash`;
  - never launch the app without an isolated `APACK_USER_DATA_DIR`;
  - never edit version or release metadata.
- **Data:** never read or write the live app's data directory. Work on copies, and ask for a copy when none is at
  hand.
- **Measurements:** use apack's `npm run measure` on a quiet machine. Record date, machine and conditions with every
  figure. A figure in prose names the record it came from.
- **Tests:** a test that cannot fail is a gate or an assertion with a named firing edit (apack's `CLAUDE.md`). Do not
  loosen an assertion, a deadline or a tolerance to get a pass.
- **Failures:** on a failure, inspect the preserved output and reproduce the specific case. After a fix, run that case
  and its suite. A failure that recurs without a diagnosis is recorded in Current handoff with its evidence, and work
  continues on phases that don't depend on it. The goal stops only when everything left depends on it.
- **Design changes:** D18.
- **Docker** (OrbStack) must be running for apack's Linux container; start it with `open -a OrbStack` when it isn't.
