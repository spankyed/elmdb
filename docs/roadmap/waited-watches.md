# A watch somebody is waiting on

Status: designed, 2026-11-22. Not built. A caller that waits for the watches pays for the gap between a watch's
runs, and should pay for the watch's read: this is what that costs today, and the change that ends it.

## Where it stands

A watch runs again after a commit that wrote into what it read, and no sooner than its gap after its last run:
`delayFactor` times what that run cost, and at least `minimumGapMs` (`WATCH_SCHEDULE`, `packages/ears/src/watch.ts`:
four times, and 16 ms). The gap is what keeps a watch over data that churns from running on every commit; the stress
reading that set it is `docs/evidence/next-phase-3.md`.

`admin.watchesSettled()` resolves once no watch is waiting to run. A caller that waits on it after a commit
therefore waits out whatever is left of the gap of every watch that commit marked.

apack waits on it. Its backend holds what a step sends a window until the views that window follows have been sent
what the step kept, so that a window is never told of something its views do not show (spankyed/apack#609), and
what it waits on to know that is `watchesSettled()`.

Measured through apack's kernel on 2026-11-22, on an Apple M1 Pro under a load of 7.6, a temporary database with
syncing off, the median of seven runs (apack's `docs/plans/apack-spec/prototype/rows-cost.mts`). A window with one
watch open, a count of a type's entities, asks its backend to write an entity:

| The ask | Answered when the entity is kept | Answered when the watches have settled |
|---|---|---|
| straight after the last was answered | 0.12 ms | 17.3 ms |
| 20 ms after the last was answered | 1.2 ms | 1.6 ms |
| one that writes nothing | 0.05 ms | 0.05 ms |

So the wait itself is a third of a millisecond, and nothing where nothing was written. What costs is a write that
follows another into the same watch within its gap: it waits a frame. One thing a person does is never that close to
the last. A run of asks, each made when the one before is answered and each writing, is: a frame a link. Asks made
together settle together, after one frame for all of them.

## Why

The caller's own fix is to make the run one ask, which writes everything in one unit and is whole where the run is
not. It is the better design where it fits. But it is a fix only for whoever knows the gap is there: a run of asks
is the natural thing to write, nothing says it costs a frame a link, and learning why takes reading this
scheduler. What a waiter should pay is what its watch costs to read, whether or not it knows a schedule exists. The
gap stays for what it is for, a watch nobody is waiting on.

## The design

The gap has two parts, and a waiter lifts one of them.

1. **A watch that is waited on is due after its factor, not after the floor.** `dueAt` for a waited watch is
   `ranAt + delayFactor × costMs`, without `minimumGapMs`. A read that costs 0.1 ms is due 0.4 ms after it last ran;
   one that costs 10 ms is still due 40 ms after. The floor is there for watches nobody is waiting on, where a frame
   is as soon as anyone looks. The factor is what bounds a watch's share of the thread, at a fifth, and it stays:
   a slow watch forced on every write of a run would otherwise take the thread.
2. **Waiting names its watches.** `watchesSettled()` waits for every watch, so one window's ask would lift the
   floor for every window's watches, and would wait on them too. It takes the watches to wait for, the `Watching`
   each `watch` answered: `admin.watchesSettled({ of })`. It resolves once none of those is waiting to run, and
   only those are due early. With none named it is as it is now, and lifts nothing.
3. **The flush is as it is.** It runs what is due, keeps to `flushSliceMs`, always runs one, and leaves the rest
   for the next turn. A waited watch left over by the slice is due at once in the next flush, not at a timer.
   `plan` reads `dueAt`, so the timer it sets for the earliest watch moves with it.
4. **What a waiter lifted ends with the wait.** A watch is waited on from the call until it has run. Its next run
   has its whole gap unless something waits again.

What apack then does is hold, for each window, the `Watching` of each view it follows, which its view host has, and
wait on those.

### What it does not change

- A watch nobody waits on: its gap, the commits it skips while it waits, and what the tally counts.
- The warning for a watch that keeps costing more than the slice.
- `minimumGapMs` itself. Lowering it for every watch is the other way to shorten the wait, and it is what the
  stress reading was taken to avoid.

## How it is checked

- `packages/ears/tests/engine/watch-schedule.spec.ts`, on its own clock: a waited watch runs after its factor and
  before the floor; one not named waits out the floor beside it; a watch whose read costs more than the floor waits
  as long waited on as not; the wait ends with the run; `watchesSettled({ of })` resolves with another watch still
  waiting.
- `packages/ears/bench/watch-stress.ts`, run again with nothing waiting: no reading moves.
- apack's `rows-cost.mts`, once apack is pinned to it and waits on its windows' watches by name: the first row
  falls from a frame to about what the watch's read costs, and the other two do not move.
