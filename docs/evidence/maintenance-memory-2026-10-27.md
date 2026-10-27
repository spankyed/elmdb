# Single-index rebuild and dump export memory — 2026-10-27

Implementation checkpoint: `c78c623902a733d6fe91baea8adfd8898c583eb7`. Baseline: freshly built CLI and Node binding
from exact engine `1723d5b911dae92a73bdc045cb4a408f45fa97f2`. No database format, dump version, public protocol or
application behavior changes. Filesystem coverage is stated separately in [filesystems.md](../architecture/filesystems.md).

## What changed and what is bounded

- Export scans primary ranges without collecting them. An external sort holds entity/relation identifiers, using
  original u64 creation order; the complete payload is read one row at a time from the same snapshot. Embeddings and
  sequences use fallible visits. The callback API does not collect output unless its caller chooses to.
- Selected value-index rebuilds stream primary entities into a spill sort. Unique values are grouped with holder
  identity, retaining only adjacent-group validation state. Distinct holders are not collapsed even when damaged
  primary records share a sequence. A late conflict aborts all earlier derived writes.
- Text rebuilds spill term/document memberships and finish one posting bitmap at a time. There is no corpus-wide
  map of all term/word bitmaps or complete in-memory derived-entry collection. The same implementation serves full
  derivation and selected text rebuilding.
- Selected vector rebuilds spill identifiers. Journal flushing drains at most 1,000 rows per batch, writes one added
  vector at a time and builds once per affected index, including groups crossing batch boundaries. Clearing vector
  ranges uses bounded key batches.
- The common sorter compacts equal-level runs while receiving rows. It no longer retains one pathname per spill;
  at most 15 run paths remain per level. Merge fan-in remains 16 and last-emitted-value semantics survive compaction.

An 8 MiB threshold applies to each sorter's payload-capacity and row-metadata accounting, plus row-buffer spare
capacity, the largest row, fixed merge heads and I/O buffers. Text rebuilding can use two sorters at once. A complete
decoded entity and its index expansion, a final posting value, hannoy's graph builder and LMDB dirty/resident pages
are separate resource costs. Whole check/rebuild still have primary inventories/audits/reports. This work removes
dataset-sized intermediate collections from the two requested paths; it does not establish a constant process RSS,
a universal per-record limit, a scratch quota or crash-leftover reclamation.

## Correctness and demonstrated failures

Local required gates pass: formatting, warning-denying workspace clippy, 273 engine tests (two manual measurements
ignored on macOS), CLI/binding/xtask suites, generated protocol, single-LMDB validation, rebuilt release binding,
TypeScript and all 37 Node tests. The release CLI was also rebuilt. Logs use `/private/tmp/elmdb-memory-`:
`clippy.log`, `workspace.log`, `generated.log`, `one-lmdb.log`, `cli-build.log`, `node-build.log`, `types.log`, `node.log`.

New deterministic coverage includes forced-small spill budgets for all selected index kinds, exact comparison with
independently maintained derived bytes, late-conflict rollback, corrupt shared sequences, export creation ordering
and cancellation through a read fixture that refuses collecting scans, more than one journal batch/index group,
online run-metadata limits, and intermediate-merge cleanup. Existing dump/load round trips remain byte-identical.

Seven disposable regressions actually fail their selected runtime assertions, not compilation or zero-test selection:

| Mutation | Failing assertion |
|---|---|
| Restore the previous primary export collector | Streaming-only read refuses collecting a primary range |
| Restore the previous selected-index collector | Scratch failure cannot be silently bypassed |
| Restore the previous selected-vector identifier collector | Same refusal for selected vector rebuilding |
| Remove online run compaction | Live run-path metadata exceeds the test allowance |
| Remove online compaction's intermediate merge | Injected merge failure is no longer observed |
| Stop vector flushing after its first batch | Commit leaves journal rows behind |
| Collapse unique holder identities in staging | A duplicate with a shared primary sequence is incorrectly accepted |

Receipts: `/private/tmp/elmdb-memory-mutations.log` and `elmdb-memory-mutant-*.log`. Production worktrees were not
mutated to obtain these failures. Another session's concurrent documentation commit was preserved at its original
hash; memory implementation moved to a separate worktree and build/dependency copies used copy-on-write.

## Resource observations

Measured serially on the same 32 GiB macOS arm64 workstation, pinned Rust release builds and Node 23.11.1, fresh
processes, local storage and uncontrolled filesystem cache/other OS activity. These are individual observations,
not statistical performance comparisons or latency budgets. Both before/after rebuilds used separate `cp -cR`
copies of the same source at each size. Fixture preparation and copying were outside the measurement.
Rebuild times include database open/rebuild/close; Node maxRSS includes process/native/mapped memory. Dump times/RSS
come from macOS `/usr/bin/time -l` around the CLI, including open/close. Node KiB and time's byte RSS are normalized
below to MiB. Do not compare absolute Node and CLI costs as if they were the same executable.

| Operation / rows | Before seconds | After seconds | Before peak RSS MiB | After peak RSS MiB |
|---|---:|---:|---:|---:|
| Dump 10k | 0.200 | 0.090 | 39.7 | 33.9 |
| Dump 100k | 2.710 | 1.220 | 355.8 | 303.1 |
| slug-10k | 0.151 | 0.150 | 71.8 | 70.2 |
| slug-100k | 1.906 | 1.635 | 330.9 | 306.2 |
| text-10k | 1.129 | 1.232 | 122.7 | 114.3 |
| text-100k | 11.406 | 12.865 | 723.5 | 463.1 |
| meaning-2k | 0.824 | 0.828 | 151.8 | 136.8 |

Text fixtures contain 1,024-byte bodies, unique slug and nonunique state value indexes, title/body text indexing and
19,997 / 199,997 relations at 10k / 100k entities. The 100k source is the retained reference fixture and has a different
fragmentation/history from the freshly seeded 10k source; this is not a controlled scale-only RSS experiment.
The vector fixture has 2,000 entities, 3,997 relations and 2,000 embeddings of 1,536 dimensions. The sampled text
rebuild trades some elapsed time for substantially lower peak RSS; neither the elapsed delta nor memory reduction
is a universal guarantee. Mapped/dirty pages explain why process RSS can still grow while the owned intermediate
buffers spill at fixed thresholds.

Owned receipts are under `/private/tmp/elmdb-memory-evidence-20261027`: `rebuild-{before,after}-*.json`,
`dump-fresh-before-*.{json,time}`, `dump-after-*.{json,time}`. An initial unsandboxed-metrics retry was required because
macOS time's kernel clock query is denied inside the sandbox; only the complete successful measurements above are
used. Older retained executables were also compared, but the table's baseline uses the fresh exact-revision build.

The 10k and 100k dumps match that fresh baseline byte for byte; 100k also matches the independently retained
`/private/tmp/elmdb-recovery-20261026-reference/primary.dump`. Vector dumps match byte for byte with 2,000 embeddings.
After every rebuilt fixture, verification checks clean derived/constraint reports, entity/relation counts, indexed
last-entity lookup and zero pending embeddings for the vector fixture. Receipt: `verification.log`. An initial ad hoc
lookup assertion compared the protocol envelope with an ID list; the corrected verifier checks its kind and ids and
all five fixture checks pass.

The recovery runner now supports `rebuild-value`, `rebuild-text` and `rebuild-vector` on its owned synthetic workloads.
Value/text modes and their scratch observer were smoke-tested on the 10k fixture; creation/seed commands retain their
existing refusal to adopt arbitrary directories. Source reference fixtures were not modified by rebuild measurements.

Fresh supported-platform and app strict-chain results for the final pinned pair are recorded on the PRs; prior
1723d5b / 837f69e64b green runs remain historical evidence, not acceptance of these changes. No merge or release.
