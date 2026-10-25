# Roadmap: where elmdb could go

Directions past the planned work (`../roadmap.md`), each a design to argue for before it is built. Status: ideas; none
is planned.

| Direction | What it would change |
|---|---|
| [Incremental views](incremental-views.md) | A view's answer updated from each commit's change set, not computed again |
| [The change log as a product](change-log.md) | Incremental backups, sync across devices, and undo, from the commit stream |
| [The JavaScript boundary](javascript-boundary.md) | Reads without a decode, and units that can span an `await` |
| [The schema as the source of types](typed-schema.md) | One declaration, the TypeScript shapes generated from it |
| [One plan algebra](plan-algebra.md) | Search, vectors, walks and filters as operations on one kind of set, with an EXPLAIN |
