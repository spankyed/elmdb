# Roadmap

What is built, the road to 1.0, what is designed and waiting, and what comes later.

## To 1.0

Ten changes, each done when its gate holds, set on 2026-11-09 from a review of `ed20979`. They come before the designs
waiting on a trigger below, which add to them and reorder nothing.

| # | Change | Done when | State |
|---|---|---|---|
| 1 | The defects the review reproduced | each is fixed with a test that failed first; the model harness covers change sets, `changedSince` and the admin calls | fixed, `79f646f`; the model harness's part is open |
| 2 | Panics caught at the binding | a failpoint's panic in each export is a JavaScript error with a code of its own, and the process lives | open |
| 3 | A mode that survives power loss | an open option commits with LMDB's own sync; a crash-replay harness passes it and fails the grouped mode once; the README states the trade | open |
| 4 | `write`, `check`, `rebuild` and `backup` off the calling thread | the event loop is never held 10 ms during a rebuild of 200,000 notes | open |
| 5 | Ranges, ordered reads and a cursor that seeks | `orderBy` and `limit 10` on an indexed field visit about ten entities, and a page costs the same wherever its cursor is | open |
| 6 | Vector builds off the commit path | one embedding a commit costs within twice a batch's; a commit of one takes under 2 ms at 100,000 vectors | open |
| 7 | Typed answers and a thin client | answers are generated as the rest of the protocol is, and a test decodes each into its type | open |
| 8 | Format 1 frozen | databases and dumps of every tag open and pass `check()` in CI; the move to LMDB 1.0 is decided first | open |
| 9 | Release and supply chain | prebuilt binaries for the seven targets; `cargo-deny` and an audit in CI; the `$schema` URL resolves | open |
| 10 | A benchmark against SQLite, committed | bytes per note, search and rebuild at a million notes are read for each release | the benchmark is built (`architecture/testing.md`, Against SQLite) and read once (`evidence/million-notes.md`) |

| Milestone | Holds | Reached when |
|---|---|---|
| 0.1, safe to embed | 1 to 4 | no input known aborts or hangs the host, and power loss has a tested answer |
| 0.2, the queries a stranger expects | 5, compound indexes read, the smaller filter first, least, greatest and sum | the README's uses need no filtering in JavaScript |
| 0.3, contracts frozen | 7, 8 | the format, the protocol and the error codes each have a compatibility test |
| 0.4, installable | 9, a quickstart, a cookbook of plans, a decision on Chinese, Japanese and Korean text | `npm install elmdb` works on the seven targets and a second program is built on it |
| 0.5, a known envelope | 6, 10 | readings at a million notes are published, each with its target |
| 1.0 | | two releases break neither the format nor the protocol, and an upgrade from 0.3 is tested |

The benchmark comes first, out of its milestone, since it is what the gates below are read from. The indexes' layout
at a million notes is designed in `roadmap/text-index-at-scale.md` and changes derived keys, so it is built before the
format is frozen; change 6 is built with it, since a vector index's journal outliving its commit is the same design.

## Built

| What | Record |
|---|---|
| The protocol's cost, measured against lmdb-js | `evidence/phase-1.md` |
| Parity with apack's engine, and apack off lmdb-js | `evidence/phase-9.md`, `phase-10.md` |
| A second database for apack's run history | `archive/goals/goal-elmdb.md`, Phase 11 |
| Ranked text search, fuzzy labels | `evidence/phase-12.md` |
| Vector and hybrid search | `evidence/phase-13.md` |
| Change sets, tracked reads and watch | `evidence/phase-14.md` |
| Call counts, bounded watches, EXPLAIN, field types, compressed values, trigram search | `evidence/next-phase-3.md` to `next-phase-9.md` |
| Graph algorithms, and their weights | [graph-algorithms](archive/roadmap/graph-algorithms.md), [rank-weights](archive/roadmap/rank-weights.md) |
| Rank indexes | [rank-index](archive/roadmap/rank-index.md), `evidence/rank-index.md` |

Other built directions are in `archive/roadmap/`.

## Designed, each waiting for its trigger

Each is a design in `roadmap/` with its measurements (`evidence/next-phase-10.md`), built when the reading it names calls
for it:

- **The smaller filter first, then value indexes as bitmaps** (`roadmap/plan-algebra.md`): when a plan over value
  indexes is measured slow in apack.
- **Incremental backup as the fields changed since a sequence** (`roadmap/change-log.md`): when apack's backup screen
  takes incremental backups.
- **Maintained views** (`roadmap/incremental-views.md`): when a view's re-run is a measurable share of a user action.
- **Lazy rows and optimistic units** (`roadmap/javascript-boundary.md`): when a view decodes fields it doesn't read,
  or a flow holds a unit across an `await`.
- **Per-type dictionaries** (`roadmap/value-dictionaries.md`): when a type's small text values are a tenth of the file.
- **Block-max scores** (`roadmap/text-index-at-scale.md`): when a common word's search is measured slow once ranking
  reads its counts and lengths from the index, which that design builds first.
- **What graph query languages answer beyond a plan** (`roadmap/deferred/graph-traversal.md`): each when a caller needs it.

## Later, each its own decision

- LMDB 1.0 through `heed3`, once released and supported by hannoy: page checksums and encryption.
- Scratch files with a configured placement and quota, reclaimed after a hard exit (`elmdb-check-*` directories a SIGKILL
  leaves; `architecture/engine.md`, Recovery resources).
- `NOT_DURABLE` naming the tokens its commit touched, so a caller invalidates those rather than every watch.
- Populated replacement measured across broader profiles, and each recovery operation on its own.
