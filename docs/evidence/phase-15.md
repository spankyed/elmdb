# Phase 15: platforms, packaging and final validation

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Node 23.11.1. elmdb pinned at `fd0c0d7`; apack's branch
`session/ears-on-elmdb`.

## elmdb's CI

The full matrix by dispatch, at `2bfe5e9` (the pinned `fd0c0d7` and two commits of docs):
[run 40202507275](https://github.com/spankyed/elmdb/actions/runs/40202507275) passed — checks on Linux x64, and the
binding built and every suite run, the model and crash harnesses included, on macOS arm64 and x64, Linux x64 and arm64
with glibc and musl, and Windows x64.

## The packaged app

`npm run build:preview -- --version 0.4.0-preview.1` builds `apack Preview.app` unsigned in `dist/preview`.

- **No symlink leaves the bundle.** The 14 symlinks it holds are Electron's framework's own, each resolving inside the
  bundle; `node_modules/elmdb` is a directory of real files. The first build copied what the linked checkout holds
  beside the package — the crate's `src/`, `build.rs`, `Cargo.toml`, and a stray script in `protocol/` — so
  `electron-builder.mjs` leaves out all but what elmdb's `package.json` publishes: the copy is `index.js`,
  `package.json`, `protocol/schema-1.json` and `elmdb.darwin-arm64.node` (apack `e35176874`).
- **The binding loads in the packaged runtime**: `build/prod/smoke-native.mjs` runs the app's executable as Node and
  opens, writes, reopens and reads an elmdb database (with node-pty and the keyring beside it).
- **It boots to `connected` against an isolated data directory**: the E2E smoke suite pinned to the packaged
  executable (`APACK_HOST_EXECUTABLE`), each worker in a data directory of its own — 5 of 5 pass, connected 1.9s after
  launch.

## apack's full chain and E2E

The first `npm run chain -- --e2e` stopped at `test:e2e`, two cases failing:

- `surface-sends`, a browser bookmark: the window sends a bookmark of a page with no favicon, which `BrowserBookmark`'s
  shape requires, so elmdb refused the create (a field holding `undefined` is absent once stored) and the refusal ended
  the API. The wire type now says the favicon may be absent, and the bookmark is stored with an empty one; a unit spec
  holds it, and dropping the default fails it (`8ff39ec1b`).
- `host-settings`, an open editor following a pack's activation: a stale spec, failing before this goal's branch.
  Since `361969c75` made the slash commands the app's, default-setup contributes no `commandSuggestion` extension to an
  editor; the case waits for the pack's `referenceSuggestion` instead (`46e7854cb`).

## The semaphore table (D33)

During the final runs every elmdb open on the Mac began failing with `No space left on device (os error 28)`, from
`sem_open`: macOS's table of named POSIX semaphores (`kern.posix.sem.max`, 10,000) was full. LMDB takes two per
database (D24) and removes them as its last process closes the database. Vitest stops a forked worker with SIGTERM,
which Node answers by dying without its `exit` handlers, so every test file's temporary databases stayed open; and the
binding left a database open at `process.exit`, since Node runs no destructor then. 7,428 `ears-*` directories of
temporary databases were left that day. Named POSIX semaphores can't be listed, so the table emptied only with a
restart (2026-10-25). `ipcs` showed the System V sets on the machine to be lmdb-js's, not elmdb's.

- **apack** (`cee3cd229`): a test worker sent SIGTERM exits through `process.exit`, so its exit handlers close what
  `createTemporaryDatabases` opened. A probe showed neither vitest pool ran `exit` handlers before the change, and the
  forks pool does after; `vitest-exit-with-parent.integration.spec.ts` holds it, and removing the handler fails it.
- **elmdb** (`907af80`): the binding keeps the databases each Node environment opened, and closes those still open at
  `process.on('exit')` and at the environment's cleanup hook, with a bounded close (`crates/elmdb-node/src/exit.rs`).
  `test/exit.test.mjs` rebuilds a database's semaphore names from its lock file's hash, checks them against an open
  database, and checks that a process ending by `process.exit`, by running out of work, and by a terminated worker
  thread leaves neither. A cleanup hook alone didn't run at `process.exit`
  ([run 40239309627](https://github.com/spankyed/elmdb/actions/runs/40239309627) failed that case).
- **CI:** the fix passed the full matrix ([run 40240131963](https://github.com/spankyed/elmdb/actions/runs/40240131963)).
  A mutant whose close closes nothing ([run 40240133958](https://github.com/spankyed/elmdb/actions/runs/40240133958))
  fails the `process.exit` case on macOS, both semaphores still there; its natural-end and worker cases pass, Node's
  teardown having released those already, so the exit case is the one that guards the fix. Locally after the restart:
  `cargo test --workspace` 170 pass, the binding's 23 tests pass.

## The final gate

- **apack's full chain with E2E**: `npm run chain -- --e2e` passed on `6100f8d64` (`master` merged in, 1,288.8s, 32 of
  33 steps run, `test:native` in the Linux container), and again on `63bad6b9e` with the steps whose inputs moved.
  Before it, two repo checks needed the worker change accounted for: the `process.exit` rule now covers scripts that
  import `bounded-spawn` rather than name it, and the subprocess inventory records the guard's second fork
  (`d5cba302b`); the guard moved into `@apackdev/testing` with `master`'s `a833e9568`, and kept its SIGTERM handling.
- **Draft PR** [#276](https://github.com/spankyed/apack/pull/276) against `master` (D16).
- **macOS compiler cases**: [run 40244728980](https://github.com/spankyed/apack/actions/runs/40244728980) passed, on
  `6100f8d64`.
- **Strict chain**: [run 40244727535](https://github.com/spankyed/apack/actions/runs/40244727535) failed one case,
  `dep-files.integration.spec.ts`: in CI `vendor/elmdb` is a clone inside the repo (D21), so the typecheck legs' reads
  of elmdb's declarations under it were reads no leg declared, where locally the link resolves outside the repo. Those
  reads are a pinned dependency, as `node_modules`' are under the lockfile, and `readsOf` drops them on the same
  ground (`63bad6b9e`). [Run 40247866928](https://github.com/spankyed/apack/actions/runs/40247866928) passed on
  `63bad6b9e`: producers, both integration shards, both native shards, and the app half.
