# One plan algebra

Status: idea.

## Where it starts

A plan is a start, steps and a terminal, run by pulling ids through closures (`../architecture/engine.md`, Reads). A
value index is read only for a `where` straight after a type start; any other `where` walks the rows; `orderBy` with
`limit` sorts the whole set; and a search is one step that collects its matches before the steps after it run. The
planner makes few choices, which is why apack warns about unindexed walks rather than relying on the planner.

## First, without bitmaps

At apack's scale, about 22,000 entities, most of the win is three small changes and a reading, built before anything
below:

- **Any `where` in a chain uses its index**, not only one straight after a type start.
- **`orderBy` with `limit` keeps a heap of the first `limit`** rather than sorting the whole set.
- **An index lookup visits its matches as it finds them**, rather than collecting and sorting them first: an indexed
  `where` with a `limit` of 50 read all 13,000 of its matches, 32 ms, in apack on 2026-10-25.
- **EXPLAIN**, below: cheap, and it would have shown that.

## Later: one algebra over bitmaps

Bitmaps are the industry's tool for intersecting filters, as Lucene and ClickHouse use them, but they give no order:
`orderBy` with `limit` still needs a heap or an index kept in value order. They are worth it when several filters
intersect over large sets, which a reading will show.


Every set a plan handles is a set of entity places (`seq`) — a roaring bitmap — and every step is an operation on such
sets: an index lookup yields one, a filter intersects, `OR` unions, a negation subtracts, a walk maps through edges, a
search yields one with scores, a vector query yields one with distances. Ranking, ordering and limits are the last
stage, over a set already narrowed. A planner then chooses between equivalent forms by what each index says it holds
(an index's cardinality, a term's document frequency), so `where(a).where(b)` intersects two indexes whatever their
order, and a filter inside a search is a bitmap the search runs within.

**EXPLAIN** answers the chosen plan: each stage, the index it read, the entities it visited and kept, and the time it
took — the reading `visits` gives today, for every stage rather than the whole.

## Value indexes as bitmaps

The algebra needs every index to answer a set, and a value index answers a range of keys today: one key per value and
entity (`d/x type index value seq`), visited to collect the places. Stored as a bitmap per value instead — the places of
the entities holding it, as a text index stores a term's — a lookup is one read and one decode, a `where` anywhere in a
chain is an intersection, and a unique index is a bitmap that holds at most one place. Its cost is the text index's
write cost (`text-index-at-scale.md`): a common value's bitmap is rewritten by every write that adds or removes it,
which the same segmenting answers.

## Open questions

- A bitmap per value against one key per entry: measured on apack's writes before the format changes.
- Places across types: a set mixing types needs the type beside each place, or a bitmap per type.
- How a caller's function, which can't cross, splits an algebraic plan.
