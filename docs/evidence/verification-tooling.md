# goal-verification-tooling: mutation testing, named failpoints and tempfile publication

2026-10-28, on `session/verification-tooling`. Machine: Apple M1 Pro, 10 cores, under the goal's own load (cargo-mutants'
jobs) and others'.

## Phase 1: cargo-mutants

cargo-mutants 27.1.0, `.cargo/mutants.toml`: the engine's suite (`test_package = ["elmdb"]`), the tests themselves,
benches, examples and xtask excluded, output under `target/`. The suite takes about 76 s idle, 63 s of it the model
harness, and 24 s without it; so a first pass runs every mutant against the suite without the model harness
(`-- -- --skip model::the_engine`) and a second (`--iterate`) only those it missed, against the whole suite.

PENDING: the baseline.

## Phase 2: named failpoints

`fail` 0.5.1, a dependency of `elmdb` without features and a dev-dependency with `failpoints`
(`crates/elmdb/src/failpoints.rs`). The hooks replaced, each now a point in `failpoints::points`:

| Hook | Point | Armed |
|---|---|---|
| `FAIL_NEXT_COMMIT` | `store::commit` | once, on the thread |
| `FAIL_COMMIT_AT` | `store::commit` | once, for a path (the model harness's `RefuseNext`) |
| `FAIL_STAMP_AT` | `store::stamp` | once, for a path |
| `FAIL_SYNC_AT` | `store::sync` | for a path until its guard drops (`FailedSync`) |
| `FAIL_RESET_CHECKPOINT` | `store::reset_checkpoint` | once, on the thread |
| `BEFORE_WRITE_RESERVATION` | `store::before_write_reservation` | a run, on the thread |
| backup's `HOOK` at `Ready` | `backup::ready` (a run), `backup::publish` (a failure) | on the thread |
| backup's `HOOK` at `Published` | `backup::published` | once, on the thread |
| `BEFORE_SCHEMA_RECOVERY`, `BEFORE_SCHEMA_WRITE` | `database::before_schema_recovery`, `…_write` | a run, on the thread |
| `BEFORE_LOAD_PUBLISH`, `AFTER_LOAD_PUBLISH` | `dump::before_load_publish` (a run), `dump::after_load_publish` | on the thread |
| `BEFORE_READ_SNAPSHOT` | `engine::before_read_snapshot` | a run, on the thread |
| `FAIL_NEXT_INDEX_REFRESH` | `engine::index_refresh` | once, on the thread |
| `PANIC_NEXT_READ` | `watch::read` | a run that panics, on the thread |
| `FAIL_NEXT_NEW` | `derive::scratch` | once, on the thread |

Not hooks, and left: `sorted.rs`'s `TEST_BUDGET`, a size override, and `watch.rs`'s `CALLBACKS`, production state.

- **The tests that used them pass unchanged in what they assert.** One changed its evidence and not its claim: the
  backup case that asserted the hook's path argument was a `.elmdb-backup-` file now asserts, at the ready point, that
  the destination holds one, a run having no argument.
- **Two defects the conversion found in its own first version**, each by the suite: a path armed twice before firing
  failed two commits where the hook's `Option` had failed one (the model harness, `RefuseNext` twice), and reading the
  thread's armings during the thread's teardown panicked, where a watch can be delivered from another thread-local's
  destructor (`watch_delivery_from_thread_exit_destructors_is_guarded`); the old hook had used `try_with` for it.
- **Each test's faults are its own:** `failpoints::tests` shows a point armed on one thread doesn't fire on another
  and fires once on its own, a path's fires for that path once on any thread, a guarded path's until the guard drops,
  and a run runs once. `fails_here` mutated to answer `fail`'s process-wide registry fails the first
  (`another thread doesn't hear it`), on a copy-on-write clone.
- **Nothing in a release build:** `cargo tree -e features -p elmdb-node -i fail` shows `fail` with its default
  features only; `cargo tree -e features,dev -p elmdb -i fail` shows `failpoints` from elmdb's dev-dependencies. The
  release `libelmdb_node.dylib` (9,113,904 bytes) holds none of the points' names (`strings`) and no `fail` symbol
  (`nm`).
- `cargo xtask check-one-lmdb` and `check-generated` pass with the new dependency.

PENDING: three workspace runs at default parallelism.

## Phase 3: tempfile publication

tempfile 3.27.0's `persist` (`src/file/imp/{unix,windows}.rs`) against elmdb's `publish_new` and `publish`:

| Guarantee | elmdb before | tempfile `persist_noclobber` |
|---|---|---|
| Linux: no-replace rename | `renameat2(RENAME_NOREPLACE)`, through `libc` | the same, through rustix |
| Linux: where unsupported | a hard link on `ENOSYS`, `EINVAL` or `EOPNOTSUPP` | a hard link and an unlink on `ENOSYS` or `EINVAL`; `EOPNOTSUPP` is returned |
| macOS: no-replace rename | `renamex_np(RENAME_EXCL)`, no fallback | the same call, with the hard-link fallback |
| Windows: move without replace | `MoveFileExW(WRITE_THROUGH)` | `MoveFileExW(0)`: no write-through |
| Windows: the temporary attribute | cleared, other attributes kept | every attribute replaced by `FILE_ATTRIBUTE_NORMAL` |

So on Unix `publish_new` is `TempPath::try_from_path(source)?.persist_noclobber(target)`, keeping the source on a
refusal, with elmdb's link for `EOPNOTSUPP` around it, and the unsafe calls are gone; its Linux spec
(`store::backup::publication_tests`) injects all three codes into that link and passes. Windows keeps `move_file`
for both publications, write-through being a flag on the move that nothing around tempfile's call can add. The
backup's replacing publish on Unix stays `std::fs::rename`, which is all tempfile's `persist` does. The dump and backup
specs pass.

PENDING: the eight-target CI matrix (run 40674565378).
