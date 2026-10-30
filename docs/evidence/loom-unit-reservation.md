# Loom: the unit reservation

The evidence for [stage 4 of the development libraries](../roadmap/development-libraries.md#4-concurrency-model-checking).
2026-10-30, on `session/loom` from `779a583`, on an Apple M1 Pro (macOS), Rust 1.99.1, Loom 0.7.2,
`LOOM_MAX_PREEMPTIONS=3`.

## The model

`crates/elmdb/src/store/unit_slot.rs`, `mod model`, over the `UnitSlot` the store uses:

| Case | Holds |
|---|---|
| `a_write_never_waits_for_the_writer_while_a_unit_is_reserved` | A plain write that goes on to take the writer finds no unit reserved |
| `of_two_units_one_reserves` | Exactly one of two units reserves |
| `a_waiter_returns_when_the_unit_ends_or_it_is_stopped` | A waiter returns once the unit ends, and once woken after its stop is set |

All three pass. The build with `--cfg loom` into its own target took 2 min 45 s from cold; the models themselves ran in
under a hundredth of a second.

## Two mutations

Each on a disposable worktree, never the working tree:

| Mutation | Result |
|---|---|
| `claim_writer` checks the slot, unlocks, then takes the writer: the race fixed in `33a17bd` | The write case fails: "a write began taking the writer of a reserved unit" |
| `release` clears the slot without `notify_all` | The waiter case fails: Loom reports a deadlock, both threads blocked |

Every other case passed under each mutation.
