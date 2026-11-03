# Roadmap

What is built, what is designed and waiting, and what comes later.

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
- **Block-max scores** (`roadmap/text-index-at-scale.md`): when a common word's search is measured slow.
- **What graph query languages answer beyond a plan** (`roadmap/deferred/graph-traversal.md`): each when a caller needs it.

## Later, each its own decision

- LMDB 1.0 through `heed3`, once released and supported by hannoy: page checksums and encryption.
- Scratch files with a configured placement and quota, reclaimed after a hard exit (`elmdb-check-*` directories a SIGKILL
  leaves; `architecture/engine.md`, Recovery resources).
- `NOT_DURABLE` naming the tokens its commit touched, so a caller invalidates those rather than every watch.
- Populated replacement measured across broader profiles, and each recovery operation on its own.
