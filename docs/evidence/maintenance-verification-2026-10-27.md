# Maintenance measurement tooling — 2026-10-27

This follows the merged remediation (elmdb #1, merge 8ae6572; apack #283, merge 516d9a1). The separate app pin
follow-up adopts exact c345182, including the final macOS occupied-Mach-name diagnostic. No storage behavior,
format, public protocol, error code or production allocator changes are made by the measurement tooling.

## Scope and conditions

Criterion 0.8.2 and dhat 0.3.3 are development dependencies. The lockfile adds 35 packages, replaces none, and normal
production dependency edges include neither tool. Commands, limits and remaining coverage are in
[maintenance verification](../roadmap/maintenance-verification.md). No private engine functions are exported merely
to benchmark them. Public selected rebuilds cover the combined derivation/sorting cost; isolated microbenchmarks
remain planned. Other library/tooling stages remain explicitly unimplemented.

Runs were serial on a MacBookPro18,1, 32 GiB RAM, 10 CPUs, arm64 macOS 26.3.1, Rust 1.99.1
(58cf8f9c3, LLVM 23.1.1), Node 23.11.1, local storage. Filesystem caches and other OS activity were uncontrolled.
The same deterministic fixtures are shared by the heap tests, benchmarks and profile example. Setup, initial
index derivation and database open are excluded. Index rebuilds repeatedly replace existing logical contents;
this is not a first-build or crash-recovery benchmark. Syncs are disabled.

## Dedicated heap assertions

A single test in its own executable owns dhat's allocator and one profiler at a time. It measures only Rust
allocations inside each operation. The 1k/10k fixtures have eight 96-byte tags per entity; export additionally has
one 8 KiB relation per entity. Text fixtures instead have 16 distinct generated words per document.

| Operation | 1k peak bytes | 10k peak bytes | Regression allowance |
|---|---:|---:|---:|
| Primary export | 102,227 | 1,250,387 | 24 MiB |
| Selected value rebuild | 1,579,401 | 9,660,787 | 16 MiB |
| Selected text rebuild | 5,721,396 | 22,506,941 | 32 MiB |

These are fixture-specific regression budgets, not a universal heap/RSS bound or a direct assertion that every
sorter allocation fits 8 MiB. They exclude preexisting live allocations, non-Rust native allocations, mapped LMDB
pages, fixture preparation and caller-retained output. Full-audit inventories/reports, selected vectors and graph
builder resources remain distinct coverage items. Exact first-50 indexed IDs, export counts and clean integrity
reports are checked outside measured windows where applicable.

## Negative proofs

`scripts/check-maintenance-verification.py` runs on disposable archived source with the current overlay, reusing the
local target cache serially. Actual runtime assertions must fail; compilation failure and zero tests do not qualify.

| Mutation | Observed result |
|---|---|
| Restore historical primary export collector from 1723d5b | 10k export peaks at 86,323,786 bytes, fails 24 MiB assertion |
| Restore historical selected value collector from 1723d5b | 10k value rebuild peaks at 18,359,648 bytes, fails 16 MiB assertion |
| Collect all sorted text memberships before emitting | 10k text rebuild peaks at 41,916,686 bytes, fails 32 MiB assertion |
| Add a 1 ms sleep to the timed key-encoding closure | Criterion reports a statistically detected regression (p < 0.05), about 0.263 microseconds to 1.262 milliseconds |

The initial 40 MiB text allowance let that collecting mutation pass at 41,916,686 bytes. It was tightened to 32 MiB,
roughly 49% above the good path's measured peak, and the mutation then failed. This correction is evidence of why
negative proofs are required. An initial archive invocation also exposed Python 3.9's lack of tar extraction filters;
the script now extracts only its own locally generated git archive without that newer API. Normal artifacts are
rebuilt after the disposable runs.

Receipts: `/private/tmp/elmdb-verification-mutations-20261027-v2/{export,value,text}.log` (the first text attempt),
`/private/tmp/elmdb-verification-mutations-text-final/text.log`, and
`/private/tmp/elmdb-verification-mutations-timing-final/timing.log`.

## Criterion baseline

Twenty samples per case, one-second warmup and three-second requested measurement window. Slow cases expand the
window to fit the samples; 10k text rebuilding took about 19 seconds for its 20 measured iterations. The initial
baseline's arithmetic mean estimates and 95% confidence intervals are below, rather than mixing Criterion's mean
and regression-slope estimates. These intervals quantify that run, not uncertainty across machines/cache states.

| Operation | Rows | Mean ms | 95% interval ms |
|---|---:|---:|---:|
| Value rebuild | 1k | 19.856 | 19.312–20.422 |
| Value rebuild | 10k | 218.433 | 212.830–224.771 |
| Text rebuild | 1k | 97.518 | 84.477–114.251 |
| Text rebuild | 10k | 914.693 | 909.946–919.217 |
| Primary export | 1k | 3.153 | 3.053–3.288 |
| Primary export | 10k | 25.381 | 24.352–26.701 |
| Indexed limit 50 | 1k | 0.539 | 0.521–0.570 |
| Indexed limit 50 | 10k | 6.105 | 5.578–6.750 |

Key encoding also covers 16/128/1024-byte strings. Export delivers/drops loads; it does not serialize or sync a dump
file. Query throughput is not mislabeled as scanning every corpus row: corpus size is in its name, with at most
50 returned IDs. The corpus-dependent query cost remains work for the existing planner goal; it is not changed here.
The untimed exact-ID fixture assertion was added after the initial baseline; fixture generation and timed closures
were unchanged. The final normal-source repeat includes it. No timing acceptance threshold or speedup claim is made.

The unchanged measured code's second normal-source run has 10k mean times of 449.659 ms (value rebuild),
1,732.405 ms (text rebuild), 30.891 ms (export) and 5.065 ms (indexed limit). Criterion labels some changes
significant despite no measured-code change. Host load was 5.03/6.07/6.61 when inspected during that run, on
10 CPUs; the responsible contribution of cache, storage, scheduling or other activity was not isolated.
This demonstrates substantial between-run variation, not a source regression or speedup. Baseline comparisons
need controlled repeated trials before they support optimization decisions; narrow per-run intervals alone do not
supply that control. Heap peaks remained identical across normal runs. There is deliberately no timing gate.

Raw baseline estimates/data and fixture/source metadata are retained under
`/private/tmp/elmdb-verification-evidence-20261027/`. Timing logs are
`/private/tmp/elmdb-verification-bench.log` and `-bench-repeat.log`.

## Native text profile before optimization

The release profile example prepares the 10k text fixture, reports PROFILE_READY, then performs 30 selected text
rebuilds. macOS sample attached to that process for ten seconds at a requested 1 ms interval, after preparation.
The loop completed in 30.272 seconds with sampling enabled. This elapsed figure is not a benchmark comparison.

The main thread has 8,132 samples. The flattened top-of-stack report includes write (990), memory movement (654),
close (536), allocator free (506), LMDB node deletion (364), byte comparison (355), and LMDB node search (334).
Call stacks place buffered writes under sorted::write_row/BufWriter::flush_buf, closes under Scratch::flush/finish
and merge, and comparisons/heap operations under the external sorter. Text analysis/document creation and LMDB
prefix clearing/replacement also appear. Symbols retain Rust v0 mangling, but the named engine paths are identifiable.
These are sampled stacks, including time in system calls, not precise CPU-cycle percentages or additive phase totals.

The evidence supports narrow future experiments with larger buffered I/O and row-buffer reuse, retaining the heap,
spill, duplicate, rollback and byte-compatibility checks. It does not justify removing spill sorting or attributing
the older 100k/1024-byte-body observation's exact 13% delta to one function. That historical fixture differs in text
shape and vocabulary; this run has no matched historical-code profile. No optimization is bundled with this tooling.

Profile: `/private/tmp/elmdb-verification-profile-20261027.txt`; launch/completion receipt:
`/private/tmp/elmdb-verification-profile.log`. The documented example/system-sampler method does not claim the
separate samply adoption stage is implemented.

## Local acceptance

Formatting, warning-denying workspace/all-target clippy, workspace tests (273 core engine passed, two manual
measurements ignored), CLI/binding/xtask tests, generated-contract validation, single-LMDB validation, rebuilt release
binding, TypeScript and all 37 Node tests pass. The ignored integration test is additionally run explicitly in
release mode and passes all six tightened heap windows with the byte peaks above. The exact CI benchmark smoke
command also exercises all eleven cases successfully. Normal artifacts were restored after the mutants.

Logs: `/private/tmp/elmdb-verification-{clippy,workspace,heap-final,bench-repeat,bench-smoke,generated,one-lmdb,node-build,types,node}.log`.
Production tree receipt: `/private/tmp/elmdb-verification-production-tree.txt`. Supported-platform CI and app strict
acceptance are recorded on the follow-up PRs for their exact final heads; local checks do not substitute for them.
