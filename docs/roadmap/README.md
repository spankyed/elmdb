# Roadmap: where elmdb could go

Directions past the planned work (`../roadmap.md`), each with the measurements it rests on and the reading that would
trigger it (`../evidence/next-phase-10.md` for the first four below, `../evidence/million-notes.md` for the road's own
two); each file says how far it is built. Directions that were built are in
`../archive/roadmap/`: field types from apack's shapes (`typed-schema.md`), graph algorithms (`graph-algorithms.md`), their weights (`rank-weights.md`) and the rank index (`rank-index.md`),
read allocations, copies and codecs (`read-allocation-codec.md`) and query work efficiency
(`query-work-efficiency.md`), one plan algebra (`plan-algebra.md`), a string's order read from its index
(`string-order.md`), the tooling for tests that can fail, faults by name and measured bounds
(`verification-tooling.md`), the maintenance verification (`maintenance-verification.md`), and EARS' move into this
repository as `@elmdb/ears` (`ears.md`).
What graph query languages answer that a plan doesn't, and the order to add it, is in
[`deferred/graph-traversal.md`](deferred/graph-traversal.md). The rest of the supply chain is in
[`deferred/supply-chain.md`](deferred/supply-chain.md): `cargo-deny` and an advisory audit, which are built, and a
schema language URL that resolves, which waits on the domain's owner. What a schema change costs the writes beside it, and the
design that would cut the slowest of them, is in
[`deferred/schema-change-write-latency.md`](deferred/schema-change-write-latency.md).

What they share: LMDB already is an ordered B-tree with cheap snapshots and one writer. The best answer to most of these
uses those directly, rather than rebuilding what Lucene or a server database needs because it lacks them.

| Direction | What it would change |
|---|---|
| [Incremental views](incremental-views.md) | Designed: a plan's answer maintained from each commit's change set, not computed again |
| [The change log as a product](change-log.md) | Designed: incremental backup from the fields changed since a sequence; sync and undo named, not designed. Built since, for entities: what changed since a position (`changedSince`) |
| [The JavaScript boundary](javascript-boundary.md) | Designed: lazy rows that decode a field when read, and optimistic units that can span an `await` |
| [Per-type dictionaries](value-dictionaries.md) | Designed, not to build on today's data: small values compressed with a dictionary trained per type |
| [A power loss that tears no file](synced-commit.md) | Built, phases 1 to 5, but for phase 5's steps by hand on each platform: the grouped mode keeps the last synced commit, its pages by a read transaction LMDB gives no writer the pages of and its meta page by a record beside the data, so a database is opened at that commit after a power loss, or a sync that failed, where its file could be no commit's |
| [Freezing format 1](format-freeze.md) | Designed: format 1 stays on LMDB 0.9, edited in place with no bump; the lock holds primary and meta keys and values, the derived keys of no index, and the dump; each index records its layout, and one of another is thrown away and built again |
| [The text index at scale](text-index-at-scale.md) | Built: fuzzy terms by seeking, a term's places in blocks with their counts, lengths held by place, and ranking read from the two. Designed from readings at a million notes: a commit notes the place it changed and the builder files it |
| [A watch somebody is waiting on](waited-watches.md) | Designed: a caller that waits for watches by name lifts the frame's floor from their gap, and pays what their reads cost |

## How the engine is put together

- [One builder for a database's life](single-builder.md): built. The index builder was stopped and started at
  every schema change, which eleven defects came from; a database now has one building, told of each schema change
  and each close, with its operations modelled.

## Development improvements

- [Libraries for safer implementation and easier development](development-libraries.md): staged platform bindings,
  benchmarks, concurrency models and diagnostics, each stage built or declined with its record.
- [Lessons from lmdb-js](lmdb-js-research.md): a research plan of six workstreams; its status says which other work
  has since closed, and which are not started.
