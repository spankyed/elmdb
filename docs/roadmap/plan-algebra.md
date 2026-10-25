# One plan algebra

Status: idea.

## Where it starts

A plan is a start, steps and a terminal, run by pulling ids through closures (`../architecture/engine.md`, Reads). A
value index is read only for a `where` straight after a type start; any other `where` walks the rows; `orderBy` with
`limit` sorts the whole set; and a search is one step that collects its matches before the steps after it run. The
planner makes few choices, which is why apack warns about unindexed walks rather than relying on the planner.

## The idea

Every set a plan handles is a set of entity places (`seq`) — a roaring bitmap — and every step is an operation on such
sets: an index lookup yields one, a filter intersects, `OR` unions, a negation subtracts, a walk maps through edges, a
search yields one with scores, a vector query yields one with distances. Ranking, ordering and limits are the last
stage, over a set already narrowed. A planner then chooses between equivalent forms by what each index says it holds
(an index's cardinality, a term's document frequency), so `where(a).where(b)` intersects two indexes whatever their
order, and a filter inside a search is a bitmap the search runs within.

**EXPLAIN** answers the chosen plan: each stage, the index it read, the entities it visited and kept, and the time it
took — the reading `visits` gives today, for every stage rather than the whole.

## Open questions

- Value indexes as bitmaps per value: their write cost against today's one key per entry.
- Places across types: a set mixing types needs the type beside each place, or a bitmap per type.
- How a caller's function, which can't cross, splits an algebraic plan.
