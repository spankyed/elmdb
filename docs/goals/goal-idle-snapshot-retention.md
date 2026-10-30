# Bound idle snapshot retention

Status: active, 2026-10-30. Branch `session/idle-snapshot-retention`, worktree
`/private/tmp/elmdb-idle-snapshot-retention`, based on current local main `717e903`.

```text
Investigate and address idle snapshot retention. Follow CLAUDE.md and the L01 research plan.
Read this goal's current handoff, scope and acceptance before continuing. Revalidate source and live
process/CI state; do not rely on historical handles as proof that work is still running.
Finish with measured reproduction/control evidence, a justified bounded-release policy if needed,
regression and mutation proofs, required validation, and an open PR against main. Do not merge,
release or change apack's pin. Preserve other agents' work and commit validated checkpoints.
```

## Scope and acceptance

| Requirement | Required evidence |
|---|---|
| Demonstrate or disprove retention | A live read-once process stays open without further database calls; another overwrites fixed-size values; compare cached and explicit-release controls, one and several readers |
| Distinguish resources | Record allocated/live/free-list pages, transaction IDs, physical file growth and map capacity; demonstrate reuse after release rather than expecting file shrinkage |
| Measure the tradeoff | Release-build read costs and matched before/after representative reads; label synthetic controls and profiler/control overhead |
| Justify implementation | Select a finite idle bound from supported workloads; preserve useful cache reuse and use supported heed APIs |
| Preserve semantics | Active snapshots, units, concurrent readers/writers, late completion after a commit, resizing, normal close and exit cleanup |
| Regressions that fire | Finite native/concurrency/resource checks and a disposable mutation that restores retention |
| Integrated delivery | Required Rust/binding checks, source-specific evidence, a pushed branch and PR with passing applicable CI; no merge/release/pin update |

## Findings and decision boundary

Pinned heed 0.23.0 exposes read-transaction destruction but no public reset/renew operation. No dependency
patch or raw-pointer reset is authorized by this plan. The first smoke experiment demonstrates linear
file growth with cached readers and stable allocation with explicit release. Its timings are debug-only.
Release confirmation and actual application-shaped overhead still determine the implementation choice.

Before implementation, compare bounded caching with removing cache retention. A proposed 100 ms idle
window would retain reuse across typical 16 ms view bursts while bounding a reader that goes idle;
it matches the existing default group-commit idle interval, rather than creating an unbounded retention
policy. This is a candidate, not a measured performance result. Active operations are never expired. Before implementing that candidate, the acceptance budget is
at most 0.1 microsecond added per cached raw Store read, and no unexplained regression beyond 10% in
the existing binding field/page/projected/full-row workloads under balanced comparisons. These are
manual experiment decision bounds, not shared-runner CI timing gates. Preserve the retention fix even
if tuning is needed; investigate a failed bound instead of changing it after seeing the result.
Measure point-read overhead and the existing field/page/projected/full-row workloads. Declare the
acceptance tradeoff before evaluating production candidates; do not select a favorable threshold after tuning.

## Current handoff

- Measurement checkpoint `551311f` records baseline release retention (50 MiB cached versus 4.6 MiB
  explicit release) and raw timings. The candidate now matches the 4.6 MiB control for one/four live
  idle processes; cached raw reads add about 50 ns, within the predeclared 100 ns budget.
- `IdleReads` implements a per-entry 100 ms window, generation invalidation on local commits,
  background expiry, joined normal shutdown and bounded detached process-exit shutdown. Runtime,
  new tests, docs and CI mutation integration form the validated runtime checkpoint.
- Disposable mutation proof passed: indefinite renewal makes the idle-child regression fail on
  retained snapshots. Full Rust checks passed (369 tests, including the model harness; clippy,
  formatting, doc tests, generated protocol and one-LMDB checks). Two additional lifetime tests
  were added afterwards; all six lifetime tests passed the focused run. Binding build, typecheck
  and tests passed (34 passed, four platform-specific skips).
- Running: balanced baseline/candidate binding reads,
  with an exact baseline source archive preserved in this checkout's owned Docker target volume.
  Revalidate actual process state before continuing; do not restart a live run.
- Next: preserve binding reports, assess the declared overhead budget, finish source review,
  commit validated runtime work, exclude the other agent's unpublished graph-doc parent from the
  PR ancestry if necessary, push and open a PR. Fix applicable CI; no merge/release/pin update.
