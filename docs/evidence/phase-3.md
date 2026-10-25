# Phase 3: format, storage and durability

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Rust 1.99.1, heed 0.23.0.

## What was built

- **The key codec** (`format/key.rs`): format 4's tags, with strings and bytes escaped as FoundationDB's tuple layer
  does (`00` as `00 FF`, ended by `00`), so any byte may appear. Property tests: byte order is tuple order, decode
  inverts encode, and a prefix's range holds exactly the keys that extend it.
- **The prefix table** (`format/mod.rs`): every prefix, its tag and whether it is primary, derived or meta; a spec
  holds each tag to its letter.
- **The value spec** (`format/value.rs`): msgpack walked marker by marker before it is decoded, since rmpv reads the
  reserved marker `0xc1` as nil; extensions refused but the timestamp; map keys strings; numbers finite.
- **The store** (`store/mod.rs`): one environment, one database, the format stamp written and synced before anything
  else, `NOT_A_DATABASE` for another program's files and for lmdb-js's data version, `WRONG_FORMAT` for another
  format, `READ_ONLY`, reads from any thread, and map growth.
- **Group commit** (`store/sync.rs`): the policy a pure function of instants, the flush and the clock behind traits.
- **The binding**: open with the options and a schema, `durable()` as a promise rejecting with elmdb's code, the
  schema and the open report typed from the generated protocol, and a second elmdb binary refused at open.

## The ports

| apack spec (`packages/apack-ears/tests/lmdb/`) | Here |
|---|---|
| `durability.spec.ts`, all six cases | `store/sync/tests.rs`, each sync finished or failed by the test |
| `store.spec.ts`: the burst and the steady stream | `store/sync/tests.rs`, with a manual clock |
| `store.spec.ts`: a durable unit, `durable()` owing nothing | `store/tests.rs`, `a_durable_write_commits_with_its_sync_and_owes_nothing_after` |
| `store.spec.ts`: a writer killed with `SIGKILL` as its write returns | `store/tests.rs`, the test binary run as its own child |
| `store.spec.ts`: across a close, read-only refusals | `store/tests.rs` |
| `store.spec.ts`: an older format refused | `store/tests.rs`, a newer and an older format |
| `format-stamp.spec.ts` | `store/tests.rs`, the stamp failing for one path |
| `store.spec.ts`: reset, replace, copy, reader slots | Phase 7 (admin) and Phase 5 (reads) |

The sync specs ran ten times in a row without a failure. While they were written, one hung: a failed assertion
unwinds and drops the worker, whose close then waited on a sync nobody would finish. The harness now drops the test's
end of the channel first, so a failing test fails.

## Map growth

The map starts at twice the data file and 64 MiB at least, and doubles in 64 MiB steps (D7). What one growth costs, a
64 KiB-value unit of 6.25 MiB into a nearly full 64 MiB map against the same unit afterwards (`what_a_growth_costs`,
release build, three runs):

| Unit that grew the map | The same unit after |
|---|---|
| 1.27 ms | 1.06 ms |
| 1.38 ms | 1.08 ms |
| 1.36 ms | 1.11 ms |

About 0.3 ms per growth, which a doubling makes rare; starting larger would cost Windows its disk, where LMDB sizes the
file to the map.

## Checks

`cargo test --workspace` (53 passed, 1 measurement ignored), `cargo clippy -- -D warnings`, `cargo fmt --check`,
`cargo xtask check-generated`, `cargo xtask check-one-lmdb`, the binding's `npm run typecheck` and `npm test`
(6 passed).
