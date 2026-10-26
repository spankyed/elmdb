# Goal: elmdb after v1, measured and typed

Status: Phases 1 and 2 done, 2026-10-26: elmdb v1 and live queries are on apack's `master` (#278). Phase 3 next.

```text
# Goal: elmdb after v1, measured and typed

Implement elmdb/docs/goals/goal-elmdb-next.md. Read Scope, Current handoff, Decisions, Phases,
Acceptance and Constraints before continuing. Phase 1 is goal-elmdb.md, finished by its own
document. The directions the later phases build or design are in elmdb/docs/roadmap/ and
elmdb/docs/roadmap.md ("Next, in order"); apack's side of Phase 5 is
apack/monorepo/docs/plans/field-types-from-shapes.md. Phases 1 and 2 are done; apack's work for
Phases 3 to 10 is on one branch cut from master, under one PR (G7).

Work in two repositories, both under ~/Develop/Projects:
- elmdb: follow elmdb/CLAUDE.md and this goal's Constraints.
- apack/monorepo (apack): follow its CLAUDE.md, its docs/AGENTS.md and its user memory, in a
  worktree (never the main checkout).

Inspect current code before relying on a path or a figure here.

Finished when:
- Every row of Acceptance is met on current source, with the commits and the evidence recorded.
- Each design phase leaves its roadmap document designed, with the measurements it rests on.
- elmdb's docs and apack's reference docs describe what was built.
- Outcome is written, the goal is archived, and closure is committed.

Commit usable checkpoints as you go. Keep Current handoff current. Do not expand or reduce scope
without the user's authorization.
```

## Scope

**Outcome.** elmdb v1 finished, then made faster where a reading of apack's real work says it pays, typed by apack's
shapes, extended with value compression and substring search, and its larger directions designed well enough that a
later goal can build them.

**Deliverables:**

1. elmdb v1: `goal-elmdb.md` met and closed.
2. One apack branch: elmdb's branch with live-queries merged into it, reconciled and validated, with its PR.
3. A reading of what apack's user actions cost in calls to elmdb, and the loops it finds turned into set reads; and
   watches bounded under any write rate (G14).
4. The planner's three fixes and EXPLAIN.
5. Field types from apack's shapes.
6. Text postings in blocks, built if a reading calls for it.
7. Fewer reads per field write.
8. Value compression.
9. Substring and regex search.
10. Designs, with measurements, for the directions in `docs/roadmap/` not built here.

**Entry points:** elmdb's `crates/elmdb/src/engine/` (`reads.rs`, `entities.rs`, `text/`, `derive.rs`), `format/`,
`docs/architecture/` and `docs/roadmap/`; apack's `packages/apack-ears/` (`database.ts`, `query.ts`, `schema.ts`), the
SDK's codegen (`packages/apack-sdk/src/build/`), `bench/ears.bench.ts`, and `drive/` for driving the app.

## Current handoff

- **Phases 1 and 2 done** (2026-10-26):
  - elmdb v1: `goal-elmdb.md` closed (`docs/archive/goals/goal-elmdb.md`), and a review of elmdb and of the apack move
    fixed what it found (every target's CI green, run 40289842885).
  - The apack move and live queries landed together on `master` with #278 (`0e6d0ffce`): #277 merged in at
    `5d1de25d2`, an audit of every file found each difference intended, and the strict chain passed on every job
    (run 40326956990). #276 is closed, superseded by #278.
  - On `master` since: the format is 1 again, edited in place (G3; elmdb `14bd362`, apack `949b73f4a`); a startup the
    API can't finish shows the API's own error (`4fd8c17d4`); a pack toggle on a busy pack waits for its teardown or
    is refused out loud (`272c7b3d9`); the external-pack E2E waits for installs' and toggles' outcomes, not clocks.
    `npm run chain -- --no-native` passed on `dacf05e33` (2026-10-26).
- **Phase 3 next.** G7's branches are cut, both from their default branch's tip and pushed: apack's `session/elmdb-next`
  at `fc2586112`, in `~/Develop/Projects/AgentBuddy-worktrees.noindex/elmdb-next` (dependencies installed, elmdb
  linked, packages built), and elmdb's `session/elmdb-next` at `f7bc8a5`, in elmdb's own checkout. apack's PR is
  opened with its first checkpoint.
- **Databases:** the development machine's elmdb and lmdb-js databases (the development profile and every
  `apack-cli` profile) were wiped on 2026-10-26, so each next open makes a format 1 database; the installed app's
  data is the user's to convert, after a release.
- **Data:** G8's dump was made on 2026-10-26 from the backup, through the lmdb-js move path, which was then deleted:
  `~/Develop/backups/abuddy-backups/elmdb-dumps/`, its README beside it. A copy is loaded at
  `~/Develop/elmdb-next-scratch/prod` (22,816 entities, 45,798 relations, at elmdb `f7bc8a5`).
- **Measured so far (2026-10-25):** a cold boot of apack with its shipped pack makes 900 reads, 174 commits outside a
  unit and one unit of 470 writes, about 26 ms in elmdb of 2.3 s (`docs/roadmap/javascript-boundary.md`). A single
  row's read through the binding is about 2.8 µs (`a0da984`). An indexed `where` with a `limit` of 50 read all 13,000 of
  its matches, 32 ms (apack's `docs/plans/live-queries.md`, Step 1's readings).
- **Tracking's cost (2026-10-26, apack's bench, `watches`; load average about 5):** a 50-row page read through the
  binding is 77 µs plain and 148 µs as a watch's run reads it (its token untracked, the read tracked), so tracking
  about doubles a re-run's read, nearly all of it filing the read set, and the untrack is nearly free. The same read
  tracked again under one token without an untrack is 90 µs, filing nothing new: an owner files each key and range
  once (`TrackIndex::add`). Before that, each repeat filed its ranges again, 319 µs a read and growing.

## Decisions

Final unless marked open. Each cites the document that owns it; change the owner and this table together.

| # | Decision | Owner |
|---|---|---|
| G1 | Phase 1 comes first, then Phase 2; every later phase builds on G7's branch. Phases 4 and 5 touch what `goal-elmdb.md`'s Phases 13 and 14 change — `reads.rs`, which tracked reads record through, and apack's `schema.ts` and codegen — so none starts before Phase 1 closes. Phase 14 there builds only the engine's side of watch (its D30) | this goal |
| G2 | A phase marked gated builds only when its reading says so, by the rule the phase states. The reading is recorded either way, and a phase whose gate isn't met is done when its reading is | this goal |
| G3 | The format is 1 until a release has users: a change to stored data's layout edits format 1 in place, with no bump and no upgrade step, and a change to derived data (a text or trigram index) needs nothing, `rebuild()` building it. Our databases are reset or converted once on a copy, never at boot. From the first release with users, format 1 is locked and each later change bumps it with its upgrade step (`docs/architecture/format.md`, Versions; apack's `docs/archive/plans/storage-format-one.md`) | `format.md`; apack's `CLAUDE.md`, Backward compatibility |
| G4 | Every phase that changes the engine reads apack's `npm run bench -w @apackdev/ears` and the single-row read through the binding before and after. A case slower by more than 10% is investigated and fixed when the cause is the change; one that remains is recorded in Outcome with its cause | this goal (as `goal-elmdb.md`'s D17) |
| G5 | A finding that a decision here is wrong is acted on: the goal changes the decision and its owning document in one commit, with the evidence linked, and continues. Only removing a deliverable needs the user | this goal |
| G6 | The goal commits, pushes and opens PRs as it needs to, in both repositories: elmdb's work on G7's elmdb branch, pushed at each checkpoint, apack's work on G7's branch, pushed at each checkpoint, under the one PR opened for it against `master`, its description kept current as each phase lands. **CI dispatches are limited:** one dispatch of a workflow per phase that needs it — elmdb's full matrix when a phase changes the binding or its build, apack's strict chain and macOS compiler cases at the end — plus one rerun after a fix. A failure is diagnosed locally or in the Linux container, never by dispatching again | the user, 2026-10-25 |
| G7 | **One branch for the rest, in each repository.** Phase 2's branch landed on `master` with #278. apack's work for Phases 3 to 10 is on one new branch cut from `master`, `session/elmdb-next`, in a new worktree (`npm run session:new -- elmdb-next`), under one PR against `master`; `master`'s later commits come in by merging `master`. elmdb's is on `session/elmdb-next` cut from `main`, in elmdb's own checkout: no elmdb worktree, since each holds a cold `target/` of up to 11 GB. Every apack checkout links that one checkout (`vendor/elmdb`), so its binding is rebuilt only between runs, never under a chain or an app run. apack's pin names commits on elmdb's branch, which is merged into `main` (fast-forward, so the pinned commits keep their hashes) before apack's PR lands | the user, 2026-10-25 and 2026-10-26 |
| G8 | **Production-shaped data** is loaded from the dump of the app's backup of 2026-10-17 (`~/Develop/backups/abuddy-backups/elmdb-dumps/agentbuddy-backup-2026-10-17T23-06-18-232Z.elmdb-dump`) by `elmdb load`, into a scratch directory outside both repositories, and is never committed; the dump and the backup are only read. It holds the backup's data in its own contract, with no schema (the README beside it). What a phase's schema refuses on its copy (the 17 duplicate short codes `docs/evidence/phase-7.md` found) is resolved on the copy and recorded | the dump's README; `format.md`, Dumps |
| G9 | **Real chat turns** are measured, at most ten per reading, through the provider or CLI the app is configured with, on the user's existing login, in an app on an isolated data directory. A faked model would hide the streaming writes being measured. With no working login, Phase 3 measures flow runs and records why the chat turn is missing | this goal |
| G10 | **One writer.** The goal runs after `goal-elmdb.md` closes, as the only session editing elmdb and apack's worktree. Changes it finds that aren't its own are left alone and recorded in Current handoff | this goal |
| G11 | **Regexes are Rust's `regex` syntax**, RE2's family: linear in the text, with no lookaround or backreferences. A pattern using one is refused naming it. `search.md` documents the syntax | this goal |
| G12 | **The run stops and reports** only when everything left depends on a failure it can't diagnose (Constraints, Failures), when a step would write or convert the user's real data rather than a copy, or when a deliverable would have to be removed (G5). A format change doesn't stop it: converting the user's data is the user's step | this goal |
| G13 | **Live queries stay their session's work.** apack's live queries (views, `useView`, the features moved onto them; the plan is archived at `docs/archive/plans/live-queries.md`) landed on `master` with #277 and #278. This goal changes them only where its own work makes it, and later changes to them reach G7's branch by merging `master`. What this goal changes beneath them keeps `engine.watch`'s contract: every phase that changes how the engine reads or writes keeps the watch specs and the model harness's watch property passing | `goal-elmdb.md`'s D30; G7 |
| G14 | **Watches are bounded by the scheduler, not by writers.** Live queries decided on no minimum re-run interval because the busiest writer it knew, the stream writer, throttles itself to 80 ms (`docs/archive/plans/live-queries.md`, Risks). Nothing holds other writers to that: a flow, an import or a pack can commit on separate turns as fast as it runs, and each such commit re-runs every watch whose read set it touched, in the process that runs chats and flows. So the bound is the watch scheduler's (`createWatches`, `packages/apack-ears/src/watch.ts`), the one place every re-run passes, and holds for any writer: a watch converges (the last commit's result is always delivered), is stale by at most a bounded delay, costs at most a bounded share of the process, and doesn't starve another. Supersedes live queries' decision | the user, 2026-10-26 |

## Phases

### Phase 1: elmdb v1

- **Deliverable:** `goal-elmdb.md`'s remaining phases (13–16) and its closure, by that document.
- **Done when:** its Outcome is written and it is archived.

### Phase 2: one branch, live queries merged

- **Deliverable:**
  - A branch cut from `session/ears-on-elmdb`'s tip after `goal-elmdb.md` closes, in a new worktree, and
    `live-queries` merged into it at its latest commit — recorded by its hash, with what `live-queries` held then
    (its plan's Status line). Work the live-queries session hasn't committed isn't merged: it comes in later (G13).
  - The conflicts reconciled, each by what the two branches meant:
    - **The engine's watch is elmdb's.** `engine.watch`, `lastCommit()`, `watchesSettled()` and `watchStats()` on
      elmdb (`goal-elmdb.md`'s Phase 14) are kept; live-queries' JavaScript-engine internals — the read recorder and
      write log in `kv.ts` and `store-access.ts`, `read-set-index.ts` and its spec, its edits to `lmdb/store.ts` — are
      dropped with the engine they belong to. `watch.spec.ts` and the harness's watch property keep every case either
      branch has, run against elmdb.
    - **Views and the features moved onto them are live-queries'**, taken as they are: the SDK's, the host's, the
      API's and the renderer's views, and prompts, actions, flows and notes on them.
    - **A feature both branches changed** keeps live-queries' shape — rows from views, the machine holding only UI
      state — with elmdb's branch's change applied within it (its search in the threads panel, the library's vector
      index, the bookmark fixes).
    - **Recorded files are regenerated, never merged by hand:** the API reports (`npm run api:update`), the schema
      (`schema:update`), the facade report (`facade:update -w @app/default-setup`), content-parity's goldens when only
      hashes moved (`content-parity:update`), `package-lock.json` (`npm install`), and each regenerated diff read.
    - **Guides and docs** keep both branches' meaning; live-queries' plan is merged, not rewritten.
  - The merged branch pushed, and one PR opened for it: stacked on `session/ears-on-elmdb`'s PR while that is open, so
    its diff is the merge, and retargeted to `master` when that lands.
- **Dependencies:** Phase 1.
- **Done when:**
  - The merge commit names `live-queries`' hash, and lists each conflicted file with how it was resolved.
  - live-queries' specs pass on elmdb: the view host's, the API's views transport, the SDK's views, the codegen cases,
    and each migrated feature's two-client spec; and so do `watch.spec.ts` and the model harness with its watch
    property.
  - apack's full `npm run chain` passes on the branch (the native step in the Linux container).
  - The message-search driving script (`drive/message-search.ts`) and a driving script of a migrated feature's view
    both run against the merged app.
  - The PR is open, and Current handoff names the branch and its worktree.

### Phase 3: what a user action costs

- **Deliverable:**
  - A count of a user action's calls to elmdb, in development only: reads, writes outside a unit, units and their
    writes, and the time spent in each, by call site. Kept at the one place every call crosses
    (`createConnection`, `packages/apack-ears/src/database.ts`), off unless asked for, and read through `dev:eval`.
  - Readings of a chat turn (a message sent and its reply streamed) and a flow run, driven from `drive/`, each counting
    the views' re-runs too: a write re-runs every view whose read set it touched.
  - **Watches bounded under any write rate (G14)**, in `createWatches` and nowhere a view or a writer has to opt in:
    - **Latest wins:** a watch has at most one run pending; commits arriving meanwhile mark it dirty, and intermediate
      states are skipped.
    - **Cost-proportional delay:** a watch that just ran waits a gap proportional to its last run's time before running
      again, so a cheap watch stays immediate and an expensive one backs off.
    - **A budget per flush:** a flush spends at most a fixed slice on re-runs, and the watches left run next, in turn.
    - **Visible:** per watch, runs a second, time a run and commits skipped, in the count above; and a development
      warning naming a watch that keeps exceeding its budget.
    - The two constants (the delay's factor, the flush's slice) set by a stress reading: a writer committing on separate
      turns as fast as it runs into a hot table, with cheap and expensive views open over it (the indexed `where` with a
      `limit` among them), recording re-runs a second, the share of a core, event-loop lag and how late a window hears.
  - Each loop of one-entity reads or unit-less writes the readings show costing a measurable share of its action,
    turned into a set read (`query.ids(list)`, `pickAll`) or grouped (`transact`, `setMany`) where it is.
- **Dependencies:** Phase 2.
- **Done when:**
  - Both readings, before and after the fixes, are recorded with date and conditions.
  - The count's spec shows it counting a read, a write and a unit, and nothing when off.
  - Specs on a fake clock show G14's guarantees: a thousand commits on separate turns cost a bounded number of re-runs
    and deliver the last result; an expensive watch doesn't delay a cheap one; a watch idle when a commit lands runs at
    once. The model harness's watch property shows each watch's last delivered value equal to a fresh read.
  - The stress reading is recorded before and after the scheduler, with the constants it set.

### Phase 4: the planner's three fixes, and EXPLAIN

- **Deliverable (`docs/roadmap/plan-algebra.md`, First, without bitmaps):**
  - In elmdb:
    - any `where` in a chain uses a built index of its field, not only one straight after a type start;
    - `orderBy` followed by `limit` keeps a heap of the first `limit`;
    - an index lookup visits its matches as it finds them;
    - EXPLAIN: a read asked to explain answers its plan's stages — the index each read, the entities each visited and
      kept, and its time — beside its answer.
  - In apack: the query builder's `explain()` and the Database console showing it.
- **Dependencies:** Phase 2.
- **Done when:**
  - The model harness passes, its watch property included, so answers and what a watch is told are unchanged.
  - Specs show each fix by its visits: an indexed `where` with a `limit` of 50 over 13,000 matches visits about 50.
  - EXPLAIN's answer is specified and shown on a plan of each kind.
  - G4's readings are recorded.

### Phase 5: field types from apack's shapes

- **Deliverable:** apack's `docs/plans/field-types-from-shapes.md`, its four steps.
- **Dependencies:** Phase 2.
- **Done when:** the plan's steps are done, the audit's findings and their fixes are recorded, and apack's
  `npm run chain -- --no-native` passes.

### Phase 6: text postings in blocks (gated)

- **Deliverable (`docs/roadmap/text-index-at-scale.md`):**
  - The reading: a message write's commit time, and a fuzzy search's time, at 1,000, 10,000 and 50,000 messages,
    added to apack's bench as text cases.
  - **The gate:** posting lists in blocks are built if the write's time at 50,000 messages is more than twice its time
    at 1,000; fuzzy terms by seeking, if the fuzzy search's is.
  - What is built keeps search's specs and the model harness passing; existing text indexes are rebuilt (G3: derived
    data, no upgrade step).
- **Dependencies:** Phase 2.
- **Done when:** the readings are recorded, and what the gate called for is built, with the readings after it.

### Phase 7: fewer reads per field write

- **Deliverable:** the reads a field write makes (`put_field` and `delete_field`, `entities.rs`: the value before, the
  other fields of a compound index, the undo record, a text index's refile) counted, and those it doesn't need removed.
- **Dependencies:** Phase 2.
- **Done when:** a spec counts a field write's reads before and after, the model harness passes, and G4's readings
  show the writes' cases.

### Phase 8: value compression

- **Deliverable (`docs/architecture/format.md`, Values):**
  - A value above a threshold stored compressed with zstd, marked by a tag byte no msgpack value starts with, and
    decompressed where the engine reads it; the threshold chosen by measuring size and read time on a copy of
    production-shaped data.
  - Format 1 changed in place, with no bump and no upgrade step (G3).
  - apack's `docs/plans/ears-value-compression.md`, written for the engine before elmdb, archived with this as its
    successor.
- **Dependencies:** Phase 2.
- **Done when:**
  - Size on disk and the readings of long reads are recorded before and after, on the copy.
  - `check()` and `rebuild()` pass over compressed values, and our databases are reset or converted once, on a copy:
    dumped by the build before the change and loaded by the build after (`format.md`, Dumps). G8's dump needs nothing.

### Phase 9: substring and regex search

- **Deliverable (`docs/architecture/search.md`, Methods):**
  - In elmdb: a trigram index narrowing candidates, then a check of each, for error text, ids and code in messages;
    how a schema declares it, decided and written into `schema.md`.
  - In apack: the manifest's declaration and the query builder's step, and default-setup's Message declaring it.
- **Dependencies:** Phase 2.
- **Done when:**
  - Match sets equal a scan's on a fixed corpus and in the model harness, for substrings and for regexes.
  - `check()` and `rebuild()` cover the index.

### Phase 10: designs

- **Deliverable:** each of these roadmap documents taken from idea to design: the format and API concrete, the
  measurements that justify building it taken and recorded, the risks named, and the work as phases with done-whens a
  later goal can run.
  - `plan-algebra.md` (bitmaps for value indexes, one algebra over them);
  - `change-log.md` (incremental backup);
  - `incremental-views.md`;
  - `javascript-boundary.md` (reads without a decode, units across an `await`);
  - value compression's per-type dictionaries, in `format.md`'s Values or a roadmap document of its own.
- **Dependencies:** Phases 3–9, whose readings they use.
- **Done when:** each document says "Status: designed", with its measurements and phases.

## Acceptance

| Requirement | Implementation / source | Evidence | Remaining work |
|---|---|---|---|
| elmdb v1 (P1) | done | `docs/archive/goals/goal-elmdb.md`; elmdb `8158696`, CI run 40289842885; PR #276 | |
| One branch, live queries merged, its PR open (P2) | done | PR #278, merged as `0e6d0ffce`; merges `97513f457`, `5d1de25d2`; strict chain run 40326956990. The two driving scripts the done-when names were run and not committed: neither is in apack's history | |
| A user action's calls to elmdb, its loops fixed, and watches bounded (P3) | missing | | all |
| The planner's three fixes and EXPLAIN (P4) | missing | | all |
| Field types from apack's shapes (P5) | missing | | all |
| Text postings in blocks, gated (P6) | missing | | all |
| Fewer reads per field write (P7) | missing | | all |
| Value compression (P8) | missing | | all |
| Substring and regex search (P9) | missing | | all |
| Designs (P10) | missing | | all |

## Constraints

- **Commits:** commit as you go in logical checkpoints, in each repository separately, with conventional messages and
  no attribution lines. Stage only your own files and read `git diff --cached` first. Never tag, publish or create
  releases. Pushes are only G6's.
- **apack's rules apply to apack's work:** its `CLAUDE.md`, `docs/AGENTS.md` and the user's memory. These include:
  - native and repeated binary execution runs in the Linux container, not on the Mac;
  - never kill processes by pattern;
  - never use `git stash`;
  - never launch the app without an isolated `APACK_USER_DATA_DIR`;
  - never edit version or release metadata.
- **Data:** never read or write the live app's data directory. Work on copies, and ask for a copy when none is at
  hand.
- **Measurements:** use apack's `npm run measure` on a quiet machine (`npm run check:idle`); on a busy one, alternate
  the builds being compared and say so. Record date, machine and conditions with every figure. A figure in prose names
  the record it came from.
- **Tests:** a test that cannot fail is a gate or an assertion with a named firing edit (apack's `CLAUDE.md`). Do not
  loosen an assertion, a deadline or a tolerance to get a pass.
- **Failures:** on a failure, inspect the preserved output and reproduce the specific case. After a fix, run that case
  and its suite. A failure that recurs without a diagnosis is recorded in Current handoff with its evidence, and work
  continues on phases that don't depend on it. The goal stops only when everything left depends on it.
- **Design changes:** G5.
- **Docker** (OrbStack) must be running for apack's Linux container; start it with `open -a OrbStack` when it isn't.
