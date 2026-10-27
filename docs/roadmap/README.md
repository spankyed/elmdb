# Roadmap: where elmdb could go

Directions past the planned work (`../roadmap.md`), each a design to argue for before it is built. Most are ideas; one is
decided, and the boundary's first part is built. Each file says which.

What they share: LMDB already is an ordered B-tree with cheap snapshots and one writer. The best answer to most of these
uses those directly, rather than rebuilding what Lucene or a server database needs because it lacks them.

| Direction | What it would change |
|---|---|
| [Incremental views](incremental-views.md) | A view's answer updated from each commit's change set, not computed again |
| [The change log as a product](change-log.md) | Incremental backups, sync across devices, and undo, from the commit stream |
| [The JavaScript boundary](javascript-boundary.md) | What a call costs and how apack groups them; reads without a decode, and units that can span an `await` |
| [Field types from apack's shapes](typed-schema.md) | Decided: codegen sends each field's type, so elmdb checks writes the compiler can't see |
| [One plan algebra](plan-algebra.md) | First three planner fixes and EXPLAIN; later, filters as operations on bitmaps |
| [The text index at scale](text-index-at-scale.md) | Posting lists in blocks on LMDB's B-tree, fuzzy terms by seeking, each when a reading calls for it |

## Development improvements

- [Libraries for safer implementation and easier development](development-libraries.md): staged platform bindings,
  benchmarks, concurrency models and diagnostics; planned, with adoption gates.
