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
