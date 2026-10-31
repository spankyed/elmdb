# Roadmap

The order of work. Each step ships something apack uses. Status: steps 1–7 built (elmdb's goal, Phases 1 and 9–14,
and goal-elmdb-next's Phases 3–9; `docs/evidence/`); what is designed and waiting is below them.

## 1. Spike: what the protocol costs

Built (`docs/evidence/phase-1.md`).

Whether to build is decided: apack is moving off lmdb-js, and search needs indexes that commit with the data. The spike
measures the one cost the design adds, encoding a result and decoding it in JavaScript, against apack today on a copy
of its converted production database (format 4, 406,969 entries, 22,816 entities):

- every `Message` row (19,803 entities), about 540 ms in apack today;
- `getRow` of 2,000 entities across types;
- an indexed `where`, a type scan with `count` and `first`, and a relation walk.

Each warm and cold (the copy on a disk image detached and attached again), medians of five, on a machine checked quiet
first. If the plan with decoding isn't faster than today on the long read, the result envelope is redesigned before
step 2.

## 2. Parity, and apack off lmdb-js

Built (`docs/evidence/phase-9.md`, `phase-10.md`).

The graph, value indexes, units, savepoints, numbering, refusals and durability, behind apack's unchanged `Ears` API.
apack's model harness drives its JavaScript engine and elmdb with the same workloads and compares both with the model
and with each other, and the crash harness runs against elmdb. apack switches when they agree, and drops lmdb-js.
apack's side is `docs/plans/ears-on-elmdb.md` in apack.

## 3. The `runs` database

Built (Phase 11, in elmdb's goal).

apack's run history moves from its record store into a second database. `RecordStore` goes away.

## 4. Text search

Built (`docs/evidence/phase-12.md`).

Ranked full text with filters and highlights, then fuzzy labels for a quick-open (`search.md`).

## 5. Change sets and watch

Built (`docs/evidence/phase-14.md`).

A commit sequence on every change set, and tracked reads whose tokens a commit names (`engine.md`, Change sets and
watch): apack's `engine.watch` runs on them, under the contract of its live queries, whose views are apack's.

## 6. Vector and hybrid search

Built (`docs/evidence/phase-13.md`).

Vector indexes on hannoy, pending embeddings, `near`, `like` and `hybrid`; apack's dormant library index ported onto
them.

## 7. After v1, measured and typed

Built (goal-elmdb-next, Phases 3–9; `docs/evidence/next-phase-3.md` to `next-phase-9.md`).

A user action's calls counted and watches bounded under any write rate; the planner's three fixes and EXPLAIN; field
types from apack's shapes, held by the database for every writer; fuzzy terms by seeking; a field write reading what it
replaces once; values of 256 bytes or more stored compressed; substring and regex search over a trigram index.

## Designed, each waiting for its trigger

Each is a design in `roadmap/` with its measurements and phases (`docs/evidence/next-phase-10.md`), built when the
reading it names calls for it:

- **The smaller filter first, then value indexes as bitmaps** (`roadmap/plan-algebra.md`): when a plan over value
  indexes is measured slow in apack, whose 13 value indexes hold 851 entries today.
- **Incremental backup as the fields changed since a sequence** (`roadmap/change-log.md`): when apack's backup screen
  takes incremental backups; a log of writes is ruled out by G8's data, where it would be 57 times the values.
- **Maintained views** (`roadmap/incremental-views.md`): when a view's re-run is a measurable share of a user action.
- **Lazy rows and optimistic units** (`roadmap/javascript-boundary.md`): when a view decodes fields it doesn't read,
  and when a flow holds a unit across an `await`.
- **Per-type dictionaries** (`roadmap/value-dictionaries.md`): when a type's small text values are a tenth of the file.
- **Block-max scores** (`roadmap/text-index-at-scale.md`): when a common word's search is measured slow.

## Planned

- **Graph algorithms in the engine** (`archive/roadmap/graph-algorithms.md`): connected components, PageRank and personalized
  PageRank, and Leiden communities, as plan steps and answers over the set a plan has built, deterministic, bounded,
  tracked and explained. Its first callers are apack's related notes, prompt context ranking and a graph view. The
  traversal it builds on (`both`, `depth`, `pathTo`, `degrees`) is built; what graph query languages answer beyond it
  is deferred, each until a caller needs it (`roadmap/deferred/graph-traversal.md`).

## Later, each its own decision

Directions past these, each a design to argue for before it is built, are in `roadmap/` (`roadmap/README.md`).

- LMDB 1.0 through `heed3`, once released and supported by hannoy: page checksums and encryption.
- Scratch files with a configured placement and quota, and reclaimed after a hard exit (`elmdb-check-*` directories a
  SIGKILL leaves; `architecture/engine.md`, Recovery resources).
- `NOT_DURABLE` naming the tokens its commit touched, so a caller invalidates those rather than every watch.
- Populated replacement measured across broader profiles, and each recovery operation on its own.
