# Incremental views

Status: idea.

## Where it starts

Tracked reads (`../architecture/engine.md`, Change sets and watch) tell a view which commits wrote into what it read,
and the view runs again. That is exact about *when* to recompute and does nothing about *what* recomputing costs: a
thread's message list, a library's tree or a flow's run nodes are read whole again for one row added to them.

## The idea

A view declared as a plan is maintained, not re-run. The engine keeps its answer and, for each commit that touches it,
derives the answer's change from the commit's change set: a row added where a filter now holds, removed where it no
longer does, moved where an ordering field changed, a count adjusted. The listener receives the delta, so a view
patches its list rather than replacing it, and the cost of keeping it current is the size of the change, not of the
answer.

- **Which plans:** filters, value-index lookups, one-hop walks, `orderBy` with `limit` (a top-k kept as a heap, its
  next candidates found when a member leaves) and counts are maintainable from a delta. Deep walks, `distinct` over a
  field and a search's ranking are not, and fall back to re-running under tracked reads, which stays the contract for
  views that are functions.
- **What apack gains:** its systems broadcast state on every cause (`SEND_STATE`); a surface subscribing to a
  maintained view needs neither the broadcast nor the reread, and two windows on one view share one maintenance.

## Where it applies

Incremental maintenance is possible over plans only. apack's views are functions — several plans with JavaScript
between them — which no engine can maintain from a delta; for them tracked reads stay the contract, re-running the
function when a commit names its token. And a view is worth maintaining only when re-running it is measured to cost:
most of apack's are cheap to run again. So this follows tracked reads, view by view, where a reading asks for it.


- How much of a plan is maintainable in practice: apack's views mix plans with JavaScript between them.
- Where the maintained answers live and what they cost to hold for a view nobody is looking at.
- Delta delivery's shape across the binding, and ordering against a caller's own writes (the commit sequence).
