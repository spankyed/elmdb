# Phase 8: crash, failure and fuzz suites

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Rust 1.99.1; CI on GitHub's runners.

## What was built

- **The crash harness** (`engine/tests/model.rs`, `mod crash`): a child process, this test binary run again, applies a
  seeded workload with syncs on as the app runs, acknowledging each step on stdout. The parent kills it at a step past
  the prelude (`6 + seed × 7` modulo the workload's length), drains what it acknowledged, reopens the files and
  compares them with the model after the acknowledged steps, or after one more: the step that may have committed
  before its acknowledgement was written. 48 seeds; on the Mac about half are killed mid-workload and the rest after
  `done`, which checks a clean reopen too.
- **The failed-sync harness** (`store/sync/tests.rs`, `failed_syncs`): a property over random sequences of commits,
  `durable()` waits, sync results (ok or `EIO`) and clock advances, on a manual syncer and clock. A wait resolves only
  once a sync covering its commit succeeded, rejects only when the sync it waited on failed, and the commits a failed
  sync left owed are synced by a later one. 48 cases.
- **Binding fuzzing** (`crates/elmdb-node/test/fuzz.test.mjs`): random bytes and random msgpack as batches, reads and
  loads; valid batches and reads with bytes flipped; arguments of the wrong type to every method of a database and a
  unit; and a closed database and an ended unit refusing every call. Each throws an error carrying an elmdb code, or
  applies, and the database's `check()` stays clean. The junk paths `backup` and `replaceWith` are handed land in a
  temporary directory.

## Deliberately broken durability paths

Each mutation was made on a copy (`$S/mut/ws`, its own target directory), never the checkout:

| Mutation | Caught by |
|---|---|
| a commit aborted but reported `Ok` (`Store::commit_txn` drops the transaction and answers success) | the crash harness: the reopened files lack acknowledged writes |
| a failed sync counted as synced (the worker advances `synced_through` on `Err`) | the failed-sync harness: a wait resolves after its covering sync failed |

## CI

The full matrix by dispatch (D20), every job building the binding and running every suite on it:

- Run [40167101450](https://github.com/spankyed/elmdb/actions/runs/40167101450): Windows and Linux arm64 musl failed,
  neither in elmdb. `@napi-rs/cli` 3.10.9 timed out on its own filesystem-reconciliation lock on Windows, and GitHub
  runs no JavaScript action inside an Alpine container on arm64.
- Run [40167559058](https://github.com/spankyed/elmdb/actions/runs/40167559058), after building Windows' binding with
  cargo and running the musl jobs in Alpine through `docker run`: napi asked for a cross linker on the arm64 runner,
  where the native `gcc` links, and the test that loads a second copy of the binary couldn't remove it on Windows,
  which holds a loaded binary's file open until the process exits.
- Run [40169607512](https://github.com/spankyed/elmdb/actions/runs/40169607512) at `a740ddd`: **every job passed** —
  macOS arm64 and x64, Linux x64 and arm64 with glibc and musl, Windows x64.
- Run [40170026515](https://github.com/spankyed/elmdb/actions/runs/40170026515) at `cfcf6c4`, after the binding's
  build and test tooling moved to a private root package (so a project linking the binding installs none of it):
  every job passed again.
