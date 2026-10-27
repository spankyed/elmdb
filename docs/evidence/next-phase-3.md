# Phase 3 of goal-elmdb-next: what a user action costs

Readings of 2026-10-27, apack `session/elmdb-next`, elmdb `c345182` (no elmdb change in this phase). Machine: Apple
M1 Pro, 10 cores, 32 GiB, macOS 26.3.1, Node 23.11.1, under other load throughout (load average 7 to 11; apack's
`npm run check:idle` read 71% idle after them, at its 70% floor), so each figure is a reading of that machine and the
comparisons alternate their builds.

## The count

apack's `admin.calls` (`packages/apack-ears/src/calls.ts`, kept in `createConnection`, commit `48fbb9c3d1`) counts each
read, each write outside a unit and each unit with its writes, with the milliseconds each took (encoding and decoding
included) and the code that made it: the first three stack frames outside the engine. Watch runs are counted per watch
under the view host's label (`<ref> · <view>`), with the states each skipped. Off, it costs one check per call. A
development app offers both engines' counts, the app's and the run history's, as `HostRuntime.diagnostics`, which
console code reads as `engineCalls`; a reading is `npm run dev:query -- 'engineCalls.start(); return true'`, the
action, then `npm run dev:query -- 'return engineCalls.stop()'`, against a `npm run dev:open -- --fresh --rm` session.
Specs: `packages/apack-ears/tests/engine/calls.spec.ts` (a read, a write and a unit counted at their call site, nothing
when off), `apack-host/tests/services/host-runtime.spec.ts` (both engines, only when asked) and the console's.

## A chat turn and a flow run

Each driven against a fresh development profile through the threads surface: a chat turn is `SEND_MESSAGE` in a new
thread, answered by the Claude Code CLI on the user's login (G9), and ends when the log records `cc.stream.completed`;
the flow run is the Command Listener's `instructions` command (`SEND_COMMAND`), which runs the Set Instructions action
and calls no model. Each is one reading.

| Action | Wall | App reads | App writes (no unit) | Watch runs | Run history reads / writes |
|---|---:|---:|---:|---:|---:|
| Chat turn, a one-word reply | 7 s | 272, 11.0 ms | 36, 3.2 ms | 43, 8.3 ms | 101, 3.6 ms / 28, 3.8 ms |
| Chat turn, about 400 words, before the scheduler | 23 s | 851, 48.4 ms | 216, 104.6 ms | 225, 39.4 ms | 101, 3.9 ms / 28, 2.7 ms |
| The same, after it | 24 s | 858, 59.5 ms | 222, 158.3 ms | 229, 49.5 ms | 101, 3.3 ms / 28, 3.0 ms |
| Flow run, before | 4 s window | 92, 5.8 ms | 12, 1.0 ms | 16, 4.5 ms | 53, 2.1 ms / 18, 1.5 ms |
| Flow run, after | 4 s window | 84, 4.4 ms | 12, 1.1 ms | 14, 3.4 ms | 53, 1.2 ms / 18, 1.5 ms |

No action wrote in a unit. Watch runs' time is inside the reads' time, not beside it.

**Where a streamed reply's time goes.** The long turn's costliest site is the threads repository's
`updateMessageState` (`packages/default-setup/src/features/threads/be/repository/`), once per streamed chunk: an
existence read (`query.ids(id).first()`), then the write builder's own existence check, then the write, 181 times,
120 ms in all. The `threads · message` view runs once per chunk after it (192 runs, 41 ms, one state skipped). The
logs system reads its feature settings once per entry (45 reads, 2 ms).

**The loops the readings show, against Phase 3's rule.** elmdb's share of a chat turn is under 1% of its wall time
(about 210 ms of 23 s), and of the flow run about 15 ms. The largest loop, the per-chunk existence read before the
write, is 15 ms of 23 s, 0.06%; every one-entity loop is a stream writer's or a logger's, one call per event, not a
loop over a list. None costs a measurable share of its action, so none was turned into a set read or a unit, and the
"after" readings differ from the "before" ones only by the scheduler. The chat turn's stream commits about every
80 ms, below the scheduler's one-frame floor, so the scheduler held none of its views back: the turn's watch runs and
deliveries are the same before and after, which is the point of G14's "a cheap watch stays immediate".

## Watches bounded under any write rate (G14)

`npm run bench:watch -w @apackdev/ears` (`packages/apack-ears/bench/watch-stress.ts`): a writer sets a field of one of
40 open tasks on every turn of the event loop, into 20,000 tasks, under three views — one row (0.02 ms a run), the
indexed `where` with a `limit` of 50 (2.1 ms) and every open task ordered (about 6,700 rows, 15.5 ms) — for 5 s, in
the built engine, synced as the app's is. `heard` is from a commit to the first delivery reflecting it.

| | Writer alone | Before the scheduler | After it |
|---|---:|---:|---:|
| Commits a second | 19,035 | 51 | 11,899 |
| Share of a core | 0.84 | 1.01 | 0.91 |
| Event loop delay p50 / p99 / max | 1.0 / 1.1 / 1.7 ms | 19.4 / 20.6 / 21.1 ms | 1.0 / 18.0 / 19.1 ms |
| Ordered view: runs a second, share of the wall | | 51, 78% | 13, 19.5% |
| Ordered view heard p50 / p99 / max | | 19 / 20 / 43 ms | 48 / 77 / 86 ms |
| Page view: runs a second, share of the wall | | 51, 10.6% | 51, 10.6% |
| Page view heard p50 / p99 / max | | 2.1 / 2.3 / 25 ms | 10.9 / 25 / 31 ms |
| Converged after the writer stopped | | yes | yes, in 25 ms |

Before, every commit's flush re-ran every view before the writer's next turn, so the views set the writer's pace: 370
times slower than alone, a whole core, and the event loop blocked for a frame on every turn. After, the writer runs at
63% of its speed alone, and the ordered view's share is the bound its constant sets.

**The constants** (`WATCH_SCHEDULE`, `packages/apack-ears/src/watch.ts`): a watch waits `4 ×` its last run's time, and
at least 16 ms, before running again; a flush starts no run past 8 ms of runs. A factor of 4 caps any one watch at
1/(1+4) = 20% of a core, which the ordered view's 19.5% shows, and bounds how late an expensive view hears a commit
by about (1+4) times its run, which its 86 ms against 5 × 15.5 ms shows. The floor of a frame is what bounds a cheap
watch's runs (at most about 62 a second however fast commits come), and is what Phase 3's done-when, a thousand
commits on separate turns costing a bounded number of runs, needs: a delay proportional to a run that costs nothing
is no delay. An 8 ms slice keeps a flush of several watches under half a frame; one run longer than the slice can't
be split, which is why the p99 delay after is the ordered view's run.

Specs on a clock the spec moves (`packages/apack-ears/tests/engine/watch-schedule.spec.ts`): a thousand commits on
separate turns cost at most `1000 / 16 + 2` runs and deliver the last result; a watch idle when a commit lands runs
without a timer; a cheap watch hears each commit a frame apart while an expensive one is in its gap; four 5 ms
watches run one a flush, oldest first; a watch always runs once per flush whatever its cost; a watch over the slice
three runs in a row is warned about once, by its label; skipped states are counted. The model-based harness's watch
property (`tests/engine/model-based.spec.ts`) passes unchanged: every watch, after every op, holds what its read
answers afresh.

The first version deferred a watch whose last run cost more than the slice whenever any time had passed in the flush,
which is always, so such a watch never ran again; the engine suite under load hung on it once (`watch.spec.ts`,
"runs once for a burst of commits", its first run over 8 ms). A flush now always runs its first due watch, and the
"always runs one watch a flush" case is the one that fails if that goes.

## G4: apack's bench before and after

`npm run bench -w @apackdev/ears`, the engine before this phase (`5838145675`) and after it (`19214e4d38`), built and
run alternately twice; each figure the lower of its two medians, in ms.

| Case | Before | After | |
|---|---:|---:|---:|
| bulk load 50k entities, 200k attributes, 100k relations | 1266.7 | 1258.5 | −0.6% |
| query by type with where and pickAll | 37.6 | 37.9 | +0.7% |
| 1,000 where lookups by a unique value | 7.21 | 7.18 | −0.4% |
| 10 where lookups on an unindexed field | 243.7 | 247.4 | +1.6% |
| 1,000 getRow | 4.78 | 4.72 | −1.4% |
| pickAll over 40k | 88.0 | 87.8 | −0.3% |
| link 10k tasks to one project, then destroy it | 794.0 | 818.5 | +3.1% |
| 1,000 creates | 115.4 | 117.2 | +1.6% |
| transact: 1,000 creates as one unit | 26.1 | 26.2 | +0.4% |
| 1,000 creates, then 100 watches on a page of the type run | 116.3 | 120.2 | +3.4% |
| 1,000 page reads, tracked as a watch runs them | 170.9 | 171.8 | +0.5% |

Every other case is within ±2.5%; none is past the 10% tolerance. The single-row read through the binding is elmdb's,
which this phase didn't change.
