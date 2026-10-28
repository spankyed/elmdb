# Goal: elmdb after v1, measured and typed

Status: Phases 1 and 2 done, 2026-10-26: elmdb v1 and live queries are on apack's `master` (#278). The review's remediation landed 2026-10-27; Phases 3 and 4 done the same day (`docs/evidence/next-phase-3.md`, `next-phase-4.md`), Phases 5 to 8 the same day (`next-phase-5.md` to `next-phase-8.md`). Phase 9 next.

```text
# Goal: elmdb after v1, measured and typed

Implement elmdb/docs/goals/goal-elmdb-next.md. Read Scope, Current handoff, Decisions, Phases,
Acceptance and Constraints before continuing. Phase 1 is goal-elmdb.md, finished by its own
document. The directions the later phases build or design are in elmdb/docs/roadmap/ and
elmdb/docs/roadmap.md ("Next, in order"); apack's side of Phase 5 is
apack/monorepo/docs/archive/plans/field-types-from-shapes.md. Phases 1 and 2 are done; Phases 3 to 10 run on
G7's branches, apack's in its main checkout under one PR and elmdb's in its own checkout.

Work in two repositories, both under ~/Develop/Projects:
- elmdb: follow elmdb/CLAUDE.md and this goal's Constraints, on G7's elmdb branch.
- apack/monorepo (apack): follow its CLAUDE.md, its docs/AGENTS.md and its user memory, in its
  main checkout, on G7's apack branch.

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
shapes, extended with value compression and substring search, its watches bounded under any write rate, and its larger
directions designed well enough that a later goal can build them.

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
`docs/architecture/` and `docs/roadmap/`; apack's `packages/apack-ears/` (`database.ts`, `query.ts`, `schema.ts`, `watch.ts`), the
SDK's codegen (`packages/apack-sdk/src/build/`), `bench/ears.bench.ts`, and `drive/` for driving the app.

## Current handoff

- **Phases 1 and 2 done** (2026-10-26): `goal-elmdb.md` is closed (`docs/archive/goals/goal-elmdb.md`), and the apack
  move and live queries landed together on `master` with #278 (`0e6d0ffce`; strict chain run 40326956990).
- **On `master` since**, each with its spec:
  - the format is 1 again, edited in place (G3; elmdb `14bd362`, apack `949b73f4a`);
  - the lmdb-js move path deleted, the backup kept as an elmdb dump (elmdb `c51e0f1`, apack `6f88ed34d`);
  - tracked reads file each range once, and a failed snapshot files the whole store (elmdb `3078c09`, `f7bc8a5`;
    apack pins `f7bc8a5`);
  - the bench measures tracking again (apack `93a98950f`);
  - a write nobody asked for that fails is reported (`fc2586112`);
  - the Brain surface no longer floods the brain: its state is one event, `BRAIN_STATE` (`ade7d000d`). Before it, a
    window asked for the run tree about 500 times a second while the brain ran, which would have swamped any reading.
- **Remediation landed** (2026-10-27): the review's R01–R08 and H01–H04 (`docs/reviews/elmdb-review-2026-10.md`;
  goal archived at `docs/archive/goals/goal-elmdb-remediation.md`), elmdb #1 and #2 into `main` (`9359949`), apack
  #283 and #284 into `master` (`5838145675`), which pins elmdb `c345182`.
- **Branches (G7):** apack's `session/elmdb-next`, cut from `master` at `5838145675`, checked out in apack's main
  checkout (`~/Develop/Projects/apack/monorepo`), linked to elmdb's checkout with the binding built at the pin, under
  PR #285; elmdb's `session/elmdb-next` cut again from `main` (`9359949`), in its own checkout. G8's copy at elmdb
  `9359949`: `check()` finds no derived differences and the 17 known duplicate short codes.
- **Phase 3 done** (2026-10-27): the call counter (`admin.calls`, apack `48fbb9c3d1`) and the watch scheduler
  (`WATCH_SCHEDULE`, `19214e4d38`), each with its specs; the readings, the stress reading and G4's bench in
  `docs/evidence/next-phase-3.md`. No loop the readings show reached a measurable share of its action: elmdb is under
  1% of a chat turn's wall time.
- **Phase 4 done** (2026-10-27): the planner's three fixes and EXPLAIN (elmdb `f8dc82e`), apack's `explain()` and the
  Database console's stage table (`d4db390d0f`, pinning `f8dc82e`); readings and mutation checks in
  `docs/evidence/next-phase-4.md`. elmdb's PR is #3 (G6: merged with its hashes kept, before apack's #285).
- **Validation:** apack's `npm run chain -- --no-native` passed over Phases 3 and 4 at `d4db390d0f` (run
  `2026-10-27T23-57-59Z-32736`, 1,047 s). Not yet run: `test:native`, and G6's strict chain and macOS compiler cases at
  the end.
- **Phase 5 done** (2026-10-27): field types from apack's shapes (apack `914316d725`; the plan archived with its
  Outcome, `docs/archive/plans/field-types-from-shapes.md`); the audit and the writers fixed in
  `docs/evidence/next-phase-5.md`. Its chain (`npm run chain -- --no-native`, run `2026-10-28T01-06-34Z-79939`) passed
  every step but `test:integration`, which was terminated (exit 143) with no case failing, while the machine ran another
  project's tests at a load average of 32 to 53 on ten cores with 8 GiB of disk free; `test:integration` passed over the
  same Phase 5 code in the run before (`2026-10-28T00-49-17Z-38419`), before the one-line threads repository fix, and
  passed alone at `914316d725` (`npm run test:integration`: 60 files, 788 cases), so every step has passed over it.
- **Disk:** the data volume runs near full. `apack-authoring-*` work dirs of checkouts that no longer exist are never
  swept, the sweep in apack's `tests/scripts/lib/authoring.sh` failing silently on the read-only compiler store inside
  each; 30 of them (about 15 GB) were removed on 2026-10-27 with the user's agreement. Not this goal's to fix.
- **Phase 6 done** (2026-10-27): apack's text bench (`2df2961774`); fuzzy terms by seeking built (elmdb `e4ff088`, a rare
  word's fuzzy search at 50,000 messages 16.7 ms to 1.24 ms); posting lists in blocks built, measured at two sizes, no
  change to a write, and reverted; readings and the gate's correction in `docs/evidence/next-phase-6.md`.
- **Phase 7 done** (2026-10-27): a field write reads what it replaces once and the entity's place once (elmdb `c3c8021`):
  2 reads to 1, 5 to 4 and 23 to 13 for an unindexed, a value-indexed and a text-indexed field; `docs/evidence/next-phase-7.md`.
- **Phase 8 done** (2026-10-27): field values of 256 bytes or more stored zstd-compressed when smaller (elmdb `933aa19`,
  apack pins it at `6d14be5b24`); G8's copy compacts to 122.1 MB from 267.8 MB; PR #3's eight targets pass;
  `docs/evidence/next-phase-8.md`. A watch scheduler hang found on the way, fixed in apack `a1b6603e5a`.
- **Next action:** Phase 9, substring and regex search.
- **Taking a reading:**
  - Driving scripts are local: `drive/` is gitignored but for its README and config, so a reading's evidence is what
    is recorded (Constraints, Evidence).
  - A reading of the app is taken against a held session, `npm run dev:open -- --fresh --rm` (a development app on a
    throwaway profile), with one-shots: `dev:query` runs console code in the app's backend (`engineCalls.start()`,
    `engineCalls.stop()`), `dev:eval` in its window (sending the threads surface `SEND_MESSAGE` or `SEND_COMMAND`).
    A `drive/` script's app is a test app and gets no `engineCalls`. Stopping the session's command by its task, rather
    than with Ctrl-C, leaves the throwaway profile behind: `apack profiles rm <name>` removes it.
  - **Known bug, open:** a message sent before the threads surface has its chat start goes out with `mode: ''`, is
    saved and answered by no flow (the Claude Code flow runs only on `mode == 'Claude Code'`). A chat-turn script waits
    for the surface's mode; the fix (the surface holding the send, or the backend refusing a mode-less message) is the
    user's call.
- **Data:** G8's dump is at `~/Develop/backups/abuddy-backups/elmdb-dumps/`, its README beside it; a copy is loaded at
  `~/Develop/elmdb-next-scratch/prod` (22,816 entities, 45,798 relations, elmdb `f7bc8a5`). The development machine's
  databases were wiped on 2026-10-26; the installed app's data is the user's to convert after a release.
- **Measured so far:**
  - 2026-10-25: a cold boot of apack makes 900 reads, 174 commits outside a unit and one unit of 470 writes, about
    26 ms in elmdb of 2.3 s (`docs/roadmap/javascript-boundary.md`); a single row's read through the binding is about
    2.8 µs (`a0da984`); on the lmdb-js engine, an indexed `where` with a `limit` of 50 read all 13,000 of its matches,
    32 ms (apack's `docs/archive/plans/live-queries.md`, Step 1's readings).
  - 2026-10-26, apack's bench (`watches`, load average about 5): a 50-row page read is 77 µs plain and 148 µs as a
    watch's re-run reads it; nearly all the difference is filing the read set again, since a re-run untracks and
    files everything anew. Filing only what changed between runs would save about 58 µs of each; considered, not
    scheduled, until Phase 3's readings say whether it matters.

## Decisions

Final unless marked open. Each cites the document that owns it; change the owner and this table together.

| # | Decision | Owner |
|---|---|---|
| G1 | Phases 1 and 2 came first and are done; every later phase builds on G7's branches, in any order its dependencies allow | this goal |
| G2 | A phase marked gated builds only when its reading says so, by the rule the phase states. The reading is recorded either way, and a phase whose gate isn't met is done when its reading is | this goal |
| G3 | The format is 1 until a release has users: a change to stored data's layout edits format 1 in place, with no bump and no upgrade step, and a change to derived data (a text or trigram index) needs nothing, `rebuild()` building it. Our databases are reset or converted once on a copy, never at boot. From the first release with users, format 1 is locked and each later change bumps it with its upgrade step (`docs/architecture/format.md`, Versions; apack's `docs/archive/plans/storage-format-one.md`) | `format.md`; apack's `CLAUDE.md`, Backward compatibility |
| G4 | Every phase that changes the engine reads apack's `npm run bench -w @apackdev/ears` and the single-row read through the binding before and after. A case slower by more than 10% is investigated and fixed when the cause is the change; one that remains is recorded in Outcome with its cause | this goal (as `goal-elmdb.md`'s D17) |
| G5 | A finding that a decision here is wrong is acted on: the goal changes the decision and its owning document in one commit, with the evidence linked, and continues. Only removing a deliverable needs the user | this goal |
| G6 | The goal commits, pushes and opens PRs as it needs to, in both repositories: elmdb's work on G7's elmdb branch, pushed at each checkpoint, apack's work on G7's branch, pushed at each checkpoint, under the one PR opened for it against `master`, its description kept current as each phase lands. **CI dispatches are limited:** one dispatch of a workflow per phase that needs it — elmdb's full matrix when a phase changes the binding or its build, apack's strict chain and macOS compiler cases at the end — plus one rerun after a fix. A failure is diagnosed locally or in the Linux container, never by dispatching again | the user, 2026-10-25 |
| G7 | **One branch for the rest, in each repository.** Phase 2's branch landed on `master` with #278. apack's work for Phases 3 to 10 is on one new branch cut from `master`, `session/elmdb-next`, in apack's main checkout, under one PR against `master`; `master`'s later commits come in by merging `master`. elmdb's is on `session/elmdb-next` cut from `main`, in elmdb's own checkout: no elmdb worktree, since each holds a cold `target/` of up to 11 GB. Every apack checkout links that one checkout (`vendor/elmdb`), so its binding is rebuilt only between runs, never under a chain or an app run. apack's pin names commits on elmdb's branch, which is merged into `main` (fast-forward, so the pinned commits keep their hashes) before apack's PR lands | the user, 2026-10-25, 2026-10-26 and 2026-10-27 (apack's main checkout) |
| G8 | **Production-shaped data** is loaded from the dump of the app's backup of 2026-10-17 (`~/Develop/backups/abuddy-backups/elmdb-dumps/agentbuddy-backup-2026-10-17T23-06-18-232Z.elmdb-dump`) by `elmdb load`, into a scratch directory outside both repositories, and is never committed; the dump and the backup are only read. It holds the backup's data in its own contract, with no schema (the README beside it). What a phase's schema refuses on its copy (the 17 duplicate short codes `docs/evidence/phase-7.md` found) is resolved on the copy and recorded | the dump's README; `format.md`, Dumps |
| G9 | **Real chat turns** are measured, at most ten per reading, through the provider or CLI the app is configured with, on the user's existing login, in an app on an isolated data directory. A faked model would hide the streaming writes being measured. With no working login, Phase 3 measures flow runs and records why the chat turn is missing | this goal |
| G10 | **One writer.** The goal runs after `goal-elmdb.md` closes, as the only session editing elmdb's checkout and apack's main checkout. Changes it finds that aren't its own are left alone and recorded in Current handoff | this goal |
| G11 | **Regexes are Rust's `regex` syntax**, RE2's family: linear in the text, with no lookaround or backreferences. A pattern using one is refused naming it. `search.md` documents the syntax | this goal |
| G12 | **The run stops and reports** only when everything left depends on a failure it can't diagnose (Constraints, Failures), when a step would write or convert the user's real data rather than a copy, or when a deliverable would have to be removed (G5). A format change doesn't stop it: converting the user's data is the user's step | this goal |
| G13 | **Live queries are `master`'s.** apack's live queries (views, `useView`, the features moved onto them; the plan archived at `docs/archive/plans/live-queries.md`) landed with #277 and #278. This goal changes them only where its own work makes it (G14), and keeps `engine.watch`'s contract: every phase that changes how the engine reads or writes keeps the watch specs and the model harness's watch property passing | `goal-elmdb.md`'s D30 |
| G14 | **Watches are bounded by the scheduler, not by writers.** Live queries decided on no minimum re-run interval because the busiest writer it knew, the stream writer, throttles itself to 80 ms (`docs/archive/plans/live-queries.md`, Risks). Nothing holds other writers to that: a flow, an import or a pack can commit on separate turns as fast as it runs, and each such commit re-runs every watch whose read set it touched, in the process that runs chats and flows. So the bound is the watch scheduler's (`createWatches`, `packages/apack-ears/src/watch.ts`), the one place every re-run passes, and holds for any writer: a watch converges (the last commit's result is always delivered), is stale by at most a bounded delay, costs at most a bounded share of the process, and doesn't starve another. Supersedes live queries' decision | the user, 2026-10-26 |

## Phases

### Phase 1: elmdb v1

- **Deliverable:** `goal-elmdb.md`'s remaining phases (13–16) and its closure, by that document.
- **Done when:** its Outcome is written and it is archived.

### Phase 2: one branch, live queries merged

- **Deliverable:** elmdb's apack branch with `live-queries` merged into it, reconciled (the engine's watch elmdb's, the
  views and the features moved onto them live-queries'), validated and landed with its PR.
- **Done:** #278, merged as `0e6d0ffce` (Acceptance).

### Phase 3: what a user action costs

- **Deliverable:**
  - A count of a user action's calls to elmdb, in development only: reads, writes outside a unit, units and their
    writes, and the time spent in each, by call site. Kept at the one place every call crosses
    (`createConnection`, `packages/apack-ears/src/database.ts`), off unless asked for, and read through `dev:query` (console code's `engineCalls`).
  - Readings of a chat turn (a message sent and its reply streamed) and a flow run, driven through `drive`'s one-shots, each counting
    the views' re-runs too: a write re-runs every view whose read set it touched.
  - **Watches bounded under any write rate (G14)**, in `createWatches` and nowhere a view or a writer has to opt in:
    - **Latest wins:** a watch has at most one run pending; commits arriving meanwhile mark it dirty, and intermediate
      states are skipped.
    - **Cost-proportional delay:** a watch that just ran waits a gap proportional to its last run's time, and at least a
      frame, before running again, so a cheap watch stays immediate and an expensive one backs off. The floor is what
      bounds a watch whose run costs nearly nothing, which a gap proportional to its cost alone would not.
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

- **Deliverable:** apack's `docs/archive/plans/field-types-from-shapes.md`, its four steps.
- **Dependencies:** Phase 2.
- **Done when:** the plan's steps are done, the audit's findings and their fixes are recorded, and apack's
  `npm run chain -- --no-native` passes.

### Phase 6: text postings in blocks (gated)

- **Deliverable (`docs/roadmap/text-index-at-scale.md`):**
  - The reading: a message write's commit time, and a fuzzy search's time, at 1,000, 10,000 and 50,000 messages,
    added to apack's bench as text cases, over a dictionary that grows with the log.
  - **The gate:** posting lists in blocks are built if the write's time at 50,000 messages is more than twice its time
    at 1,000; fuzzy terms by seeking, if a rare word's fuzzy search's is. A common word's fuzzy search ranks its
    thousands of matches, whose growth is block-max scores' (G5: the first reading's fuzzy case was a common word, and
    grew no more than its exact search; corrected 2026-10-27, `docs/evidence/next-phase-6.md`). What the gate calls
    for is built and measured, and kept only where the reading after it improves (blocks, measured, did not).
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
  - apack's `docs/archive/plans/ears-value-compression.md`, written for the engine before elmdb, is this phase's
    predecessor, superseded by it.
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
| A user action's calls to elmdb, its loops fixed, and watches bounded (P3) | done | apack `48fbb9c3d1` (count), `19214e4d38` (scheduler), PR #285; `docs/evidence/next-phase-3.md`: chat turn and flow run before and after, the stress reading before and after with the constants, G4's bench; specs `calls.spec.ts`, `watch-schedule.spec.ts`, the model harness | the strict chain, with G6's at the end |
| The planner's three fixes and EXPLAIN (P4) | done | elmdb `f8dc82e`, PR #3; apack `d4db390d0f`, PR #285; `docs/evidence/next-phase-4.md`: visits on each fix, EXPLAIN on a plan of each kind, mutation checks, G4's bench and single-row read | the strict chain, with G6's at the end |
| Field types from apack's shapes (P5) | done | apack `914316d725`, PR #285; `docs/evidence/next-phase-5.md`: the mapping, the schema, the audit on G8's copy, the writers fixed | the strict chain, with G6's at the end |
| Text postings in blocks, gated (P6) | done | apack `2df2961774`, elmdb `e4ff088`; `docs/evidence/next-phase-6.md`: the reading at three sizes, the gate's correction (G2, G5), fuzzy seeking built with its readings after, blocks measured at two sizes and not built | the strict chain, with G6's at the end |
| Fewer reads per field write (P7) | done | elmdb `c3c8021`; `docs/evidence/next-phase-7.md`: the spec counting a field write's reads before and after, the model harness, G4's bench | the strict chain, with G6's at the end |
| Value compression (P8) | done | elmdb `933aa19`, PR #3 (eight targets pass); apack `6d14be5b24`; `docs/evidence/next-phase-8.md`: size and long reads before and after on G8's copy, the threshold chosen, `check()` and `rebuild()` over compressed values, G4 | the strict chain, with G6's at the end |
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
- **Evidence:** each phase's readings go in elmdb's `docs/evidence/next-phase-<n>.md`; the `phase-<n>.md` files there
  are `goal-elmdb.md`'s.
- **Tests:** a test that cannot fail is a gate or an assertion with a named firing edit (apack's `CLAUDE.md`). Do not
  loosen an assertion, a deadline or a tolerance to get a pass.
- **Failures:** on a failure, inspect the preserved output and reproduce the specific case. After a fix, run that case
  and its suite. A failure that recurs without a diagnosis is recorded in Current handoff with its evidence, and work
  continues on phases that don't depend on it. The goal stops only when everything left depends on it.
- **Design changes:** G5.
- **Docker** (OrbStack) must be running for apack's Linux container; start it with `open -a OrbStack` when it isn't.
