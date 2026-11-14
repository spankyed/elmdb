# Roadmap: where elmdb could go

Directions past the planned work (`../roadmap.md`), each with the measurements it rests on and the reading that would
trigger it (`../evidence/next-phase-10.md`); each file says how far it is built. Directions that were built are in
`../archive/roadmap/`: field types from apack's shapes (`typed-schema.md`), graph algorithms (`graph-algorithms.md`), their weights (`rank-weights.md`) and the rank index (`rank-index.md`),
read allocations, copies and codecs (`read-allocation-codec.md`) and query work efficiency
(`query-work-efficiency.md`), and the maintenance verification (`maintenance-verification.md`).
What graph query languages answer that a plan doesn't, and the order to add it, is in
[`deferred/graph-traversal.md`](deferred/graph-traversal.md).

What they share: LMDB already is an ordered B-tree with cheap snapshots and one writer. The best answer to most of these
uses those directly, rather than rebuilding what Lucene or a server database needs because it lacks them.

| Direction | What it would change |
|---|---|
| [Incremental views](incremental-views.md) | Designed: a plan's answer maintained from each commit's change set, not computed again |
| [The change log as a product](change-log.md) | Designed: incremental backup from the fields changed since a sequence; sync and undo named, not designed |
| [The JavaScript boundary](javascript-boundary.md) | Designed: lazy rows that decode a field when read, and optimistic units that can span an `await` |
| [Per-type dictionaries](value-dictionaries.md) | Designed, not to build on today's data: small values compressed with a dictionary trained per type |
| [A string's order from its index](string-order.md) | Designed, not built: every declared value index keeps a key for each distinct string it holds, under the collation's sort key, so an order, a page and a string range read keys and entries where they read every entity, and a write that repeats a string files nothing more; a marker of the collator's stamp and a build keep a stale sort key from being read |
| [The text index at scale](text-index-at-scale.md) | Built: fuzzy terms by seeking, a term's places in blocks with their counts, lengths held by place, and ranking read from the two. Designed from readings at a million notes: a commit notes the place it changed and the builder files it |

## Where the code lives

- [EARS in elmdb's repository](ears.md): planned, not built. apack's `@apackdev/ears`, the fluent TypeScript API over
  a database, moves here as `@elmdb/ears`, with what apack keeps, what it costs and the phases.

## Development improvements

- [Libraries for safer implementation and easier development](development-libraries.md): staged platform bindings,
  benchmarks, concurrency models and diagnostics, each stage built or declined with its record.
- [Tooling for tests that can fail, faults by name, and measured bounds](verification-tooling.md): mutation testing,
  failpoints, heap assertions, per-process tests, model-based properties, profiling; all built.
- [Lessons from lmdb-js](lmdb-js-research.md): a planned survey of what lmdb-js does that elmdb might learn from.
