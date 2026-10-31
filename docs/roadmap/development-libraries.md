# Libraries for safer implementation and easier development

Status: partial implementation. Criterion maintenance benchmarks are implemented (2026-10-27); stage 1 is built and
stage 3 was evaluated and declined (2026-10-30), each with its record below; stage 4's first model is built and kept
(2026-10-30, [its record](../evidence/loom-unit-reservation.md)); stage 5 is built and kept behind a feature that is off
by default (2026-10-30, [its record](../evidence/diagnostics.md)).
The commands are in `../architecture/testing.md`, Repeatable maintenance measurements. Not yet benchmarked: the private
value-index emission and the spill sorter on their own, whose combined cost the public rebuild cases measure.

## Purpose and scope

Reduce handwritten platform code, make performance changes easier to evaluate, and improve evidence for concurrency
and operational failures. This follows the [review](../reviews/elmdb-review-2026-10.md) and its
[finding dispositions](../evidence/elmdb-remediation.md). It complements the active engine goals; it does not reorder
or expand them. Reconcile each affected file with ongoing work before implementation.

The initial work belongs entirely in elmdb. No apack changes or copied apack build directory are needed. Application
collection of Rust diagnostics is a separate integration decision, made only after a binding-level proof.

Keep the existing tempfile, proptest, parking_lot and generated protocol tooling. Add a dependency only when a small
implementation demonstrates its benefit. Do not introduce a second LMDB implementation, change the database format,
or change public error meanings as part of this work.

## Order and decisions

| Stage | Library | Decision and deliverable |
|---|---|---|
| 1 | windows-sys | Replace handwritten Windows declarations and constants; preserve behavior |
| 2 | Criterion | Establish repeatable benchmarks for a small set of engine operations |
| 3 | rustix | Evaluate Unix API coverage; migrate only operations that become clearer and safer |
| 4 | Loom | Model one production concurrency state machine, then decide whether to expand |
| 5 | tracing | Prove useful diagnostics and collection through the binding before broader instrumentation |

Each stage is a separate reviewable change. Stages 1 and 2 can proceed independently; stage 3 follows the platform API
inventory from stage 1. Stages 4 and 5 have explicit evaluation gates rather than unconditional adoption.

## 1. Generated Windows bindings

Use [windows-sys](https://docs.rs/windows-sys/latest/windows_sys/) as a direct Windows-only dependency with the minimum
required features. Inventory the declarations in process claims and backup publication, including event creation,
module pinning, last-error handling, moves and file attributes. Replace declarations and numeric constants with the
corresponding generated APIs. Keep resource ownership in OwnedHandle or a small private RAII wrapper.

Acceptance:

- No duplicated Windows ABI declarations remain in the migrated paths. Unsafe calls explain pointer validity,
  ownership and lifetime; generated declarations alone do not establish safety.
- Capture last-error values immediately where required. Preserve claim collision behavior, module lifetime,
  no-clobber publication, replacement behavior and clearing of temporary file attributes.
- Windows CI passes the existing positive tests and disposable negative proofs, including backup metadata and
  process-claim behavior. Review dependency features and lockfile changes for unnecessary additions.

Built. The six hand-declared kernel32 functions are windows-sys's: `CreateEventW`, `GetLastError` and
`GetModuleHandleExW` in the binding's process claim (`elmdb-node/src/process.rs`), and `MoveFileExW`,
`GetFileAttributesW` and `SetFileAttributesW` in backup publication (`store/backup.rs`), with their numeric flags,
attributes and `ERROR_ALREADY_EXISTS` named by it. windows-sys 0.61.2 was already in the lockfile through other
dependencies, so no crate was added; the features are the four the binding's calls need and the one backup's do.
`GetLastError` still follows `CreateEventW` immediately, the claim's handle is still owned by `OwnedHandle`, and the
pinned module and the attribute clearing are unchanged.

## 2. Repeatable performance benchmarks

Add [Criterion](https://github.com/bheisler/criterion.rs) as a development dependency. Start with key encoding,
streaming value-index emission, an indexed query with a limit, and the spill sorter's in-memory and spill paths.
Use fixed seeded fixtures, input-size series and explicit throughput units. Keep setup outside the timed section
unless setup is the operation being measured. Treat operations that mutate state carefully so iterations do equal work.

Acceptance:

- A documented command produces a baseline and comparison on the same machine and build configuration.
- Record toolchain, commit, fixture sizes, hardware and cache assumptions with published results. Observe scaling
  across sizes; do not infer algorithmic complexity from one timing.
- Demonstrate detection of one deliberate slowdown in a disposable copy. Initially publish results without a hard
  shared-runner CI timing gate; set budgets only after measuring normal variance.
- Retain the recovery workload harness for populated replacement, peak RSS, scratch space and disk growth. Criterion
  measurements do not substitute for those resource measurements.

The same operations are counted in instructions too, which don't move with the machine's load
(`verification-tooling.md`, stage 7).

## 3. Typed Unix filesystem operations

Evaluate [rustix](https://docs.rs/rustix/latest/rustix/fs/index.html) against the actual Linux and macOS operations:
no-replace rename, descriptor-relative opens, ownership checks and no-follow handling. Record an API/target coverage
table before editing. Prefer standard-library APIs where sufficient, including file locking. Retain libc for APIs
without a suitable replacement; removing it entirely is not a goal.

Acceptance:

- Migrate a narrow operation only if it reduces custom unsafe code or makes ownership and error handling clearer.
  Confirm macOS and Linux semantics separately; an API name is not evidence of equivalent guarantees.
- Preserve race resistance, collision refusal, symlink and ownership checks, and error-code behavior. Do not weaken
  no-clobber publication to obtain a common implementation across platforms.
- Run affected platform tests, including the real Linux read-only-mount refusal. Keep explicit filesystem limitations
  documented: a wrapper does not certify exFAT or SMB behavior or durability.
- If coverage offers little improvement, record the decision and retain the existing code without adding rustix.

Declined, 2026-10-30, against rustix 1.1.5 (already in the lockfile, through tempfile). The Unix calls elmdb makes
itself, and what rustix offers each:

| Operation | Where | Today | rustix | Decision |
|---|---|---|---|---|
| No-replace rename (Linux `RENAME_NOREPLACE`, macOS `RENAME_EXCL`) | `store/backup.rs` `publish_new` | tempfile's `persist_noclobber`, which calls rustix | the same call, by hand | keep tempfile |
| Link fallback where the rename is refused | `exclusive_rename_or_link` | `std::fs::hard_link`, matched on `libc` errno constants | `Errno` constants | keep: no unsafe code to remove |
| Process randomness for the Linux claim (`AT_RANDOM`) | `elmdb-node/src/process.rs` | `libc::getauxval` and a 16-byte read | `runtime::random`, a hidden experimental module "for libc-like users", still a raw pointer | keep libc |
| The macOS claim's Mach receive right | `elmdb-node/src/process.rs` | the task port and two Mach calls, declared | none: Mach isn't POSIX | keep |
| `atexit` | `elmdb-node/src/exit.rs` | a declared C function | none | keep |

Descriptor-relative opens, ownership checks, no-follow handling and file locking are LMDB's and tempfile's, not code
of elmdb's own, and the read-only mount refusal is LMDB's error with elmdb's reader registration. Nothing here would
lose unsafe code or gain clearer ownership by moving, so rustix isn't a dependency of elmdb's own.

## 4. Concurrency model checking

Evaluate [Loom](https://github.com/tokio-rs/loom) on unit reservation versus ordinary writer acquisition first. State the
invariant explicitly: once a unit owns the reservation, another writer cannot proceed into an incompatible acquisition
path. Include reservation release and refusal paths in a bounded model.

Acceptance:

- Exercise the same synchronization decision logic used by production, using a narrow test configuration for Loom
  primitives. Do not create an independent toy implementation that can drift from production.
- Keep LMDB and native OS locks outside the model. Document what the abstraction assumes, what Loom explores, its
  limitations, and the exploration bounds used in CI.
- A disposable mutation reproducing the historical reservation race fails the model. The corrected model passes in
  a bounded CI job; existing real-process and deterministic barrier tests remain required.
- Expand to watch teardown or close coordination only if the first model is maintainable and catches useful failures.
  If production abstraction costs exceed the benefit, document the experiment and defer adoption.

Built (2026-10-30): the slot is `store/unit_slot.rs`, its primitives `crate::sync`, and its model beside it; the
command, bounds and what the model leaves out are in `../architecture/testing.md`, Concurrency models. Kept: the
historical race and a release that wakes no waiter each failed the model on a disposable copy, and the production change
is one small type the store calls instead of a lock and a condition variable
([record](../evidence/loom-unit-reservation.md)). Watch teardown and close coordination are modelled by stage 8 of
[verification tooling](verification-tooling.md#8-randomized-concurrency-testing), on the same primitives.

## 5. Structured operational diagnostics

Status: built, 2026-10-30. `tracing` spans on a rebuild (the recovery operation), the writer's acquisition and a
unit's begin and wait (the transaction-wait paths) and the sorter, behind elmdb's `diagnostics` feature, off by
default. Nothing collecting, they cost at most 0.02% of any benchmarked operation's instructions, a gate holds them to
0.05%, and a Node program collects them through the binding's experimental `collectDiagnostics()`
([the record](../evidence/diagnostics.md)). Public configuration through the binding, and apack's use of it, are not
decided; they are proposed apart when a stall needs them.

Evaluate [tracing](https://docs.rs/tracing/latest/tracing/) with one recovery operation and one transaction-wait path.
Record operation phase, elapsed time, counts, spill bytes and committed-versus-durable outcome where available.
Exclude entity values, query payloads and sensitive paths by default. Use coarse spans rather than per-row events.

The engine emits diagnostics; it never installs a global subscriber. The CLI or test executable may configure one.
Prove how an embedding Node application can opt into collection before committing to a public configuration surface.
Rust remains the definition of any new cross-language contract.

Acceptance:

- A test collector verifies useful phase and failure events without changing return values, error precedence or commit
  behavior. Verify quiet behavior without a collector and measure disabled and enabled instrumentation overhead.
- Define feature/default policy and collection ownership from the proof. Test supported feature combinations; avoid
  duplicate state machines for diagnostics and avoid introducing a Tokio runtime solely for tracing.
- Document an end-to-end diagnostic example through the binding. If useful collection requires apack changes,
  propose that integration separately and use an isolated apack worktree for any subsequent implementation.

## Delivery and validation

For each stage, record the dependency version and license, target/MSRV compatibility, feature choices, dependency-tree
impact and maintenance rationale. Follow the repository's pinned toolchain and existing dependency conventions.
Update architecture/testing documentation where commands or guarantees change. Put experiment results and negative
proof receipts in docs/evidence, linked from the implementing PR.

Run the checks required by CLAUDE.md for engine changes: formatting, clippy, workspace tests, generated-contract and
single-LMDB checks, plus binding build, types and tests. Run the shipped-platform matrix for platform changes. Keep
expensive builds and benchmarks under one execution owner and use isolated worktrees while other agents are active.
Demonstrate new correctness assertions failing on disposable mutations rather than modifying a shared working tree.

Completion means the accepted stages have reviewable implementations and evidence, and every evaluated-but-deferred
stage has a written decision. Single-index rebuilding and dump export already stream/spill their intermediate
collections (c78c623).
This plan does not resolve remaining full-audit inventories, safe crash cleanup, structured
committed-error metadata or filesystem certification; those remain distinct follow-ups in the review dispositions.
