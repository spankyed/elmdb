# One plan algebra

Status: designed, 2026-10-28. The first part, the planner's three fixes and EXPLAIN, is built (goal-elmdb-next's
Phase 4, `../evidence/next-phase-4.md`). The rest is designed here and waits for its trigger: at apack's scale no plan
intersects sets large enough for it to pay (`../evidence/next-phase-10.md`, Plan algebra).

## Where it stands

A plan is a start, steps and a terminal, run by pulling ids through closures (`../architecture/engine.md`, Reads). Since
Phase 4, a type start's first indexed `where` or `withRole` among its leading filters drives, the filters before it are
checked per match, an index lookup visits its matches in place order and stops at a `limit`, `orderBy` with `limit`
holds only the first `limit`, and EXPLAIN answers each stage's index, visits, kept ids and time. A search, a vector
query and a trigram search each answer a set the steps after them filter.

What the planner still does not do: choose which of two indexed filters drives (the first wins, whichever is smaller),
intersect two indexes rather than check one per match, or run a search inside a filter's set when the filter comes
first.

## The measurements

- **apack's value indexes are small.** G8's data under apack's schema holds 13 value indexes with about 850 entries in
  all, the largest `Thread.shortCode` (343); a message is found through its thread's `contains` edges, not an index.
  No apack plan intersects two large sets.
- **Where bitmaps would pay**, measured over 100,000 tasks with two value-indexed fields (`status` half `open`, `prio` a
  fifth `high`, 10,000 both; release build, 2026-10-28, a load of 11): `where(status).where(prio)` as the planner reads
  it today takes 58.7 ms, the 50,000-entry index driving and `prio` read per match; with the two filters swapped,
  26.2 ms; the `prio` lookup alone 12.9 ms. The same answer from two roaring bitmaps (16 KB each) decoded and
  intersected takes 5 µs, and turning its 10,000 places into ids 4.4 ms.
- **The store's layout**, measured by Phase 9 on the trigram index: one key per value and entity tripled G8's file
  where a bitmap per value and block of 2^16 places added an eighth (`../evidence/next-phase-9.md`). A value index
  stored per entry pays the same per-key cost.
- **Ranking**: a common word searched exactly over 50,000 messages takes 21 ms (apack's text bench, Phase 9's G4), every
  holder scored; block-max scores are the answer when that is too slow (`text-index-at-scale.md`).

So the first gain at scale is the order of the filters, which needs only each index's size; bitmaps are the second, and
both are worth building only when a plan over large sets is measured slow.

## The design

### 1. Cardinality, and the smaller filter first

Each value index keeps, per value, how many entities hold it: `d/xn type index value → count`, kept by the writes that
add and remove its entries, derived by `check()` and `rebuild()`. Among a type start's leading indexed filters the one
with the fewest entities drives; the others are checked per match. EXPLAIN names the chosen index and each candidate's
count.

### 2. Value indexes as bitmaps

A value index stores, per value and block of places, a roaring bitmap: `d/x type index value block → bitmap`, the
trigram index's layout (`d/g`), the block 2^16 places. A unique index's value holds one place; its check reads one key.
The ordered range reads (`orderBy` from an index, a range `where`) walk values in order and, within a value, places in
order, which a bitmap's iteration gives. Keys of values over 256 bytes, which today aren't indexed, stay unindexed.

### 3. Sets as the algebra

A step yields a set of places with its type — a bitmap per type — and the plan's filters become operations on sets:
an index lookup a bitmap, two filters an AND, `withRole` a bitmap of the role index, a negation (`lacks`, `unlinkedTo`)
a difference from the type's places, a walk the far ends' places, a search a set with scores, a trigram search its
candidates checked. Order, `limit` and `after` stay the last stage over a set already narrowed, by a heap for `orderBy`
with `limit`. A filter before a search becomes the `within` bitmap the search runs inside, which `search` already takes
(`text::search::search`'s `within`).

A caller's function `where` can't cross; it splits the plan where it stands, as today: the plan before it runs as sets,
its ids go to the function, the plan after it starts from what the function kept.

### API

No new step. The plan's steps keep their meaning, and their answers stay in the order they answer today: a type
start's in place order, a set's in the order it came. EXPLAIN adds each stage's input and output cardinality and, for a
set operation, its kind (`and`, `or`, `minus`).

## Risks

- **A bitmap's rewrite on every write that adds a common value**, which the block bounds: Phase 9 measured a created
  message's trigram filing at 0.3 ms, rewriting a block's bitmap per trigram; a value index rewrites one bitmap per
  indexed field, not hundreds.
- **Order.** A bitmap is in place order; an answer in another order (an `orderBy` without `limit`) still sorts.
- **Places across types**: one bitmap per type keeps a mixed set's type with each place, at the cost of a per-type loop.
- **Equivalence.** Every reordering must answer what the plain reading answers; the model harness's plans and the
  plans spec are the proof, extended with two-filter cases.

## Phases

1. **Cardinality and the smaller filter first.** Done when: `d/xn` is kept by every write and covered by `check()` and
   `rebuild()` (the model harness's derived reads include it); the plans spec shows `where(a).where(b)` visiting the
   smaller index's matches whichever order they're written in, its answer equal to the plain reading's; EXPLAIN shows
   the counts; the 100,000-task reading above is taken again and is within 10% of its smaller-first figure either way.
2. **Value indexes as bitmaps.** Triggered by a plan over value indexes measured slow in apack, or an index whose
   entries are a measurable share of the file. Done when: the layout is format 1 edited in place (G3 of goal-elmdb-next,
   while it holds); a lookup, a range, a unique check and `orderBy` from an index answer what they answer today
   (model harness, plans spec); G8's file size and apack's bench are recorded before and after.
3. **Sets as the algebra.** Triggered by phase 2 and a plan of two or more large filters measured slow. Done when: two
   indexed filters intersect as bitmaps (the reading above within 2× of its 4.4 ms bitmap figure), a filter before a
   search is its `within`, a function `where` splits the plan as today, and every plan spec and the model harness pass
   unchanged.
