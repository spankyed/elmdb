# Testing

What proves elmdb v2 correct today, and where each proof lives.

`cargo nextest run --workspace` runs the Rust suites, each test in its own process (`cargo test --workspace --doc` runs
doc tests); `npm test` runs the N-API binding's suite (`crates/elmdb-node/test/`), then `@elmdb/ears`'s vitest suites
(`packages/ears/tests/`). A test that hangs is killed at its deadline in `.config/nextest.toml`.

## Workspace architecture & static gates

Every pure-Rust library crate (`elmdb`, `elmdb-storage`, `elmdb-sim`, `elmdb-wire`, `elmdb-engine`, `elmdb-projections`,
`elmdb-vec`, `elmdb-query`, `elmdb-invalidation`, `elmdb-cli`) enforces `#![forbid(unsafe_code)]`; all LMDB C FFI is
isolated inside `crates/elmdb-lmdb`, and N-API glue inside `crates/elmdb-node`.

Repository-wide static checks run in `ci.yml`'s `repo` job and `cargo xtask pre-push`:

- **`cargo xtask check-sync`**: verifies every lock, condition variable, and thread-local in the engine crates goes
  through `elmdb-storage::locks` (except the two files explicitly justified in `xtask/src/main.rs`'s `UNMODELLED`:
  `elmdb/src/diagnostics.rs` and `elmdb-lmdb/src/env.rs`).
- **`cargo xtask check-commits`**: verifies every raw LMDB write-transaction commit (`commit_raw()`) is confined to the
  single durability choke point (`crates/elmdb-storage/src/durability.rs`).
- **`cargo xtask check-one-lmdb`**: verifies the workspace builds exactly one `lmdb-master-sys` C library.
- **`cargo xtask check-exact`**: verifies exact-pinned tokenizer/collator/graph dependency versions match `Cargo.lock`.
- **`cargo xtask check-diagnostics-off`**: verifies default workspace builds do not pull `tracing` into `elmdb`.
- **`cargo xtask check-failpoints-off`**: verifies default workspace and `elmdb-node` builds never enable the opt-in
  `failpoints` feature via Cargo feature unification.
- **`cargo xtask check-leiden-serial`**: verifies `leiden-rs` stays single-threaded and deterministic.
- **`cargo xtask check-workspace-deps`**: verifies every `[workspace.dependencies]` entry is referenced by at least one
  workspace member.
- **`cargo xtask check-generated`, `check-versions`, `check-notices`**: verify TypeScript/JSON-Schema generated protocol
  artifacts, npm/Cargo version parity across `@elmdb/*` packages, and third-party license notices (`cargo-about`).
- **`cargo deny check bans sources`** (and `advisories` when `Cargo.toml`/`Cargo.lock`/`deny.toml` changes): verifies
  crates.io-only sources and RustSec advisories across all seven shipping targets (`deny.toml`).

## Deterministic simulation (DST) & end-to-end suites

The core engine, storage durability, projections, vector/rank indexes, query planner, and live-query invalidation are
generic over `KvStore`, `Vfs`, and `Clock` (`crates/elmdb-storage`), so the exact same production code runs against both
real LMDB (`crates/elmdb-lmdb`) and the deterministic pure-Rust simulator (`crates/elmdb-sim`):

- `crates/elmdb-sim/tests/dst_durability.rs`: crash/reboot recovery, dual-slot 4 KiB `synced.v2` torn-page replay,
  Grouped-mode durable-pin freelist-reuse protection, `MapResized` adoption, and fail-stop poisoning on sync failures.
- `crates/elmdb-lmdb/tests/differential.rs`: real `LmdbStore` MVCC snapshot isolation, savepoints, `MDB_INTEGERKEY`,
  O(1) sub-DB truncation, durable pin invariants, and cross-thread concurrency.
- `crates/elmdb-lmdb/tests/sim_differential.rs`: differential fidelity proofs comparing `SimKvStore`+`SimVfs` directly
  against real `LmdbStore`+`OsVfs` on MVCC snapshot visibility, freelist page reuse (pinned vs. unpinned), cross-process
  `MAP_RESIZED` adoption, and `RecoveryStatus` transitions.
- `crates/elmdb-engine/tests/oracle_dst.rs`: randomized CRUD, links, nested savepoints, `beforeCommit` hooks, schema
  evolution, O(1) renames, archive/purge, and `check()` against a reference oracle on `SimKvStore`.
- `crates/elmdb/tests/oracle_state_machine.rs`: shrinking `proptest-state-machine` oracle running on real `Database`
  (`LmdbStore` + `OsVfs`) verifying exact state, exact refusal `ErrorCode`s, nested savepoints, host `beforeCommit`
  cascades, O(1) `rename_field`, archive + `purge_archived`, `dump`/`load` round-trips, `check(None)`, and all five
  deterministic indexes (`Value`, `Unique`, `Order`, `Text`, `Trigram`) with a step-coverage guard; plus an
  approximate-index oracle proving `Vector` (HNSW SQ8), `Rank` (PageRank), `GraphPpr`, and `GraphLeiden` satisfy
  `derived == rebuild(primary)` within strict tolerances and HNSW recall@5 `>= 0.85`.
- `crates/elmdb/tests/multiprocess_stress.rs`: real child-process stress tests exercising Grouped cross-process
  pin-txnid races, concurrent `MAP_RESIZED` adoption across multiple reader and writer processes (`256 KiB -> 4+ MiB`),
  same-process `DatabaseLocked` collision vs. cross-process isolation, hard-killed child lock/reader-slot recovery, and
  cross-process `LiveQueryHub` watch catch-up across creates, patches, destroys, and schema bumps.
- `crates/elmdb/tests/failpoints.rs` (`--features elmdb/failpoints`): zero-cost thread-scoped and path-scoped failpoint
  hooks (`crates/elmdb-storage/src/failpoints.rs`, compiled out in default/release builds) testing real OS/LMDB/durability
  seams (`mdb_txn_commit`, `mdb_env_sync`, `mdb_env_set_mapsize`, sidecar write/fsync/rename, `fs_publish`,
  `close_inner`, and `SKIP_GROUPED_DURABLE_PIN`).
- `crates/elmdb-projections/tests/projections_dst.rs`: Value, Unique, Order (ICU collator), Text (BM25F + highlights),
  and Trigram projections, deferred watermarks, rebuilds, and corruption detection.
- `crates/elmdb-vec/tests/vector_and_rank_dst.rs`: SQ8 HNSW vector index and incremental PageRank (`Rank`) rebuilds,
  drains, and invariants.
- `crates/elmdb-query/tests/query_dst.rs`: planner/executor equivalence across scans, filters, joins, sorting,
  aggregations, graph traversals, Leiden communities, and `ExactSum` float summation.
- `crates/elmdb-invalidation/tests/invalidation_dst.rs`: zero-false-negative differential invalidation across 10 watch
  plan shapes, `CommitRing` catch-up, 3-clock fencing, and sans-I/O `WatchScheduler`.
- `crates/elmdb/tests/v2_e2e.rs` & `crates/elmdb-cli/tests/cli.rs`: real-LMDB `Database` facade, streaming `dump`/`load`,
  `backup_into`, hooks, watches, and CLI end-to-end suites.

## Concurrency models (Loom & Shuttle)

Concurrency models execute production code on deterministic schedulers instead of OS threads:

- **Loom** explores every interleaving up to `LOOM_MAX_PREEMPTIONS` (3 in PR CI, 4 in `models.yml`).
- **Shuttle** runs `ELMDB_SHUTTLE_ITERATIONS` schedules (default 100,000 in PR CI, 10,000,000 in `models.yml`), split
  equally across uniform random schedules, PCT depth 2, and PCT depth 3, printing any failing schedule for
  `shuttle::replay`.

All modelable synchronization primitives live in `crates/elmdb-storage/src/locks.rs` (`elmdb_storage::locks::sync::*`
and the L1–L5 ordered locks `SchemaEpochLock`, `MapGate`, `WriterLock`, `DbiPublishGate`, `MetaLock`, `MetaRwLock`).
In default/release builds these compile directly to `parking_lot` and `std::sync` with zero overhead; under `--cfg loom`
or `--cfg shuttle`, Loom/Shuttle replace the underlying mutexes, rwlocks, condvars, atomics, and thread-locals (with
L1–L5 lock-order tracking active even in `--release` model builds).

Four production concurrency models run in CI (`ci.yml` `loom` and `shuttle` jobs) and deep runs (`models.yml`):

1. **Lock hierarchy & barriers (`crates/elmdb-storage/src/locks_model.rs` — Loom + Shuttle):** L1–L5 ordering, `MapGate`
   resize exclusion against `read_arc()`/writers, `DbiPublishGate` dual-lane `read_copy`/`read_txn`/`write` exclusion,
   and cross-database `LockScopeId` isolation.
2. **Durability gate (`crates/elmdb-sim/src/durability_model.rs` — Shuttle):** real `DurabilityGate<SimKvStore, SimVfs>`
   under concurrent Grouped writes, `sync()`, `grow_map()`, `unacked_reboot` + `acknowledge_restore()`, fail-stop
   poisoning, and `MapResized`/peer-commit pin retries.
3. **Database close & drain lifecycle (`crates/elmdb/src/lifecycle.rs` & `db_lifecycle_model.rs` — Loom + Shuttle):**
   real `DbLifecycle<T>` `enter_op()`/`LifecycleGuard`/`close()` `SeqCst` + `Condvar` drain handshake guaranteeing
   `Arc::strong_count == 1` before handle teardown, concurrent `close()` waiter outcome sharing, zero-timeout &
   bounded-yield behavior, and `MarkCloseDone` panic safety.
4. **Live-query hub & scheduler (`crates/elmdb-invalidation/src/model.rs` — Shuttle):** real `LiveQueryHub` +
   `Engine<SimKvStore, SimVfs, SimClock>` + `QueryEngine` verifying zero-false-negative concurrent registration/commit,
   overlapping `rerun_query_watch` `run_id` + 3-clock fencing, and race-free `stop_watches()` / `unregister_watch()`.

```sh
# Fast PR-tier Loom runs
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 CARGO_TARGET_DIR=target/loom \
  cargo test -p elmdb-storage --release --lib locks::model
RUSTFLAGS="--cfg loom" LOOM_MAX_PREEMPTIONS=3 CARGO_TARGET_DIR=target/loom \
  cargo test -p elmdb --release --lib lifecycle::model

# Fast PR-tier Shuttle runs (100,000 schedules per case by default)
RUSTFLAGS="--cfg shuttle" CARGO_TARGET_DIR=target/shuttle \
  cargo test -p elmdb-storage --release --lib locks::model
RUSTFLAGS="--cfg shuttle" CARGO_TARGET_DIR=target/shuttle \
  cargo test -p elmdb-sim --release --lib durability_model
RUSTFLAGS="--cfg shuttle" CARGO_TARGET_DIR=target/shuttle \
  cargo test -p elmdb --release --lib lifecycle::model
RUSTFLAGS="--cfg shuttle" CARGO_TARGET_DIR=target/shuttle \
  cargo test -p elmdb-invalidation --release --lib model
```

Each uses its own `CARGO_TARGET_DIR` (`target/loom`, `target/shuttle`) so model builds never invalidate the default
workspace target directory.

## Power loss (LazyFS), read-only mounts & negative controls

- **`.github/workflows/lossy-filesystem.yml`** mounts LazyFS on Linux and runs `crates/elmdb/tests/power_loss.rs`
  (compiled with `--features failpoints`), covering:
  - **`clear-cache`** (`a_filesystem_that_drops_what_was_not_synced_...`): drops unsynced writes and proves recovery to
    the last `synced.v2` checkpoint (plus an `ELMDB_LOSSY_UNSYNCED=1` negative control that must fail).
  - **`torn-seq`** (`a_filesystem_that_persists_unsynced_data_without_meta_...`): persists ONLY unsynced `D+3`
    data-page writes while crashing LazyFS before any unsynced meta-page write lands, proving the Grouped `MDB_RDONLY`
    durable pin keeps snapshot `D` intact — paired with an `ELMDB_LOSSY_UNPINNED=1` negative control that stays in
    `DurabilityMode::Grouped` with `db.flush()` and disables ONLY the durable pin via `SKIP_GROUPED_DURABLE_PIN` (must
    fail at DB open/check/model verification).
  - **`torn-op`** (`a_filesystem_that_tears_a_write_...`): splits a write to `data.mdb` into 3 parts (`persist=[1,3]`)
    and crashes LazyFS mid-write.
- **`scripts/check-read-only-mount.py`** (`crates/elmdb/tests/read_only_mount.rs`, run in `ci.yml`'s `repo` job) mounts
  a real Linux read-only bind mount (`sudo -n unshare --mount --propagation private`) proving v2 `read_only: true` opens
  succeed without creating `synced.v2.lock` or `lock.mdb`, block writes with `ErrorCode::ReadOnly`, and fail loudly on a
  disposable-copy mutation that attempts writable sidecar locking.
- **`scripts/check-negative-controls.py`** (run in `ci.yml`'s `repo` job) applies disposable-copy mutations (such as
  removing `install_durable_pin` in `crates/elmdb-storage/src/durability.rs`) and proves core safety assertions fail.

## Sanitizers (Miri, ASan, UBSan)

`.github/workflows/sanitizers.yml` runs weekly and on dispatch:
- Nightly **Miri** on targeted pure-Rust unit tests (wire codecs, `ExactSum`, interval/read-set tracking, graph
  algorithms).
- Nightly Linux **ASan + UBSan** (`CC=clang`, `-fno-sanitize=alignment` for LMDB's 2-byte-aligned `EVEN(n)` pointers)
  across Rust and the bundled C libraries (`lmdb-master-sys`, `zstd-sys`).

## Mutation testing

- **Per-PR diff gate (`.github/workflows/mutants.yml`, `scripts/mutants-in-diff.py`):** mutates changed Rust files
  against their crate's test suite (`cargo-mutants` + `cargo-nextest`), sharded across 4 Linux runners plus a dedicated
  Windows job for `#[cfg(windows)]`-only code. Exclusions live in `.cargo/mutants.toml` (file-wide, never line-numbered)
  or inline `// mutant-skip: PATTERN` comments checked by `cargo xtask check-mutant-exclusions`.
- **Weekly concurrency-model mutants (`.github/workflows/model-mutants.yml`, `scripts/mutants-models.py`):** mutates
  each file guarded by the four Shuttle models (`locks`, `durability`, `lifecycle`, `invalidation`) under `--cfg shuttle`
  and tests each mutant against its Shuttle model alone (200,000 schedules, re-tested at 2,000,000 schedules if missed).

```sh
# Test diff mutants against a base ref
python3 scripts/mutants-in-diff.py origin/main

# Test Shuttle model mutants for one model
python3 scripts/mutants-models.py durability
```

## Binding (`elmdb-node`) & `@elmdb/ears`

- **`crates/elmdb-node/test/`**: N-API binding integration tests (including wire fuzzing, panic catching via
  `ELMDB_TEST_PANIC`, process-claim isolation, process-exit cleanup, off-thread promises, BigInt/number boundaries, and
  surrogate rejection). On Linux, `scripts/check-process-claim-linux.py` and
  `scripts/check-process-publication-mutations.py` verify process claims under minimal container environments and
  negative-control mutations.
- **`packages/ears/tests/`**: vitest fast specs (including a TypeScript reference-model property suite and watch pump
  specs) plus integration specs (`vitest.integration.config.ts`) packing `@elmdb/ears` into a clean consumer with no
  `@types/node` on TypeScript 5.7+. `npm run api:check -w @elmdb/ears` locks public declarations via API Extractor.

## Build directories & temporary files

- `.cargo/config.toml` points `TMPDIR` / `TMP` / `TEMP` at the checkout's `tmp/` directory.
- `cargo xtask sweep` (invoked by `cargo xtask pre-push`) bounds `target/`, `target/loom`, `target/shuttle`, and the
  scratch target (`cargo xtask scratch-target`); `cargo xtask pre-push` refuses local builds below 20 GB free disk space.
- `cargo xtask compare` alternates commands across two git revisions in isolated worktrees under `target/compare/`.

## Platforms & releases

- **`ci.yml`** runs Linux checks (`checks`, `repo`, `loom`, `shuttle`), fast-tier platforms (`macos-15` arm64,
  `windows-latest` x64 via `platforms.yml`), and `@elmdb/ears` across Linux/macOS/Windows; `ci-pass` gates merges.
- **`full.yml`** runs the complete platform matrix (Linux glibc/musl x64 & arm64, macOS arm64 & x64, Windows x64)
  testing each built binary across Node 22.17.0, Node 24, and Electron 37.2.4 (`scripts/runtime-binding.cjs`);
  `full-pass` gates ready-for-review PRs.
- **`release.yml`** builds manylinux_2_28 glibc, Alpine musl, macOS, and static-CRT Windows platform packages, generates
  CycloneDX SBOMs, and runs `scripts/release-smoke.mjs` on every target.
- **`advisories.yml`**, **`lmdb-upstream.yml`**, and **`cla.yml`** enforce daily RustSec/npm advisories, upstream LMDB
  review tracking, and CLA verification.
