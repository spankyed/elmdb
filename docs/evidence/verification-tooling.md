# goal-verification-tooling: mutation testing, named failpoints and tempfile publication

2026-10-28, on `session/verification-tooling`. Machine: Apple M1 Pro, 10 cores, under the goal's own load (cargo-mutants'
jobs) and others'.

## Phase 1: cargo-mutants

cargo-mutants 27.1.0, `.cargo/mutants.toml`: the engine's suite (`test_package = ["elmdb"]`), the tests themselves,
benches, examples and xtask excluded, output under `target/`. The suite takes about 76 s idle, 63 s of it the model
harness, and 24 s without it; so a first pass runs every mutant against the suite without the model harness
(`-- -- --skip model::the_engine`) and a second (`--iterate`) only those it missed, against the whole suite.

### The baseline

The first reading of each module, against the fast suite, and what became of each survivor. Mutants counts every
mutant of the module without the configuration's exclusions; caught and unviable are together where a run split over
two invocations kept only the survivors separately. A timeout is a mutant that makes a loop never end: the suite's
deadline catches it.

| Module | Mutants | Caught | Unviable | Missed | Timeout | Survivors: killed / excluded / finding |
|---|---|---|---|---|---|---|
| `engine/trigram.rs` | 66 | 55 | 8 | 3 | 0 | 1 / 2 / 0 |
| `engine/entities.rs` | 91 | 86 (both) | | 5 | 0 | 2 / 3 / 0 |
| `engine/reads.rs` | 369 | 294 (both) | | 75 | 0 | 62 / 12 / 1 |
| `engine/text/search.rs` | 232 | 177 (both) | | 50 | 5 | 43 / 7 / 0 |
| `engine/values.rs` | 116 | 82 (both) | | 34 | 0 | 31 / 3 / 0 |
| `schema/diff.rs` | 66 | 64 (both) | | 2 | 0 | 2 / 0 / 0 |

Of `reads.rs`' 75, the whole suite (the model harness with it) caught 5 of the 12 it was run on before the specs for the
rest made it unneeded; the 62 killed count those 5. Each kill is a cargo-mutants rerun of that mutant against the new
specs: `--iterate` over `text/search.rs`, `values.rs` and `schema/diff.rs` (81 mutants: 76 caught, the 5 timeouts again),
over `reads.rs` (61: 56 caught; of the 5 missed, the step limit's then caught by its new spec, three
excluded as unreached and the finding left), and a fresh run of `entities.rs` (88: 82
caught, 6 unviable, none missed). `trigram.rs`' was its own rerun (`9a978be`).

**What the survivors showed untested**, each now a spec: a query's clauses as parsed; BM25's score over weighted fields,
a fields option and a clause that doesn't match; recency's factor; a one-edit term over a two-edit one; a prefix only for
the last word; search, `near` and `containing` among a set, past its first place and each entity once; highlights joined
across touching terms; suggestions that exclude the word and give two letters no edit; `near`'s and hybrid's scores, and
hybrid's vector list four times `k` deep; a timestamp's three encodings; the kind a refused value names; the value spec's
refusal and the compression threshold; a relation widened from one-to-one and a field's type narrowed; `withRole` after
another filter; the filters before a driving index; a `where` over several types, indexed and walked, in either order;
deep walks, link filters and `edges` past the trash; `where` null over an index; `plainObjects` on a row read and on
every terminal that writes rows or values; false before true; `distinct` and `groupBy` over lists; findLabels' ties in
creation order; `findRelations` by both ends and by neither; EXPLAIN's milliseconds, its stages' times, and the indexes it
names; and a plan of exactly the step limit.

**Excluded as answering what the code answers** (`.cargo/mutants.toml`, each with its reason): trigram's block layout;
entities' two single-field `unique_keys` arms and `check_unique`'s skip of its own entry; a compressed frame within a
byte of its msgpack; a field name with no first letter; the fuzzy DFA's own distance bound; an exact walk's distance-0
match; an exact term already merged at weight 1; a phrase's union checked by `holds`; the trash and the live range
never sharing a place; `nearest`'s first ask; `orderBy` then `limit` through `first_ordered` or the full sort, and its
cut; a stage's answer, which no stage or terminal reads; the timestamp and finite guards no stored value reaches; and
the `withRole` code below.

**Findings:**

- **EXPLAIN's stage times** (`reads.rs`, `pull`): a mutant that charges a downstream stage's time to the stage before it
  changes no count, only milliseconds, which no deterministic reading tells apart. Left as a survivor.
- **Unreached code** (`reads.rs`): `past_filters`' `withRole` arm and `roles_declared`. A `withRole` is always indexed by
  the built-in role index, so it drives rather than stands before a driving index. Excluded, as unreachable.
- **The step limit and the stack** (`reads.rs`, `MAX_STEPS` 256): a plan of 256 steps overflows the 2 MB stack of a
  test thread in a debug build; on 8 MB, Node's main thread's, it runs in debug and release. Nothing holds a read to a
  thread of that size; the spec runs it on one.

### `--in-diff`

PENDING.

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

`cargo test --workspace` three times in a row at the default parallelism, on `c8c00ee`, beside cargo-mutants' three
jobs at a load average of 205 to 250: the engine's 294 cases passed each time, in 151, 164 and 169 s, and every other
suite with them. A failpoint left armed for another test would have failed an unrelated case.

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

elmdb's eight-target CI matrix (run 40674565378, a dispatch on `9a978be`, which holds Phases 2 and 3) passed:
macOS arm64 and x64, Linux x64 and arm64 on glibc and musl, and Windows x64, each building the binding and running
every suite, the Windows backup mutation script among them.
