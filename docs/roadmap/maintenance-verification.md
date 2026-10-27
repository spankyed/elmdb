# Maintenance verification after remediation

Status: 2026-10-27. This is the current disposition; older dated review/evidence sections retain their historical
observations. The remediation PRs (#1 and apack #283) are merged. The macOS claim diagnostic correction is c345182;
apack's follow-up PR #284 pins that exact engine source. Diagnostic correction preserves ALREADY_LOADED and refusal.

## Completed and remaining work

| Item | Current disposition |
|---|---|
| Single-index value/text/vector rebuild intermediates | Streaming/spill implementation completed at c78c623; not an arbitrary-record/RSS bound |
| Dump export collections | Streaming/spill implementation completed at c78c623; callback may choose to collect |
| Filesystem notes | Coverage/support policy published; no additional filesystem certification implied |
| macOS occupied Mach name diagnostic | Corrected at c345182; owner ambiguity stated, fail-closed behavior retained |
| Criterion | Public key encoding, selected value/text rebuild, export and indexed limit benchmarks implemented |
| dhat | Dedicated-process export and selected value/text rebuild heap assertions implemented at two sizes |
| Private emission/sorter microbenchmarks | Not exposed through a production API; public rebuild cases cover their combined cost. Isolated microbenchmarks remain a follow-up |
| Heap coverage beyond these fixtures | Selected vectors, isolated full derivation, large records/postings and graph-builder observations remain |
| Full check/rebuild inventories and reports | Still workload-dependent; no constant-heap claim |
| Scratch operations | Placement configuration, quotas and safe reclamation after hard exit remain |
| Populated replacement | Broader profiles and isolated operation measurements remain; one historical preparation-inclusive observation is not a general allowance |
| NOT_DURABLE metadata | Precise touched-token payload remains an API decision; callers invalidate all |
| Filesystem validation | Deployment-specific tests, including any intended NFS/SMB/exFAT configuration, remain |
| Other developer tooling | Unimplemented stages in development-libraries.md and verification-tooling.md remain planned |

## Repeatable timing

```sh
cargo bench -p elmdb --bench maintenance -- --save-baseline before
# Run after the change, on the same host/toolchain and with comparable cache/load conditions:
cargo bench -p elmdb --bench maintenance -- --baseline before
# Build and exercise every case without collecting a timing baseline:
cargo bench -p elmdb --bench maintenance -- --test
```

Criterion uses 20 samples, one-second warmup and a three-second requested measurement window. Slow operations can
require longer windows. Fixture creation, database open and initial derivation are excluded. Selected rebuilds
repeatedly replace the same logical contents; this is steady-state maintenance, not first-build/crash-recovery timing.
Syncs are disabled. Export delivers and drops loads without serializing a dump file. Indexed queries return at most
50 IDs; corpus size is recorded in the benchmark name. Report input sizes, hardware, toolchain, commit and cache/load
conditions. Preserve target/criterion reports outside the worktree when comparing commits. No wall-time CI gate.

The deterministic fixture has eight 96-byte indexed tags per entity, and 16 distinct generated words per text
document. Benchmarks use 1k/10k entities; key encoding uses 16/128/1024-byte strings. Do not compare these fixtures'
absolute times with the older 100k/1024-byte-body recovery observations. Keep scripts/measure-recovery.mjs for RSS,
populated replacement, disk and scratch observations.

## Dedicated heap gate

```sh
cargo test -p elmdb --release --test maintenance_heap -- --ignored --exact maintenance_heap_bounds --nocapture
```

One ignored integration test owns one dhat allocator/profiler in its own executable. It creates each fixture before
profiling and measures export or rebuilding only. The ordinary workspace suite does not run the expensive gate;
Linux CI runs it explicitly. No normal engine/binding executable installs a profiler or changes allocator.

The fixtures use 1k/10k entities. Export additionally includes one 8 KiB relation payload per entity. Budgets are
24 MiB for export, 16 MiB for value rebuild and 32 MiB for text rebuilding,
with actual peak Rust heap bytes printed per case.
These are regression allowances for those inputs, not literal 8 MiB sorter guarantees or universal memory limits.
They exclude allocations already live before profiling, native allocations outside Rust's allocator, LMDB mappings,
RSS, fixture preparation and output retained by a caller. Correctness checks and indexed reads run outside profiling.

Criterion 0.8.2 (MIT/Apache-2.0; published MSRV 1.86) and dhat 0.3.3 (MIT/Apache-2.0) are development-only dependencies;
dhat declares no MSRV in its manifest; the pinned Rust 1.99.1 toolchain is used.
The lockfile adds 35 packages and replaces none. Production dependency edges exclude both tools.
Criterion default features are disabled; cargo_bench_support is enabled.
Dhat's allocator is linked only into this dedicated test binary. No second LMDB implementation is introduced.

Sources: [Criterion](https://docs.rs/criterion/0.8.2/criterion/),
[dhat testing and isolation](https://docs.rs/dhat/0.3.3/dhat/).

Evidence and negative proofs are recorded in [measurement tooling evidence](../evidence/maintenance-verification-2026-10-27.md).

## Next decisions

Disposable regressions and a native profile are now recorded in the linked evidence. Control between-run timing
variation before optimizing: the unchanged measured code varied substantially across the two observed timing runs.
Keep timing runs separate from heap profiling: allocation
backtraces materially distort CPU costs. Tooling improvements must coordinate with the agent owning goal-elmdb-next;
this work neither resumes that goal nor adopts its prospective feature changes.

## CPU profile before optimization

```sh
cargo build -p elmdb --release --example profile-maintenance
# Terminal 1: wait for PROFILE_READY (it prints the PID after fixture preparation):
target/release/examples/profile-maintenance 10000 30
# Terminal 2 on macOS, using that PID while rebuilds are running:
sample PID 10 1 -file /private/tmp/elmdb-text-rebuild.sample.txt
```

The example performs 30 selected text rebuilds on the same deterministic 10k fixture, with no dhat allocator or
Criterion sampling loop. The system sampler is a local macOS profiling method; it does not implement the separate
planned samply adoption. Sampling overhead makes this unsuitable as a timing comparison. It does not reconstruct
the older 100k measurement's exact inputs or prove the cause of its elapsed-time delta. Use the sampled call stacks
to choose a narrower experiment, preserve the spill/rollback contracts, and remeasure before accepting an optimization.

## Demonstrated negative proofs

```sh
cargo bench -p elmdb --bench maintenance -- --save-baseline remediation-20261027
python3 scripts/check-maintenance-verification.py --output /private/tmp/elmdb-verification-proof-UNIQUE
```

Use a new output directory and a full repository history. The script archives committed source plus current changes
into a disposable directory, restores historical export/value collectors, buffers text memberships, and injects a
one-millisecond delay into the timed key-encoding closure. Each heap mutation must fail its runtime heap assertion;
the timing mutation must complete and report a statistically detected regression. Compilation errors and zero tests
do not qualify. It reuses target for build/baseline caching: run serially, then rebuild/retest normal artifacts before
using them. Platform-specific negative proofs remain separate. No production worktree is mutated.
