# Phase 4 of goal-elmdb-next: the planner's three fixes, and EXPLAIN

elmdb `f8dc82e` (the fixes and EXPLAIN), apack `d4db390d0f` (`explain()` and the Database console), 2026-10-27. Machine:
Apple M1 Pro, 10 cores, 32 GiB, macOS 26.3.1, Node 23.11.1, under other load (load averages from 8 to 21 over the runs), so every
comparison alternates its builds.

## What changed

`docs/architecture/engine.md`, Reads: plans, states the three choices and EXPLAIN's answer:

- **A type start's filters are read from an index wherever the indexed `where` stands among them.** The first `where`
  or `withRole` among a type start's leading filters (`where`, `withRole`, `has`, `lacks`, `ofType`, `within`) that
  every start type has a built index for is read instead of the walk, and the filters before it are checked per match.
- **An index lookup visits its matches as it reads them**: one value's entries are keyed by place, so the range is in
  creation order already, and a `limit`, `first` or `exists` reads no further. The lookup used to collect the whole
  range and sort it, and counted a visit only for each id it handed on, which is why `visits` never showed its cost;
  a visit is now an index entry read.
- **`orderBy` then `limit` holds the first `limit`** in a sorted buffer, ties as they came, instead of sorting the set.
- **EXPLAIN**: a query read with `explain` answers `{ kind: "explained", answer, visited, ms, stages }`, a stage per
  start, step and terminal with its index, what it visited and kept, and its own time. apack's `explain()` terminal
  answers it as `QueryExplanation`, and the Database console shows one as a table of stages (`ExplainDisplay.vue`);
  its examples explain `query(EARS.Entity.Thread).where('pinned', true).limit(20)`.

## Specs, each shown failing

`crates/elmdb/src/engine/tests/plans.rs`, over 13,000 open tasks and 2,000 done:

| Case | Shows |
|---|---|
| an indexed `where` with a `limit` visits its limit | `where('status', 'open').limit(50)` visits 50 of the 13,000 matches; the 50 are the first 50 of the whole answer |
| a `where` after other filters reads its index | after `has`, `lacks`, an unindexed `where` and `ofType`, `where('status', 'done')` visits the index's 2,000 matches, not the 15,000 tasks, and answers what the index-first order answers |
| `orderBy` then `limit` | ascending and descending, limits 1, 10, 2,500 and past the set, and a string field with missing values: the first `n` of the full sort |
| EXPLAIN on a plan of each kind | an index read under a limit answering rows; a first `where` with no index, walking; an unindexed `where` after a filter; an index driving the filter before it; every entity ordered and limited; ids given, one missing. Each stage's index, visited and kept, the answer equal to the plain read's, the stages' times within the whole |

Each was shown failing on a copy-on-write clone of the checkout (`cp -cR`, never the working tree), one mutation at a
time:

| Mutation | Fails |
|---|---|
| The lookup collects its range before visiting, counting each entry it reads | the limit case; EXPLAIN |
| No driving index after the first step | the after-other-filters case; EXPLAIN |
| A later equal value displaces an earlier one in the `limit` buffer | the `orderBy` case |
| The filters before a driving index are skipped | the after-other-filters case |
| EXPLAIN counts no kept ids | EXPLAIN |

The whole engine suite (277 cases), the binding's 37 Node tests, clippy and the generated-protocol check pass; the
model harness and the tracked-read specs pass unchanged, so no answer and no watch's read set changed. apack's
`explain()`: `packages/apack-ears/tests/engine/queries.spec.ts` (its stages on an index-driven query, a walk, and its
refusal for a function `where`) and `packages/default-setup/tests/features/database/fe/components/simple-table/
composables/explanation.spec.ts` (the console tells a real explanation from every other result).

## Readings

**The page view.** G14's stress reading (`npm run bench:watch -w @apackdev/ears`) has the indexed `where` with a
`limit` of 50 among its views: 2.098 ms a run before (Phase 3's reading, elmdb `c345182`), 0.137 ms after, the rest of
the reading unchanged (13,040 commits a second, the ordered view 19.4% of the wall).

**G4: apack's bench**, the engine over elmdb `c345182` and `f8dc82e` alternately, twice, through the `vendor/elmdb`
link; each figure the lower of its two medians, in ms:

| Case | Before | After | |
|---|---:|---:|---:|
| bulk load 50k entities, 200k attributes, 100k relations | 1277.0 | 1301.5 | +1.9% |
| query by type with where and pickAll | 37.41 | 35.54 | −5.0% |
| 1,000 where lookups by a unique value | 7.07 | 6.84 | −3.2% |
| 100 withRole lookups | 32.34 | 25.13 | −22.3% |
| query by type with 3 chained steps | 26.14 | 24.70 | −5.5% |
| 1,000 query(type).first() over 40k | 4.16 | 4.39 | +5.4% |
| 1,000 getRow | 5.01 | 4.73 | −5.5% |
| 1,000 creates | 110.3 | 119.3 | +8.2% |
| transact: 1,000 creates as one unit | 25.58 | 26.45 | +3.4% |
| 1,000 creates, then 100 watches on a page of the type run | 111.0 | 116.1 | +4.6% |

Every other case is within ±3.1%. None is past the 10% tolerance. The creates' +8.2% is in a path this phase didn't
change (writes, and a type start's `first`, read no index), on a machine whose load average rose from 8 to 21 during the
runs; `withRole`'s −22% is the role index's lookup streaming.

**The single-row read through the binding**, µs per `readSync` of one row on 1,000 rows, the median of five batches of
50,000, the two bindings alternated five rounds: before 3.247, 3.179, 3.202, 3.216, 3.303; after 3.299, 3.272, 3.240,
3.207, 3.259. Medians 3.216 and 3.259, +1.3%; a row read takes no plan, so this is the binding's noise.
