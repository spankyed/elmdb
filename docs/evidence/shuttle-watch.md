# Shuttle: the unit slot and watch delivery

The evidence for [stage 8 of the verification tooling](../roadmap/verification-tooling.md#8-randomized-concurrency-testing).
2026-10-30, on `session/shuttle` from `session/loom`, on an Apple M1 Pro (macOS), Rust 1.99.1, Shuttle 0.9.7, random
schedules, 100,000 per model unless stated.

## The models

`store/unit_slot.rs`, `mod model`: the three cases Loom runs, under Shuttle. `engine/watch/model.rs`:

| Case | Holds |
|---|---|
| `commits_delivering_together_leave_the_listener_with_the_last_answer` | The last answer heard is the last commit's, none twice in a row, and the listener is put back |
| `a_watch_ended_during_delivery_hears_nothing_after_and_is_dropped_once` | Once `unwatch` returns, the listener isn't called; it is dropped once and nothing stays filed |
| `a_listener_ending_its_own_watch_is_dropped_once` | A listener's own `unwatch` drops it once |
| `listeners_ending_each_others_watch_do_not_wait_on_each_other` | Two listeners ending each other's watch from two committing threads finish |
| `a_commit_racing_a_tracked_read_names_its_token_or_the_read_saw_it` | No commit is missed by a read that didn't see it |

All eight pass, in 1.7 s together; the build with `--cfg shuttle` into its own target took about 3 minutes from cold.

## What the first run found

`unwatch` removed the watch under the tracking lock and returned, while a delivery on another thread had already read
its answer and was about to call the listener: the listener was called after `unwatch` returned, against its
documentation. 1,000 schedules didn't reach it; 200,000 did, in 0.16 s, seed `13875660750434334592`. `unwatch` now waits
for a delivery holding the listener on another thread, and doesn't wait when called from inside a listener of the same
database.

## Mutations

Each on a disposable worktree, never the working tree, the other cases run beside it:

| Mutation | Result |
|---|---|
| A delivery puts the listener back without checking `again` | `commits_delivering_together…` fails: heard `[0, 1]`, the last commit 2 |
| A token read files its read set without checking the sequence | `a_commit_racing_a_tracked_read…` fails: the commit missed a read that didn't see it |
| `claim_writer` checks the slot, unlocks, then takes the writer (the race fixed in `33a17bd`) | `a_write_never_waits…` fails |
| `unwatch` doesn't wait for a delivery on another thread | `a_watch_ended_during_delivery…` fails: the listener was called after `unwatch` returned |
| `unwatch` waits even inside a listener | `listeners_ending_each_others_watch…` and `a_listener_ending_its_own_watch…` deadlock |
| `unwatch` inside a listener answers `Ended` while a delivery holds the ended watch's listener (2026-10-31, 3,000 schedules a model) | `a_listener_ending_its_own_watch…` fails: its own delivery holds it; `listeners_ending_each_others_watch…` fails: `Ended` before the other listener was dropped |

Each failed only the case written for it.

## Not modelled

The listener dropped outside the callback guard, which the roadmap names: the guard is what `Database::close`
consults from a listener, and the database, its store and LMDB are outside the model. `engine/tests/tracked.rs` covers
close from a listener on real threads.
