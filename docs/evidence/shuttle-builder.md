# Shuttle: the index builder's scheduling

The evidence for the builder's model (`docs/architecture/testing.md`, Concurrency models). 2026-11-01, on
`session/rank-models` from `71ecd47`, on an Apple M1 Pro (macOS), Rust 1.99.1, Shuttle 0.9.7, 100,000 schedules per
model (a third random, a third each under PCT at depths 2 and 3), other builds running (a load average about 7).

## The model

`engine/builder/model.rs` runs the builder's `Core` on Shuttle's threads, with a stand-in for what a piece of work does
and a clock the case moves:

| Case | Holds |
|---|---|
| `work_queued_is_run_after_it_was_queued_and_waits_in_the_queue_once` | Two threads queue a rank index racing the builder: each queue call is followed by a run that started after it, and the index never waits in the queue twice |
| `a_waiter_returns_once_the_work_queued_before_it_is_done` | A waiter that queued an index's work returns only once that work is done |
| `a_stop_ends_the_builder_waiting_pausing_or_working_and_its_waiters` | A stop racing a queue ends the builder whether it is idle, pausing for a rank index on a clock that never moves, or working on an index that polls for the stop, and ends a waiter |
| `a_failure_is_seen_by_the_waiter_its_work_wakes` | A failed piece of work is in `failures` once the waiter returns |
| `a_rank_index_waits_for_its_commits_to_pause` | A rank index's drain starts only once `QUIET` has passed on the clock since its last queue |

All five pass, in 1.3 s together; the build with `--cfg shuttle` into its own target took about 3 minutes from cold.

## What the first mutation run found

With the stop case queuing an index's work before the rank index's, the builder was working when the stop came and
ended before it reached the rank index's pause, so a pause that ignored stopping passed every case. The case queues the
rank index first now, and the same mutation deadlocks it.

## Mutations

Each on a disposable worktree, never the working tree, 30,000 schedules a model, the other cases run beside it:

| Mutation | Result |
|---|---|
| `queue` doesn't wake the builder or the waiters | `a_waiter_returns…` and `a_failure_is_seen…` deadlock |
| The builder doesn't mark itself busy as it takes work | `work_queued…`, `a_waiter_returns…` and `a_failure_is_seen…` fail: a waiter returned with the work undone |
| The pause doesn't check for a stop | `a_stop_ends…` deadlocks |
| `queue` adds work already waiting | `work_queued…` fails: the index waited in the queue twice |
| The pause ends at once | `a_rank_index_waits…` fails: the drain started 0 ms after the last commit |
| A failure isn't recorded | `a_failure_is_seen…` fails |

The model harness's rank index, on the same worktree: a trashed entity left in the index fails
`the_engine_holds_what_the_model_holds_after_every_step`, in 12 s.

## Not modelled

The store, the chunks' units, LMDB and `database.rs`'s slot the builder is kept in, which a schema change and a close
take to stop it: the rank index's own specs (`engine/tests/rank_index.rs`) and the model harness run the builder on
real threads. Time past `PATIENCE` is not reached by any case, whose clocks move less than it.
