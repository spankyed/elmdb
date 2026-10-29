# Tooling for tests that can fail, faults by name, and measured bounds

Status: partial implementation. Stage 3 has dedicated dhat assertions for selected value/text rebuilding and export
(2026-10-27); stages 1, 2 and 9 were built on 2026-10-28 (`../archive/goals/goal-verification-tooling.md`, Outcome),
each with what it leaves below. Other stages remain planned. The commands are in `../architecture/testing.md`, Repeatable
maintenance measurements, Failpoints and Mutation testing.

## Purpose and scope

The [review](../reviews/elmdb-review-2026-10.md) and its follow-up reviews kept finding the same three things: tests
that passed with their fix reverted, fault injection spread across test-only thread-locals, and resource bounds stated
in prose that the code didn't meet. This plan names the tools that turn each into something a run checks. It sits
beside [Libraries for safer implementation and easier development](development-libraries.md), which covers platform
bindings, Criterion, rustix, Loom and tracing; nothing here repeats those stages, and where one overlaps it says so.

The work is elmdb's alone. Developer tools (`cargo-mutants`, `cargo-nextest`, `samply`) are installed, not
dependencies; the crates are development dependencies or behind a feature that release builds never enable.

## Order and decisions

| Stage | Tool | Replaces |
|---|---|---|
| 1 | cargo-mutants | Hand-made "disposable mutation" proofs and the scripts that archive the tree to break one line |
| 2 | fail | `FAIL_NEXT_COMMIT`, `FAIL_SYNC_AT`, `FAIL_RESET_CHECKPOINT`, `PANIC_NEXT_READ`, `FAIL_NEXT_INDEX_REFRESH` and the `BEFORE_*` hooks |
| 3 | dhat | RSS readings as the evidence for a memory bound |
| 4 | cargo-nextest | The `bounded_watch_child` harness and `--exact` child-test selection |
| 5 | proptest-state-machine | Small hand-written independent models (H04) |
| 6 | samply | Native profiles that macOS refused without sudo, and V8 `--prof` symbol guesses |
| 7 | iai-callgrind | Wall-clock comparisons on a loaded machine; complements Criterion (development-libraries.md, stage 2) |
| 8 | shuttle | Interleavings Loom can't reach at a tractable bound; complements Loom (development-libraries.md, stage 4) |
| 9 | tempfile `persist_noclobber` | The hand-written no-clobber publication in `store/backup.rs` |

Stages 1–3 come first: they target the three recurring findings. The rest are independent and each is its own change.

## 1. Mutation testing

Run [cargo-mutants](https://mutants.rs/) over `crates/elmdb`. It mutates each function in turn (returns replaced,
conditions negated, operators swapped) and reports every mutation the test suite still passes, which is a list of
tests that cannot fail.

Acceptance:

- A documented command, scoped with `--file` or `--re` so a run over one module is minutes, not the whole crate.
- A first full run's surviving mutants are triaged into a recorded list: each gets a test, or a stated reason it is
  equivalent behavior. Start with `store/`, `engine/watch.rs`, `engine/derive/` and `elmdb-node/src/process.rs`.
- The repository's mutation scripts (`scripts/check-*-mutation*.py`) are replaced where cargo-mutants expresses the
  same mutation; platform-specific ones it can't reach (a Windows attribute, a real read-only mount) stay.
- CI runs it on the files a change touches (`--in-diff`), reporting survivors rather than failing, until the
  surviving list for those files is empty.

**Built, 2026-10-28:** `.cargo/mutants.toml` and the commands in `../architecture/testing.md`, Mutation testing — a
first pass against the suite without the model harness, then `--iterate` against the whole suite — with a baseline
over `engine/trigram.rs`, `reads.rs`, `text/search.rs`, `entities.rs`, `values.rs` and `schema/diff.rs` triaged
(`../evidence/verification-tooling.md`): of 169 survivors, 144 killed (5 by the whole suite, the rest by new specs),
24 excluded as answering what the code answers, and one finding left, EXPLAIN's stage times, which no count shows; the
triage also found a step limit whose plan needs more than a debug test thread's 2 MB stack. The three mutation scripts
make changes cargo-mutants doesn't (an expression swapped for another, a Windows attribute, heap and timing regressions)
and stay, each saying why in its header. The baseline over `store/`, `engine/watch.rs`, `engine/derive.rs` with
`derive/`, and `elmdb-node/src/process.rs` followed (`../evidence/mutation-baseline.md`): of elmdb's 90 survivors, 47
killed by new specs, 13 removed with the dead code they were in, 11 excluded, 2 a finding (`MAX_GROWTHS`, which a spec
would need terabytes of map for) and 17 in `backup.rs`' publication code for other platforms: the Linux 3 caught in a
Linux container, the Windows 14 read on Windows. `process.rs` runs against elmdb-node's suite, its Linux claim caught
in a Linux container. On Windows, of the Windows code's 21 mutants, a new spec killed 2 of the 6 survivors, 3 are
excluded as disjoint flags or equivalent, and `pin_module` joins `claim`'s napi mapping as a finding the binding's suite
catches; those and an acquisition error other than an occupied claim are the findings. CI's `mutants` job runs
`scripts/mutants-in-diff.py` on every pull request, on Linux and on Windows, reporting the survivors in the files it
touches; it replaced `check-windows-backup-mutation.py`, which broke one line of the Windows publication by hand.

## 2. Named failpoints

Use [fail](https://docs.rs/fail/latest/fail/) (TiKV's failpoints) behind a `failpoints` feature. A failpoint is a
named site (`fail_point!("store::commit::sync")`) a test configures to return an error, panic, sleep or call back.
Disabled, the macro compiles to nothing.

Acceptance:

- Every test-only thread-local hook in `store/`, `engine/` and `database.rs` is a named failpoint, and the hook is
  gone. The names follow the module path so a test reads as the site it breaks.
- The existing fault-injection tests pass unchanged in what they assert. Failpoints are process-global, so the tests
  that use them run under `fail::FailScenario` and, with stage 4, one process each.
- A release build of the binding contains no failpoint code (checked by building without the feature).

**Built, 2026-10-28,** otherwise than this says: a process-global failpoint would fire in every test running at the
time, so the tests don't run under `FailScenario` or one process each. A site's condition asks
`crates/elmdb/src/failpoints.rs` whether the point is armed for it — once on the arming thread, once for a database's
path, or for a path until a guard drops — and a spec shows a point armed on one thread doesn't fire on another
(`../architecture/testing.md`, Failpoints). All fifteen hooks are failpoints; `fail`'s `failpoints` feature is a
dev-dependency feature, and the release binding holds no point's name and no `fail` symbol.

## 3. Memory bounds as assertions

Use [dhat](https://docs.rs/dhat/latest/dhat/) in its testing mode: a test installs the profiler as the global
allocator and asserts peak heap bytes. A bound engine.md states becomes a test that fails when it stops holding.

Acceptance:

- Tests for each bound engine.md, Recovery resources, claims: full `check()` and `rebuild()` derivation stays
  within the spill threshold plus the stated overheads, on fixtures large enough that an unbounded collection
  exceeds it. A mutation that collects one index's entries into a `Vec` fails it.
- Single-index rebuild and dump export now stream/spill intermediate collections (c78c623). Dedicated heap
  assertions exercise export and selected value/text rebuilds at two input sizes. Extend coverage to selected
  vector rebuilds and isolated full derivation; remaining full-check/rebuild inventories and reports require
  separate observations rather than an invented constant-heap guarantee.
- dhat measures the Rust heap only. LMDB's mapped pages and the process RSS remain `scripts/measure-recovery.mjs`'s.

Full derivation and a selected vector rebuild keep something per entity by design, so the heap test holds them to
growth between two sizes rather than to an invented constant: a full rebuild or check at most 1,024 bytes an entity
between 6,000 and 12,000 entities whose entries fill every sorter at both (measured 387 to 469, the inventory alone
about 300), and a selected vector rebuild at most 768 bytes a vector between 1,000 and 2,000 (measured 390 to 460). In
disposable copies, full derivation keeping one value index's entries before emitting them grows a full rebuild 3,372
bytes an entity and a full check 3,290, and a vector flush keeping every decoded vector grows a vector rebuild 1,602;
each fails. The heap test takes about three minutes on an M1 Pro.

## 4. One process per test

Run the suite with [cargo-nextest](https://nexte.st/). Each test is its own process, with per-test slow and kill
timeouts in `.config/nextest.toml`.

Acceptance:

- Tests that spawn themselves as a child to bound a hang or isolate process state (`bounded_watch_child`, the
  read-only registration child) become ordinary tests under a nextest timeout, and their marker-string checks go.
- `cargo nextest run` is the documented test command and CI's; doc tests still run under `cargo test --doc`.
- A test filtered to nothing is reported as zero tests run, which CI treats as a failure.

`cargo nextest run` is the documented command and CI's on every target, and cargo-mutants runs through it
(`.cargo/mutants.toml`'s `test_tool`), so a mutant that hangs a test is caught at that test's deadline. The nine watch
tests behind `bounded_watch_child` are ordinary tests under a ten-second deadline; a test hung in a disposable copy was
killed at it and the run failed. A filter selecting nothing exits 4. The read-only registration child stays, with the
other tests that run the binary again: each needs a second process, since this process's open of the same directory is
refused before LMDB's lock file is reached, and its marker check stays because libtest reports a child filtered to
nothing as a pass.
## 5. Model-based property tests

Use [proptest-state-machine](https://docs.rs/proptest-state-machine/latest/proptest_state_machine/), on the proptest
elmdb already uses. A reference model (plain maps of entities, relations and index contents) and the engine take the
same random operation sequence; after each step their answers must agree, and proptest shrinks a failure to the
shortest sequence.

Acceptance:

- One model covering create, set, trash, restore, destroy, link and unlink, with value-index and `where`/`limit`
  queries compared against the model. It is independent of `derive.rs`: it never calls engine code to compute an
  expected answer.
- A mutation in shared derivation that `check()` cannot see (H04's case) fails the model.
- Expanded to schema changes and units only once the first model has caught something or proven cheap to keep.

The model harness (`engine/tests/model.rs`) is this model: every derived read answered by the model alone, units and
savepoints included, and a workload that rebuilds every index or one, so derive.rs's reconstruction, which `check()`
shares, is compared with the model; it compares an order by a field, whole and its first one or three, and a filter's
first two. Its workload is a proptest-state-machine reference machine: each write chosen from what the model holds, one
in eight blind so refusals are still compared, and a chosen step that shrinking left without its target dropped rather
than kept as a refusal. Before, every step picked from the pool blind: over 500 generated workloads, 25.6% of steps were
refused by a rule, and removing or moving a relation succeeded about one time in thirty. With the reference machine,
2.5%, every one of them blind, and both about four times in five; `generated_workloads_complete_every_kind_of_step_and_make_every_refusal`
holds generation to completing every kind of step at least twenty times in 200 workloads and making each of the model's
refusals, and fails with moving a relation no longer chosen from the model. Mutations in disposable copies: a move that
changes only the target and isn't filed again passes the blind harness and fails this one in seconds; a single-index
rebuild that skips an entity, which the hand-written H04 test missed, fails it; a count derived one too many fails it, as
it did H04's test, which, the model covering it, went.

## 6. Native profiling

Use [samply](https://github.com/mstange/samply) for native CPU profiles of the engine and of the binding under Node.
It samples without sudo on macOS and symbolizes Rust frames.

Acceptance:

- A documented command profiles a benchmark or `scripts/measure-recovery.mjs` run, with the build profile that keeps
  symbols (`debug = "line-tables-only"` on a profiling profile, never the release one).
- One recorded profile attributes a known cost to a named Rust function, so the method is shown to work.

## 7. Instruction-count benchmarks

Use [iai-callgrind](https://github.com/iai-callgrind/iai-callgrind) for the operations Criterion times. It counts
instructions and cache accesses under Valgrind, so a reading does not move with the machine's load. Valgrind runs on
Linux, so it runs in the Linux container or CI.

Acceptance:

- The same operations as the Criterion stage, so a regression can be seen in both.
- A deliberate slowdown in a disposable copy shows in the counts; the counts of two unchanged runs are identical.
- Reported, not gated, until normal variance across toolchain updates is known.

## 8. Randomized concurrency testing

Use [shuttle](https://github.com/awslabs/shuttle) where Loom's exhaustive search does not reach a useful bound: watch
delivery with nested unwatch, close and drop, and token registration racing commits. Shuttle explores interleavings
at random, reproducibly from a seed, and scales to more threads and steps than Loom.

Acceptance:

- Adopted only after the Loom stage decides how production synchronization is abstracted for a model; both use the
  same abstraction rather than two.
- A mutation reproducing a race this branch fixed (the listener dropped outside the callback guard, or a write
  slipping between the unit check and the writer) fails within the configured iterations.

## 9. Library no-clobber publication

Evaluate tempfile's [`persist_noclobber`](https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html#method.persist_noclobber)
against `publish_new` and `move_file` in `store/backup.rs`. tempfile is already a dependency; its no-clobber persist
uses the platform's exclusive rename with a link fallback, and clears Windows' temporary attribute.

Acceptance:

- A comparison per platform of what each does: exclusive rename, fallback errors, attribute handling, write-through
  on Windows. Replace the hand-written path only where tempfile's guarantees are at least as strong.
- Directory syncs and the post-publication error that says the result is visible stay elmdb's; tempfile does
  neither.
- If tempfile lacks a needed guarantee (Windows write-through), record that and keep the code.

**Built, 2026-10-28:** on Unix, `publish_new` is `TempPath::persist_noclobber`, its unsafe `renameat2`/`renamex_np`
calls gone; elmdb still links on `EOPNOTSUPP`, which tempfile returns rather than handling. tempfile 3.27.0 moves on
Windows without `MOVEFILE_WRITE_THROUGH` and clears every attribute, so both Windows publications keep `move_file`; the
backup's replacing publish on Unix stays `std::fs::rename`, which is what tempfile's `persist` does.

## Delivery and validation

As development-libraries.md, Delivery and validation: each stage records version, license, toolchain compatibility,
features and dependency-tree impact; results and mutation receipts go in docs/evidence, linked from the change.
A tool that doesn't pay for itself gets a written decision and is removed.
