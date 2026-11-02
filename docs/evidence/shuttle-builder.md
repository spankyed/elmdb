# Shuttle: the index builder's scheduling

The evidence for the builder's model (`docs/architecture/testing.md`, Concurrency models). 2026-11-02, on
`session/rank-models` over `e5ecfec`, on an Apple M1 Pro (macOS), Rust 1.99.1, Shuttle 0.9.7, 30,000 schedules per
model (a third random, a third each under PCT at depths 2 and 3), other builds running (a load average about 8).

## The model

`engine/builder/model.rs` runs the builder's `Core` on Shuttle's threads, with a stand-in for what a piece of work does
and a clock the case moves:

| Case | Holds |
|---|---|
| `work_queued_is_run_after_it_was_queued_and_waits_in_the_queue_once` | Two threads queue a rank index racing the builder: after each call returns a run starts, and the index never waits in the queue twice |
| `work_queued_while_it_runs_runs_again` | Work queued while its own run is underway, which is a commit landing as a drain finishes, runs again |
| `a_waiter_returns_once_the_work_queued_before_it_is_done` | A waiter that queued an index's work returns only once that work is done |
| `a_stop_ends_the_builder_waiting_or_pausing_and_its_waiters` | A stop racing a queue ends the builder whether it is idle or pausing for a rank index on a clock that never moves, and ends a waiter |
| `a_stop_ends_the_work_underway_and_its_waiters` | A stop reaches a piece of work that polls for it, and ends a waiter |
| `a_failure_is_seen_by_the_waiter_its_work_wakes` | A failed piece of work is in `failures` once the waiter returns |
| `a_rank_index_waits_for_its_commits_to_pause` | A rank index's drain starts only once `QUIET` has passed on the clock since the last queue the case made |
| `a_steady_stream_of_commits_is_drained_once_patience_has_passed` | Commits that never pause hold a drain back until `PATIENCE`, and no longer |

All eight pass, in about 3.4 s together.

## What mutation runs found

- With the stop case queuing an index's work before the rank index's, the builder was working when the stop came and
  ended before it reached the rank index's pause, so a pause that ignored stopping passed every case.
- With it queuing the rank index first, the clock never moving ended the builder in the pause, so it never reached the
  work: the case's polling was dead and `Core::stopping -> false` passed. So they are two cases.

Two gaps a reading found rather than a run, each now holding a mutation below: with the clock held until every queue call
returned, no run could start before one, so `work_queued…` couldn't see a `queue` dropping work underway, which is what
`work_queued_while_it_runs_runs_again` is for; and a pause checked against the builder's own `queued_at` couldn't see that
record going stale, so the case keeps its own.

## Mutations

Each on a disposable worktree, never the working tree, 30,000 schedules a model, the other cases run beside it:

| Mutation | Result |
|---|---|
| `queue` doesn't wake the builder or the waiters | `a_waiter_returns…`, `a_failure_is_seen…`, `work_queued_while_it_runs…` and `a_stop_ends_the_work_underway…` fail |
| The builder doesn't mark itself busy as it takes work | `work_queued_is_run…`, `a_waiter_returns…` and `a_failure_is_seen…` fail: a waiter returned with the work undone |
| The pause doesn't check for a stop | `a_stop_ends_the_builder_waiting_or_pausing…` and `a_steady_stream…` fail |
| `queue` adds work already waiting | `work_queued_is_run…` fails: the index waited in the queue twice |
| `queue` skips work underway | `work_queued_while_it_runs…` fails: it never ran again |
| The pause ends at once | `a_rank_index_waits…`, `a_steady_stream…` and `work_queued_is_run…` fail |
| `queued_at` isn't refreshed by a later queue | `a_rank_index_waits…` and `a_steady_stream…` fail |
| The pause has no cap | `a_steady_stream…` fails: it never ends |
| A failure isn't recorded | `a_failure_is_seen…` fails |
| `Core::stopping` always answers false | `a_stop_ends_the_work_underway…` fails: the work never ends |

The model harness's rank index, on the same base:

| Mutation | Result |
|---|---|
| A commit never hands the builder its rank index's pushes | `the_engine_holds_what_the_model_holds_after_every_step` fails in 2 s: "the drain left a bound of 0.2", which only its check of the bound against the tolerance shows, every state being within the bound it reports |

The harness took 119 s on this base, with other builds running.

## Not modelled

The store, the chunks' units, LMDB and `database.rs`'s slot the builder is kept in, which a schema change and a close
take to stop it: the rank index's own specs (`engine/tests/rank_index.rs`) and the model harness run the builder on
real threads.
