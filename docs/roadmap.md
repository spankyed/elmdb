# Roadmap

The order of work. Each step ships something apack uses. Status: steps 1–6 built (elmdb's goal, Phases 1 and 9–14;
`docs/evidence/`); what comes next is below them.

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

## Next, in order

From a review of the engine on 2026-10-25 (the reasons in `roadmap/`):

1. **Count the crossings of a chat turn and a flow run** in apack, as a cold boot's were counted
   (`roadmap/javascript-boundary.md`), and turn the loops of one-entity reads they show into set reads.
2. **The planner's three small fixes and EXPLAIN** (`roadmap/plan-algebra.md`, First, without bitmaps).
3. **Posting lists in blocks**, if a message write's time is measured to grow with the corpus
   (`roadmap/text-index-at-scale.md`).
4. **Field types from apack's shapes** (`roadmap/typed-schema.md`; planned in apack's
   `docs/archive/plans/field-types-from-shapes.md`).

Bitmaps, reads without a decode, units that span an `await` and the change log follow, each when a measurement or a
feature calls for it.

## Later, each its own decision

Directions past these, each a design to argue for before it is built, are in `roadmap/` (`roadmap/README.md`).

- Value compression with per-type dictionaries.
- LMDB 1.0 through `heed3`, once released and supported by hannoy: page checksums and encryption.
- Substring and regex search.
- Fewer reads per field write. The global sequence key every insert rewrites costs little: LMDB has one writer, and
  the key's page is dirty in the commit anyway.
