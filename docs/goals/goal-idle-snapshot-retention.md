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

- Release reproduction confirms the smoke result; raw source-hashed evidence is committed under
  `docs/evidence/idle-snapshot-retention/`. Workspace clippy and formatting passed.
- Production behavior is unchanged at the measurement checkpoint. A draft `idle_reads.rs` is
  intentionally excluded from that checkpoint until integrated and validated.
- Next: integrate the 100 ms idle cache, prove resource/lifetime behavior and mutation sensitivity,
  measure matched overhead and run required landing gates.
- No PR exists yet. The graph-planning commit on local main belongs to another agent; preserve
  it and recheck remote main before publishing. No merge, release or dependency-pin update.
