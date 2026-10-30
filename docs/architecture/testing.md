# Testing

What proves elmdb correct, and where each proof lives. `cargo nextest run --workspace` runs the Rust suites, each test in
its own process (`cargo test --workspace --doc` the doc tests); `npm test` runs the binding's. A test that hangs is
killed at its deadline in `.config/nextest.toml`: ten seconds for a watch test, where a regression deadlocks a
delivery, fifteen minutes for the model harness, which takes four to five on CI's Intel macOS runner, and four for any other;
the rest run on. A filter that selects no test fails the run. A test still runs the test binary again where it needs
a second process: the crash harnesses' killed writers, another process's commit, schema change and map growth seen by
this one, a read-only open beside another process's writer, and the process claim.

## The model harness

The acceptance suite is a model harness (`crates/elmdb/src/engine/tests/model.rs`), ported from apack's
(`@apackdev/ears`, `tests/_harness/`): random workloads over a small pool of ids, so writes collide, refer to each
other and are refused, checked after every operation against a plain reference model. The workload is a state machine
(proptest-state-machine): each step is chosen from what the model holds — an id it has, an edge it has, a key holding a
list — and one write in eight blind, so the engine is still checked to refuse what the model refuses. A failing case shrinks
to its shortest, dropping a chosen step whose target an earlier deletion removed rather than keeping it as a refusal,
and `generated_workloads_complete_every_kind_of_step_and_make_every_refusal` holds the generation to completing every
kind of step and making every refusal. The engine must hold what the model holds, refuse what it refuses, and answer
every derived read as the model does: a type's entities, count and trash, value index lookups and their first two, an
order by a field and its first one or three, roles, each edge's far ends, and a text index's matches for a few words. A
unit, a savepoint and a refused commit roll back with it. A workload also rebuilds every index or one now and then:
`check()` derives what it compares with the code a rebuild writes with, so after a rebuild only the model's answers can
show that code wrong. Ranking is checked separately, against fixed corpora (`engine/text/tests.rs`,
`engine/vector/tests.rs`).

It runs in two places:

- in Rust, against the engine directly;
- in apack, against the engine through the binding and `@apackdev/ears` (`tests/engine/model-based.spec.ts`), where it
  also checks a watch on every derived read, every row and the whole state against the read run afresh after every
  op; and during the move to elmdb it drove apack's JavaScript engine with the same workloads and compared the two
  (`docs/evidence/phase-9.md`).

## Where a test's files go

`.cargo/config.toml` points `TMPDIR`, and on Windows `TMP` and `TEMP`, at the checkout's `tmp/`, so every temporary
directory a test makes lands there, through cargo and nextest alike. A test killed at a deadline leaves its directory
behind, which `git clean -fdX tmp` removes; a mutation job's land in its own copy of the tree, which cargo-mutants
deletes. `tmp/` is committed, holding only its `.gitignore`, because the compiler and the linker read these variables
too: the directory has to exist before anything is built, in a fresh clone, a worktree building into another target
and a mutation copy alike. A unit test's database is refused growth past 1 GiB (`TEST_MAP_LIMIT`, `MAP_FULL`), which no test nears, so a
mutant that keeps one growing fails its test instead of filling the disk.

## Crashes and failed syncs

The crash harness (`engine/tests/model.rs`, `mod crash`): a child process, the test binary run again, applies a seeded
workload with syncs on and acknowledges each step; the parent kills it with SIGKILL mid-workload, reopens the files and
compares them with the model after the acknowledged steps, or one more. The child waits to be killed by reading its
stdin, which the parent holds, so it ends with the parent however the parent ends, rather than outliving it. Its
temporary directories are inside the parent's: a child killed in the middle of a rebuild leaves its scratch there, and
the parent's cleanup removes it. The
failed-sync harness (`store/sync/tests.rs`) runs random sequences of commits, `durable()` waits, failing syncs and clock
advances, and checks that a wait resolves only once a sync covering its commit succeeded and rejects when the sync it
waited on failed.

## Failpoints

A fault a test injects — a commit refused as on a full disk, a failed sync, a pause before a read takes its snapshot,
a panic in a watch's read — is a named `fail` failpoint (`crates/elmdb/src/failpoints.rs`, the names in its `points`).
`fail`'s registry is the process's and the suite's tests run in parallel, so a site's condition asks `failpoints`
whether the point is armed for it: once on the thread that armed it (`fail_once_here`, `run_once_here`), once for the
database at a path (`fail_once_at`), or for a path until a guard drops (`fail_while_at`), for a fault that fires on
another thread, a sync or a commit. A test arms a point only through these, never `fail::cfg`, which would arm it for
every test running at the time; `failpoints`'s own spec shows a point armed on one thread doesn't fire on another.

Only elmdb's tests enable `fail`'s `failpoints` feature (a dev-dependency feature, which resolver 2 keeps out of every
other build), and without it a site is nothing: a release binding holds no point's name and no `fail` symbol
(`docs/evidence/verification-tooling.md`).

## Mutation testing

cargo-mutants (`.cargo/mutants.toml`) mutates the engine's source — operators, conditions, a function's body — and runs
the engine's suite against each mutant; one the suite passes is a test missing. A run is scoped, never the whole crate:

```sh
# Pass 1, every mutant of the files named, against the suite without the model harness (about 25 s idle)
cargo mutants -j 3 -o target/mutants --file crates/elmdb/src/engine/trigram.rs -- -- --skip model::the_engine
# Pass 2, only those pass 1 missed, against the whole suite
cargo mutants -j 3 -o target/mutants --file crates/elmdb/src/engine/trigram.rs --iterate
# A branch's own changes, each crate against its own suite, survivors listed (what CI runs on a pull request)
python3 scripts/mutants-in-diff.py main
# What the exclusions leave out of a file
diff <(cargo mutants --list --no-config --file F) <(cargo mutants --list --file F)
# The binding's process claim, against elmdb-node's suite rather than the engine's
cargo mutants -j 3 -o target/mutants --file crates/elmdb-node/src/process.rs --test-package elmdb-node
# Some mutants of a file again, after the specs that should kill them: a regex over their names
cargo mutants -j 3 -o target/mutants --file F -F 'F:(LINE|LINE):' -- -- --skip model::the_engine
```

Code behind `cfg(target_os = "linux")` or `cfg(windows)` doesn't compile on a Mac, so its mutants show as missed there:
the Linux ones are read in a Linux container (`python3 scripts/linux.py -- sh -c 'cargo install --locked
cargo-mutants && cargo mutants …'`), and the Windows ones on a Windows machine, with cargo-mutants and the toolchain installed there
(`docs/evidence/mutation-baseline.md`, The Windows reading).

CI's `mutants` job runs `scripts/mutants-in-diff.py` on every pull request, against the base it merges into, on Linux
and on Windows, so a change to code only one of them compiles is mutated where it runs: each missed mutant is annotated
on its line and listed in the job's summary, and the job fails only when cargo-mutants does. A change to code for the
other platform shows its mutants as missed there, since the job compiles them to nothing.
`--in-diff` alone would test a binding's or the CLI's mutants against the engine's suite, which `test_package` names, so
the script runs each crate's files against that crate's suite.

Each job builds a copy of the tree under the system's temporary directory, about 1.3 GB on the `mutants` profile (the
test profile without debug info), so `-j` is set by the disk as much as the cores. `--iterate` reads what earlier runs
caught from the output directory's `previously_caught.txt`, so a pass is resumed, and narrowed, by running it again into
the same directory.

A survivor is killed by a new test, recorded as a finding, or excluded in the configuration with the reason it answers
exactly what the code does; no assertion is loosened for either. An exclusion names its mutants, not their line's: where
an operator repeats on a line, or a mutant beside it is caught, the pattern carries the line and the mutation. A line
that moves shows the excluded mutant as missed, and can hide a mutant of the same kind in the same function that moves
onto that line; the `--list --no-config` diff above shows what each exclusion hides, and is the check after an edit. The hand-made mutation scripts that remain
(`scripts/check-process-publication-mutations.py`, `check-maintenance-verification.py`) make changes cargo-mutants
doesn't: one expression swapped for another, and heap and timing regressions.

## Derived data

`check()` runs after every model-harness case, so an index that drifts from its primary data fails the case that
caused it. `engine/tests/derived.rs` corrupts derived keys directly and checks that `check()` reports them and
`rebuild()` repairs them; text and vector indexes are checked and rebuilt in their own specs (`engine/text/tests.rs`,
`engine/vector/tests.rs`).

## Tracked reads

`engine/tracking/tests.rs` holds the interval index to a brute-force scan of every read set, through random adds,
removals and written keys; `engine/tests/tracked.rs` checks that a commit names exactly the tokens it wrote into,
that a stopped scan is tracked as far as it went, the commit sequence, and `Database::watch`.

## The binding

`crates/elmdb-node/test/`: each operation through JavaScript, and fuzzing (`fuzz.test.mjs`): random bytes and random
msgpack as batches, reads and loads, flipped bytes, arguments of the wrong type, a closed database and an ended unit.
Each must apply or throw: an error carrying an elmdb code when the payload reached elmdb, and N-API's own for an
argument of the wrong JavaScript type; and `check()` stays clean. The TypeScript types are generated
from the Rust ones (`cargo xtask generate`), so a mismatch fails the typecheck before a test runs.

## Platforms

CI (`.github/workflows/ci.yml`): format, clippy and tests on every push, on Linux; on dispatch and pull requests, the
full matrix — macOS (arm64, x64), Linux (x64, arm64; glibc and musl) and Windows x64 — each job building the binding
and running every suite, the model and crash harnesses included, on it.

Filesystem support requirements and the limits of this platform coverage are in [filesystems.md](filesystems.md).

## Maintenance memory paths

Forced-small spill budgets exercise value/text/vector single-index rebuilding and dump ordering. Selected rebuilds
must match independently maintained derived bytes; a late unique conflict must roll back every earlier write, and
corrupt shared entity sequences must not collapse distinct holders before validation. Vector journal tests cross
multiple batches and index groups. A streaming-only read fixture refuses collecting range scans during export and
checks creation order and consumer cancellation. External-sort tests check online run compaction, last-insert-wins
across levels, oversized rows and intermediate-merge failure cleanup. These are intermediate-storage assertions, not
an absolute heap or RSS cap for LMDB or hannoy. Dated large-fixture observations and disposable regression failures
are recorded in [maintenance memory evidence](../evidence/maintenance-memory-2026-10-27.md).

## Repeatable maintenance measurements

`cargo bench -p elmdb --bench maintenance` measures deterministic key encoding, selected value/text rebuilds,
export and indexed limit queries. Criterion reports comparisons; timing is not a CI pass/fail budget.
`cargo test -p elmdb --release --test maintenance_heap -- --ignored --exact maintenance_heap_bounds --nocapture`
runs one isolated dhat test, asserting workload-specific heap allowances for export and selected value/text
rebuilds at 1k/10k entities, and how much a full rebuild, a full check and a selected vector rebuild grow per entity
between two sizes. Linux CI runs this dedicated gate and a benchmark smoke run. Normal executables retain
their allocator.

**Timing.** A comparison is two runs on one host and toolchain, with comparable cache and load:

```sh
cargo bench -p elmdb --bench maintenance -- --save-baseline before
# after the change:
cargo bench -p elmdb --bench maintenance -- --baseline before
# every case built and run once, no baseline:
cargo bench -p elmdb --bench maintenance -- --test
```

Criterion takes 20 samples, a one-second warmup and a three-second window. Fixture creation, the open and the first
derivation are outside the timing; a selected rebuild replaces the same contents again, so it is steady-state
maintenance, not a first build or a recovery. Syncs are off, an export drops its loads without writing a dump, and an
indexed query answers at most 50 ids. The fixture holds eight 96-byte indexed tags per entity and 16 distinct words per
text document, at 1k entities, or at the counts `ELMDB_BENCH_ROWS` lists (`ELMDB_BENCH_ROWS=1000,10000` to see how a
cost grows with the data), and key encoding uses 16, 128 and 1,024-byte strings. Report input sizes,
hardware, toolchain, commit and load with a result, and keep `target/criterion` outside the worktree when comparing
commits. Unchanged code has varied substantially between runs here, so a difference is checked against that variation
before it is acted on. `scripts/measure-recovery.mjs` stays the measure of RSS, populated replacement, disk and scratch,
which Criterion doesn't see.

**A Linux container from a Mac.** `scripts/linux.py` runs a command over a copy of the checkout in `rust:1-bookworm`,
with two CPUs and 6 GiB. Its builds stay in a Docker volume labelled with the checkout's path, so a second run builds
only what changed; the toolchain, Cargo's crates and installed tools are in volumes every checkout shares. A volume whose
checkout is gone is removed at the start of the next run, and a run refuses when Docker has less than 8 GB free. A
container started by hand with `docker run` and a named volume leaves that volume behind for good, which is why the
script is the way in.

**Instruction counts.** `benches/instructions.rs` counts the instructions of the same operations under Valgrind's
Callgrind, through [gungraun](https://github.com/gungraun/gungraun) (the renamed iai-callgrind): each run once, its
setup, fixture and answer check outside the count, the maintenance ones at 1k entities and the reads over the same
10k messages. A count doesn't move with the machine's load, so one run before a change and one after compare. Text
work is the exception: the tokenizer, charabia, looks words up in a hash map seeded randomly per process, so
`text_rebuild` moves by a small fraction of a percent between runs of one commit
(`../evidence/instruction-counts.md`). Valgrind runs on Linux; elsewhere the benchmark says so and exits. From a Mac it
runs in a Linux container, with Valgrind and the runner at the dependency's version installed:

```sh
python3 scripts/linux.py --apt valgrind --unconfined -- sh -c '
  cargo install --locked gungraun-runner --version 0.20.0 && cargo bench -p elmdb --bench instructions'
```

The runner turns off address randomization for each count, a call Docker's default seccomp profile refuses, hence
`--unconfined`. A run compares itself with the one before it in `target/gungraun`;
`-- --save-baseline=<name>` and `-- --baseline=<name>` keep and compare named ones. On every pull request CI's
`instructions` job runs `scripts/instruction-counts.py`: the base it merges into, then the change against it, each
operation's count and change written to the job's summary. A count is reported and never fails the job, until the
counts' movement across toolchain updates is known.

**Heap.** The heap gate's one ignored test owns a dhat allocator in its own executable, builds each fixture before
profiling and measures only the export, the rebuild or the check. Its allowances are 24 MiB for an export (with an
8 KiB relation payload per entity), 16 MiB for a value rebuild and 32 MiB for a text rebuild, each case printing its
peak: regression allowances for those inputs, not memory limits. Full derivation and a selected vector rebuild keep something per entity
by design, so they are held to growth instead: between 6,000 and 12,000 entities of 250-byte values (each entity's
entries about 2.4 KB, past every sorter's 8 MiB at both sizes), a full rebuild or check may grow 1,024 bytes an entity,
its inventory measuring about 300; between 1,000 and 2,000 vectors of 256 dimensions, a vector rebuild may grow 768,
measuring about 320 to 350, where one decoded vector is 1 KB. They leave out what was live before profiling, native allocations,
LMDB's mappings and RSS. Timing and heap profiling run apart, allocation backtraces distorting CPU time. Criterion 0.8.2
and dhat 0.3.3 are development dependencies only, Criterion without default features, and dhat's allocator is linked
into that one test binary.

**A CPU profile**, before optimizing a maintenance path:

```sh
cargo build -p elmdb --release --example profile-maintenance
target/release/examples/profile-maintenance 10000 30   # prints PROFILE_READY and its PID once the fixture is built
sample PID 10 1 -file /private/tmp/elmdb-text-rebuild.sample.txt   # macOS, while the rebuilds run
```

Thirty selected text rebuilds of one 10k fixture, with no allocator or sampling loop of its own. Sampling slows it, so
it chooses the next experiment rather than timing one.

**A profile that names Rust functions**, with samply, which records a process it starts without sudo on macOS and on
Linux, and the `profiling` profile, the release build with line tables:

```sh
cargo build -p elmdb --profile profiling --example profile-reads
samply record --save-only --unstable-presymbolicate -o reads.json.gz -- target/profiling/examples/profile-reads page 20000
samply load reads.json.gz   # the Firefox Profiler, in a browser
```

`profile-reads` repeats one of the read benchmark's reads (`field`, `page`, `page_projected`, `all_rows`) after its
fixture is built; `--unstable-presymbolicate` writes the symbols beside the profile, so it can be read without the
binary. `read-allocations` counts each read's allocations with dhat. Readings: `docs/evidence/read-allocation-codec.md`.

**The gates shown failing.** `scripts/check-maintenance-verification.py --output <new directory>` archives the source
with current changes into a disposable directory, restores the export and value collectors that held whole collections,
buffers text memberships, and slows the timed key encoding by a millisecond: each heap mutation must fail its
assertion and the slowdown must be reported as a regression. Run it serially, in a full history, after
`cargo bench -p elmdb --bench maintenance -- --save-baseline <name>`; it reuses `target`, so rebuild afterwards. The
evidence is [measurement tooling evidence](../evidence/maintenance-verification-2026-10-27.md).
