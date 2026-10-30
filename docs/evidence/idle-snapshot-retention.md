# Idle snapshot retention

## Baseline: 2026-10-30

Release build on Linux aarch64 (OrbStack), Rust 1.99.1, two CPUs, 6 GiB RAM, no additional
swap. Source: main `717e903` plus the measurement-only checkpoint containing this document;
[raw results](idle-snapshot-retention/baseline-release.json) include exact source-file and binary hashes.

```sh
python3 scripts/linux.py -- sh -euc 'export CARGO_INCREMENTAL=0; python3 scripts/measure-idle-readers.py --source 717e903779643fb2592f70f790f24b42f2495b44 --output /w/target/idle-reader-baseline-release'
```

The writer overwrites 128 values of 8 KiB in one transaction, 32 times. One or four other processes
read once, remain alive and perform no further database operations. Writes begin after 200 ms idle.
An otherwise identical control explicitly releases its cached transaction after that first read.
Syncs are disabled and all databases are disposable.

| Reader policy | Allocated pages after 32 overwrites | File bytes | Live primary-tree pages |
|---|---:|---:|---:|
| Cached (one or four processes) | 12,808 | 52,461,568 | 385 |
| Explicit release (one or four processes) | 1,167 | 4,780,032 | 385 |

Both start at 393 pages / 1,609,728 bytes. The cached reader remains at transaction 3 as the
writer reaches transaction 35; free-list entries accumulate but cannot yet be reused. The raw
`mdb_stat` output records reader slots and free-list pages, using the exact Cargo-resolved LMDB
source in a separate executable. Reader counts in environment info are slot high-water counts,
not counts of currently active snapshots. After readers close, 16 further overwrites require no
additional pages: reuse resumes, rather than shrinking the existing file. Map capacity stays 64 MiB.

ABBA runs of 100,000 raw Store point reads: cached 54.6 and 48.0 ns/read; explicit release
180.4 and 189.6 ns/read. The control includes an extra cache mutex acquisition. These are synthetic
raw borrowed-value reads, not application latency or a prediction of the final implementation.
The evidence warrants bounded idle release while preserving useful burst reuse.

Validation of this measurement checkpoint: workspace clippy with all targets and warnings denied,
and formatting passed. Production behavior is unchanged. Candidate policy, semantic regressions,
mutation proof, matched binding measurements and full landing gates remain to be completed.

## Bounded-cache candidate: 2026-10-30

Under the same release measurement conditions, completed snapshots expire 100 ms after their read
returns. One and four read-once processes now match the explicit-release control at every sampled
frontier: 1,167 allocated pages / 4,780,032 file bytes after warmup, with 385 live primary-tree pages.
[Raw candidate results](idle-snapshot-retention/candidate-release.json) include source and binary hashes;
the report transparently corrects a mistyped caller-supplied parent label. This measured snapshot
precedes additional lifetime tests; its runtime implementation is the candidate being validated.

Cached raw reads measured 99.4 and 101.6 ns/read, about 50 ns above the baseline and below the
predeclared 100 ns additional-cost budget. Explicit release measured 199.9 and 204.0 ns/read, including
its extra lock. These separate runs establish a useful scale, not precise application latency.
Matched binding comparisons are recorded below.

The 100 ms window preserves reuse within typical 16 ms view bursts and matches the existing default
sync idle interval. It is an internal policy, with no public API, dependency or file-format change.
Pinned heed exposes destruction but no supported read reset/renew API. The candidate adds one worker
thread per open database, two clock observations on a cached read and fixed per-entry metadata for
at most eight entries. The worker sleeps when empty, does not own the database or map gate, and
releases only completed transactions. Operating-system scheduling may delay expiry; active reads
and open units intentionally retain their snapshots for their existing lifetimes.

A disposable mutation renews expired entries indefinitely instead of releasing them. The idle-child
regression failed in 3.41 seconds with `completed snapshots were not released without another read`,
then its parent failed its barrier; compilation succeeded and the selected test executed. The normal
implementation passes. Run `python3 scripts/linux.py -- python3 scripts/check-idle-reader-mutation.py`
to repeat the proof. The working source was never mutated.

## Matched binding and allocations: 2026-10-30

Linux aarch64 on OrbStack, two CPUs, 6 GiB RAM, no additional swap, Node 24.10.0. Build the
baseline and candidate binding in release mode, preserve each generated binary, then run the
existing `crates/elmdb-node/bench/reads.mjs --json` in eight fresh processes, ABBAABBA.
Each process checks answers and measures 2,000 reads per small workload and 30 full-row reads.
The table is the median of the four process medians per version; these are observations, not CI gates.

| Workload | Baseline | Bounded cache | Change |
|---|---:|---:|---:|
| Field | 2.209 µs | 2.292 µs | +3.8% |
| Page | 187.834 µs | 185.334 µs | −1.3% |
| Projected page | 53.230 µs | 54.042 µs | +1.5% |
| Full rows | 37.205 ms | 36.082 ms | −3.0% |

All are within the predeclared 10% budget. The small apparent improvements are not evidence of
an application speedup. [Per-process summaries](idle-snapshot-retention/binding-comparison.json)
record binary/answer hashes, runtime, fixture, raw-report hashes and source identities. Both binaries
were independently inspected: the candidate contains the expiry worker and the baseline does not;
each loaded binary's hash matches its report. Answer hashes match across all eight processes.
Raw reports are retained in `/private/tmp/elmdb-idle-reader-evidence/idle-binding-comparison` on
the measurement host; the committed summaries are sufficient to inspect the reported medians but
do not contain every timing sample.

Reproduce using the baseline/candidate public checkpoints in that summary, in isolated checkouts:
run `npm ci && npm run build`, preserve each binding directory and its benchmark, then execute
`node crates/elmdb-node/bench/reads.mjs --json` in ABBAABBA order. In a copied Linux snapshot
without `.git`, supply `ELMDB_PROFILE_SOURCE_COMMIT` and `ELMDB_PROFILE_SOURCE_DIRTY` together
as documented by the benchmark. Keep the same fixture and default iteration counts. A fresh
process does not imply cold filesystem caches. These workloads represent the existing read fixture,
not a trace of apack actions or a deployment-wide performance guarantee.

[Heap comparison](idle-snapshot-retention/allocation-comparison.json): the established DHAT example
reports identical allocation counts, allocated bytes and peak heap for all four reads. The source-matched
debug executables were run directly by distinct hashed filenames and checked for worker identity,
avoiding ambiguity in Cargo's shared convenience executable path. Fixture/warmup and preexisting,
native and mapped memory are excluded. This confirms no additional measured per-read allocations;
it does not measure the additional worker's stack or operating-system resources.

The original measurement commits inherited an unrelated unpublished graph-planning document from
local main. Rebase excluded that parent from the PR. Baseline checkpoint `cecc01b` and candidate
`59040f2` preserve the measured runtime/fixture source; the summary maps original and public commits.
No other agent's checkout was edited.

Local validation: all required Rust gates passed (369 workspace tests, one ignored doc example);
the six final lifetime regressions also passed after the last test additions. Binding build, strict
typecheck and tests passed (34 passed, four platform-specific skips). Generated loader/declarations
match committed source. Full platform CI is tracked on PR #32; local Linux evidence does not certify
other operating systems or filesystems.

## Mutation review: 2026-10-30

The first Linux CI mutation pass tested 43 mutations: 17 caught, 14 unviable, 11 missed and one
timeout. Inspecting survivors exposed coverage gaps even though functional CI was green: a cache
could stop reusing hot transactions, or a worker could outlive explicit close/Drop, while the
existing database-answer and reopen tests still passed. Runtime code was already correct.

Two focused tests now require actual hot-cache reuse within the retention window and complete
worker-state release on both explicit close and Drop. Targeted disposable mutations of reuse,
close and Drop each compiled and failed their intended assertion; the original source passes.
[Proof summaries and raw-log hashes](idle-snapshot-retention/mutation-proofs.json) record each case.
Reproduce through `scripts/linux.py -- python3 scripts/check-idle-reader-mutation.py --case CASE`,
where CASE is `reuse`, `close` or `drop`. The default remains the cross-process expiry proof.
All idle-cache tests have a 20-second nextest deadline. Remaining timing-comparison survivors must
be interpreted as boundary or performance-policy mutations, not as proof of a retained active snapshot;
the independent expiry and page-reuse regressions are the correctness evidence. CI will repeat on
the final test checkpoint. The added code is test-only; the measured runtime source is unchanged.

During the post-mutation check, reusing the normal Cargo target exposed executable-cache
contamination: Cargo reused the disposable hot-cache mutant's executable for the unmodified source.
The source was untouched. The mutation driver now uses `target/idle-reader-mutations`, separate
from normal validation. The affected normal package outputs were removed before repeating the
proofs and unmodified-source checks. This also explains why the allocation comparison above runs
verified hashed executables directly instead of trusting a shared convenience path.

After isolation, all four proofs passed and the freshly rebuilt unmodified workspace passed all
374 tests, clippy, formatting, doc tests, generated-contract and one-LMDB checks on the same bounded
Linux host. This is the final test checkpoint's local Rust validation; the earlier counts describe
the earlier runtime checkpoint.
Binding build/typecheck/tests were repeated successfully (34 passed, four platform-specific skips),
and generated loader/declarations match the committed files. The runtime portion of `idle_reads.rs`
and the remaining runtime files match `59040f2`; only test/harness/documentation code changed.
