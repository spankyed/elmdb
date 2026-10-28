# Incremental views

Status: designed, 2026-10-28. Not to build until a view's re-run is measured to cost: every apack view measured runs in
well under a millisecond a commit, and a chat turn's watch runs are 0.2% of it (`../evidence/next-phase-10.md`,
Incremental views).

## Where it starts

Tracked reads (`../architecture/engine.md`, Change sets and watch) tell a watch which commits wrote into what it read,
and apack's watch scheduler (`packages/apack-ears/src/watch.ts`, Phase 3) runs it again no sooner than four times its
last run's time and at least a frame after the last, so an expensive view takes at most a fifth of a core. That is
exact about when to recompute and bounded in what it costs, but a view still recomputes its whole answer for one row
changed in it.

## The measurements

- **A chat turn**: a 400-word reply's 229 watch runs take 49.5 ms of its 24 s; the `threads · message` view runs once
  per streamed chunk, 0.21 ms a run (Phase 3, `next-phase-3.md`).
- **The largest thread on G8's data**, 1,647 messages: its messages read whole as rows in 8.9 ms, the first 50 in
  0.68 ms (release binding, 2026-10-28). apack's thread view reads a page.
- **The stress reading**: an ordered view of 6,700 rows runs in 15.5 ms; the scheduler holds it to 19.5% of a core
  under a writer committing 11,899 times a second (Phase 3).

A maintained view would save at most the 49.5 ms a turn and the page's 0.68 ms a commit. Nothing apack runs today is
slow enough to need it.

## The design

### Which plans

A view maintained from deltas is a plan — not a function — whose steps are all maintainable:

| Step | How a commit's change set updates the answer |
|---|---|
| a type start | an entity created joins, one destroyed or trashed leaves (a live scope) |
| `where`, `has`, `lacks`, `withRole`, `ofType` | an entity whose changed field now passes joins, one that no longer passes leaves |
| `out`, `in` (one hop) from fixed ids | a relation added or removed from the start joins or leaves its far end |
| `orderBy` | an entity whose ordering field changed moves |
| `orderBy` then `limit` | a top-k kept with its next candidate: an entry leaving pulls the next by reading the plan's ordered source from the k-th value on |
| `count` | adjusted by the joins and leaves |

Deep walks, `distinct` over a field, searches and `after` are not maintained: a view holding one is re-run under
tracked reads, as today. A function, which apack's views mostly are (several plans with JavaScript between them), is
never maintained.

### What a listener receives

`db.watch(reads, listener, { maintain: true })` delivers, after the first full answer, a delta per commit that changes
it: `{ sequence, joined: [{ id, at, row? }], left: [id], moved: [{ id, at }] }`, positions in the answer after the
change. A plan that isn't maintainable is refused with the step that isn't (`NOT_MAINTAINABLE`), so a caller knows
which contract it has. The maintained answer lives in the watch, in memory, as the ids in order (and their rows when
the terminal answers rows); an unwatched view holds nothing.

### In apack

`fromView` and `useView` stay functions over tracked reads. A view that is a single plan may declare itself maintained
(`defineViews`' `maintained: true`), and the view host then forwards deltas to its windows, which patch their list
rather than replace it.

## Risks

- **Equivalence.** A maintained answer must equal a fresh read after every commit; the model harness's watch property
  (every watch's last delivered value equal to a fresh read) extends to maintained watches, with deltas applied.
- **Memory.** A maintained answer is held for as long as it is watched: a view of every message holds 19,681 ids.
- **A row's change that a filter doesn't read** still changes a `rows` answer; a maintained `rows` view re-reads each
  changed row it holds, which is what the delta's `row` carries.

## Phases

1. **Filters and counts.** Triggered by a view measured to cost a share of a user action (a reading like Phase 3's
   with one view over 10% of the action's time in elmdb). Done when: a type start with filters and `count` is
   maintained; the model harness's watch property holds for maintained watches; a commit touching one entity costs
   the delta's work, shown by a reading of the view over 20,000 rows before and after.
2. **Order and top-k.** Done when: `orderBy` and `orderBy` then `limit` are maintained, an entry leaving the top-k pulls
   the next by an ordered read, and the property above holds.
3. **One-hop walks and apack's maintained views.** Done when: `out`/`in` from fixed ids is maintained, apack's view host
   forwards deltas for a view declared maintained, and a window's list patches rather than replaces.
