# Roadmap: where elmdb could go

Directions past the planned work (`../roadmap.md`). Five are designed, each with the measurements it rests on and the
reading that would trigger it (`../evidence/next-phase-10.md`); field types are decided and built. Each file says which.

What they share: LMDB already is an ordered B-tree with cheap snapshots and one writer. The best answer to most of these
uses those directly, rather than rebuilding what Lucene or a server database needs because it lacks them.

| Direction | What it would change |
|---|---|
| [Incremental views](incremental-views.md) | Designed: a plan's answer maintained from each commit's change set, not computed again |
| [The change log as a product](change-log.md) | Designed: incremental backup from the fields changed since a sequence; sync and undo named, not designed |
| [The JavaScript boundary](javascript-boundary.md) | Designed: lazy rows that decode a field when read, and optimistic units that can span an `await` |
| [Field types from apack's shapes](typed-schema.md) | Built: codegen sends each field's type, so elmdb checks writes the compiler can't see |
| [One plan algebra](plan-algebra.md) | The planner's three fixes and EXPLAIN built; designed: the smaller filter first, then value indexes as bitmaps and filters as operations on them |
| [Per-type dictionaries](value-dictionaries.md) | Designed, not to build on today's data: small values compressed with a dictionary trained per type |
| [The text index at scale](text-index-at-scale.md) | Posting lists in blocks on LMDB's B-tree, fuzzy terms by seeking, each when a reading calls for it |

## Development improvements

- [Libraries for safer implementation and easier development](development-libraries.md): staged platform bindings,
  benchmarks, concurrency models and diagnostics; initial benchmarks implemented, other stages planned.
- [Tooling for tests that can fail, faults by name, and measured bounds](verification-tooling.md): mutation testing,
  failpoints, heap assertions, per-process tests, model-based properties, profiling; initial heap gates implemented,
  other stages planned.

[Maintenance verification after remediation](maintenance-verification.md) reconciles completed memory paths,
current benchmark/heap gates and outstanding operational work.
