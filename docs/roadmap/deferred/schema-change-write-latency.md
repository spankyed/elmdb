# What a schema change costs the writes beside it

Status: deferred, 2026-11-19. Nothing calls for it yet. This records what a write waits for while a schema change is
made, what it would take to wait for less, and the reading that would make that worth doing.

## Where it stands

A schema change (`Database::set_schema`, `database.rs`) holds the schema's lock for writing from its check for an open
unit until it has told the index building what the change left (`docs/roadmap/single-builder.md`). Since #195 it no
longer lets go of the lock part way. Letting go was how it waited for the builder's thread to end, and that window is
where the eleven defects that design lists came through.

Every write takes the schema's lock for reading from before its transaction until it commits. So a write that lands
during a change waits for all of it:
- the change waits for LMDB's one writer, which an index build's chunk may hold for a few milliseconds;
- it then applies the schema in its own write transaction (`apply_schema`);
- and then it hands the building its work.

Measured on 2026-11-19, on an Apple M1 Pro with a release build: 60,000 rows, and twenty schema changes 60 ms apart,
each interrupting the build of a value index over them. A writer set one field in a loop beside them, and each build
was run three times, alternated (the program is in elmdb#198's description).

| | write p50 | write p99 | schema change p50 |
|---|---|---|---|
| before #195 (7f1d1e6) | 3.05–3.08 ms | 3.46–3.56 ms | 5.9–6.4 ms |
| after #198 | 2.93–3.01 ms | 7.04–7.16 ms | 5.3–5.5 ms |

The middle of the distribution doesn't move, and a schema change got faster. The slowest writes during schema changes
take about twice as long.

## What would make it worth doing

- A caller that changes its schema while it writes, often enough that the slowest writes matter, and says so. apack
  sets its schema at open and when a pack is installed, which is neither frequent nor beside a stream of writes.
- Or a reading of the slowest writes during a schema change, on a database apack holds, that is over a frame (16 ms).
  The reading above is less than half that.

## The design, when it is

The wait a write meets has two parts. Only the first can go without reopening the window.

1. **The change takes LMDB's writer before the schema's lock.** It holds the schema's lock while it waits for the
   writer, which an index chunk may hold, so writes queue behind both. Taking the writer first means the change
   waits for the chunk with the schema's lock free, and writes go on meanwhile. The change then takes the schema's
   lock with the writer held, and applies in that same transaction.
   - The order the locks are taken in changes, and every other taker must agree. A write takes the schema's lock and
     then the writer. Both orders in one process would deadlock, so the change would have to wait for the schema's
     lock only by trying it: holding the writer, it tries the lock for writing; where a write holds it for reading
     and waits for the writer, the change lets go of the writer and starts again.
   - A unit takes the schema's lock for reading as it begins and holds the writer while it is open. A change already
     refuses an open unit (`UNIT_OPEN`), so it waits for none.
   - `apply_schema`'s transaction is `Store::write_committed`'s, which takes the writer itself. It would take a
     transaction the change already holds, in the way `Unit` does.
   - **What it saves:** the chunk's few milliseconds, which is most of the p99 above. A write still waits for the
     change's own transaction.
2. **The change's own transaction** applies the schema and removes the data of the indexes it drops. Writes have to
   wait for it: a write that committed under the schema it replaces would be refused anyway (`SCHEMA_CHANGED`). It
   could be made smaller by deleting a dropped index's keys in chunks, as a build fills them in, after the change. That
   is a change to `format` and to `check()`: a dropped index's keys would stay until deleted. It is not worth its cost
   for a schema change's latency alone.

## How it would be checked

- The measurement above, run again, should show the p99 near the p50, with the schema change no slower.
- The lock-order change is the risky part. A Shuttle model of the three takers (a write, a unit's begin, a schema
  change) over a stand-in writer, which no other model holds, should hold that none of them waits forever and that no
  write commits under a schema that changed while it ran.
