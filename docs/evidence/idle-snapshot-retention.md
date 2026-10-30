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
The matched binding comparisons are still pending.

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
