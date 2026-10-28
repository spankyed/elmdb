# Goal: mutation testing, named failpoints and tempfile publication

Status: in progress, 2026-10-28. Stages 1, 2 and 9 of `docs/roadmap/verification-tooling.md`, in one pass.

```text
# Goal: mutation testing, named failpoints and tempfile publication

Implement elmdb/docs/goals/goal-verification-tooling.md. Read Scope, Decisions, Phases, Acceptance and Constraints
before continuing. The tools and what each replaces are in elmdb/docs/roadmap/verification-tooling.md, stages 1, 2
and 9. Work in ~/Develop/Projects/apack/elmdb, following its CLAUDE.md and this goal's Constraints, on one branch.

Inspect current code before relying on a path, a hook's name or a figure here.

Finished when:
- Every row of Acceptance is met on current source, with the commits and the evidence recorded.
- verification-tooling.md, testing.md and the docs this changes describe what was built.
- Outcome is written, the goal is archived, and closure is committed.

Commit usable checkpoints as you go. Keep Current handoff current. Do not expand or reduce scope without the user's
authorization.
```

## Scope

**Outcome.** elmdb's mutation checks are run by a tool rather than by hand, its injected faults are named failpoints
that compile out of every non-test build, and its file publication is `tempfile`'s where `tempfile` gives the same
guarantees.

**Deliverables:**

1. cargo-mutants configured for the workspace, a baseline over the engine's highest-risk modules with every survivor
   triaged, and the command documented.
2. The test-only fault hooks replaced by `fail` failpoints, keeping each test's faults its own.
3. `publish_new` (the dump's no-clobber publication) and the backup's replacing publication on `tempfile`'s
   `persist_noclobber` and `persist`, if Phase 3's comparison shows they hold the same guarantees.

**Not in scope:** the other stages of verification-tooling.md and development-libraries.md; apack, whose pin need not
move, since no behaviour of the binding changes.

## Current handoff

- **State:** started 2026-10-28 on `session/verification-tooling`, from `main` at `0646348`, after elmdb-next merged
  (apack `master` at `98386b36a1`, whose chain passed with it linked). cargo-mutants 27.1.0 is installed.
- **Phase 3's reading of `tempfile` 3.27.0** (`src/file/imp/{unix,windows}.rs`), before Phase 3 starts: on Linux and
  macOS `persist_noclobber` renames with `RENAME_NOREPLACE` and falls back to a hard link and an unlink on `ENOSYS` or
  `EINVAL`, where `exclusive_rename_or_link` also falls back on `EOPNOTSUPP` and macOS today has no fallback; on Windows
  it clears every attribute (`FILE_ATTRIBUTE_NORMAL`) and moves without `MOVEFILE_WRITE_THROUGH`, which both of elmdb's
  publications pass.
- **Phase 2 done** (`2c9b64e`): fifteen hooks as failpoints through `crates/elmdb/src/failpoints.rs` (one,
  `FAIL_SYNC_AT`, wasn't in Phases' list); V2's spec and mutation, V3's `cargo tree` and symbol check, and three
  workspace runs at a load of 205 to 250.
- **Phase 3 done** (`87e31c9`): Unix's `publish_new` on `persist_noclobber`, Windows' `move_file` kept for
  write-through; the eight-target matrix passed (run 40674565378).
- **Phase 1 under way.** `trigram.rs` done (`9a978be`). Pass 1 (the fast suite) over `reads.rs`, `text/search.rs`,
  `entities.rs`, `values.rs` and `schema/diff.rs` finished: 667 mutants, 465 caught, 161 missed, 36 unviable,
  5 timeouts; results kept in `target/mutants-pass1/mutants.out`, whose `previously_caught.txt` `--iterate` reads.
  `entities.rs` done (`c8c00ee`). `values.rs`, `text/search.rs` and `schema/diff.rs`' survivors have specs or
  exclusions (`26650b0`), being confirmed by a fast-suite `--iterate` over those three. `reads.rs`: of its 75
  survivors, a full-suite pass reached 12 before it was stopped (it ran from a tree without the new specs): 5 caught,
  7 missed, of which `answer`'s, `find_relations`' three and `stage`'s `withRole` now have specs,
  `walk_type`'s `<=` is excluded, and `pull`'s `upto + 1` moves only which stage a time is charged to — a finding, no
  count showing it.
- **Disk:** each cargo-mutants copy takes about 1.3 GB on the `mutants` profile; -j 3.
- **To continue:** after the confirmation, the full suite over `reads.rs`' survivors:
  `cargo mutants -j 3 -o target/mutants-pass1 --iterate --file crates/elmdb/src/engine/reads.rs`; then the same
  over the other three files for what the fast suite left; then `--in-diff` once against `main`.
- **Evidence:** `docs/evidence/verification-tooling.md`, with Phase 1's baseline still to record.
- **Next action:** triage what the full suite leaves in `reads.rs`, record the baseline, then `--in-diff`.

## Decisions

| # | Decision |
|---|---|
| V1 | One branch, `session/verification-tooling`, in elmdb's checkout; merged fast-forward. |
| V2 | **A test's faults stay its own.** The hooks are thread-local, or keyed by a database's path (`FAIL_COMMIT_AT`, `FAIL_STAMP_AT`), so the suite's parallel tests can't trip each other's. `fail`'s registry is process-wide, so a failpoint is configured only through a helper that keeps that scope: an action given with `fail::cfg_callback` that acts on the configuring thread only, or only for a given path. A test configuring a failpoint directly (`fail::cfg`) is refused by review, and the helper's doc says why. |
| V3 | **Failpoints compile out.** `fail` is a dependency of `elmdb` without features, and a dev-dependency with `failpoints`, so `fail_point!` is empty in every build but `cargo test`'s (resolver 2 keeps dev-dependency features out of normal builds). Phase 2 shows this rather than assuming it: `cargo tree -e features` for `elmdb-node` lists `fail` without `failpoints`, and the release binding holds none of `fail`'s registry symbols. |
| V4 | **cargo-mutants runs scoped, not over the crate.** The model harness takes about a minute, so a whole-crate run is hours. The documented commands are `--file` over named modules and `--in-diff` against a branch's base. Its configuration (`.cargo/mutants.toml`) sets the test command, a timeout and the exclusions, each exclusion with its reason. |
| V5 | **A survivor is triaged, not ignored.** Each is one of: a test added that kills it; a behaviour-preserving mutation (an equivalent rewrite), skipped with `#[mutants::skip]` or a config exclusion and a one-line reason; or a real gap recorded as a finding. No assertion is loosened to make a mutant die or live. |
| V6 | **Publication moves to `tempfile` only where it holds what the hand-written code holds**, read off `tempfile`'s source at the version the lockfile pins: an atomic no-replace on Linux (`renameat2(RENAME_NOREPLACE)`, with a hard-link fallback where it isn't supported, as `exclusive_rename_or_link` does), macOS (`renamex_np(RENAME_EXCL)`) and Windows (`MoveFileExW` without replace); clearing the temporary-file attribute Windows files are created with; and the durability `MOVEFILE_WRITE_THROUGH` gives. What `tempfile` lacks stays as a step around its call; if that leaves more code than it removes, the hand-written code stays and the evidence says why. |
| V7 | The hand-made mutation scripts (`scripts/check-process-publication-mutations.py`, `check-windows-backup-mutation.py`, `check-maintenance-verification.py`) are each replaced by a cargo-mutants invocation where one reproduces what it checks, on the platform it runs on, and kept otherwise with the reason in its header. The heap and timing mutations of `check-maintenance-verification.py` are not source mutations cargo-mutants makes, and are expected to stay. |

## Phases

### Phase 1: cargo-mutants

- **Deliverable:** `.cargo/mutants.toml`; a baseline over `engine/trigram.rs`, `engine/reads.rs`, `engine/text/search.rs`,
  `engine/entities.rs`, `engine/values.rs` and `schema/diff.rs`; every survivor triaged (V5); the commands in
  `docs/architecture/testing.md`; V7's scripts decided.
- **Done when:**
  - `cargo mutants --file <module>` runs to completion for each module above, its report (caught, missed, unviable,
    timeout) recorded in `docs/evidence/verification-tooling.md`.
  - Every missed mutant is killed by a new test, skipped with its reason, or recorded as a finding.
  - Each new test is shown failing under the mutant it kills (cargo-mutants' rerun of that mutant).
  - `cargo mutants --in-diff` over a branch is documented and run once against this goal's own diff.

### Phase 2: named failpoints

- **Deliverable:** every hook below a failpoint, configured through V2's helper; the hooks and their statics gone.
  - In `store/mod.rs`: `FAIL_COMMIT_AT`, `FAIL_STAMP_AT`, `FAIL_NEXT_COMMIT`, `FAIL_RESET_CHECKPOINT`,
    `BEFORE_WRITE_RESERVATION`.
  - In `database.rs`: `BEFORE_SCHEMA_RECOVERY`, `BEFORE_SCHEMA_WRITE`.
  - In `dump.rs`: `BEFORE_LOAD_PUBLISH`, `AFTER_LOAD_PUBLISH`.
  - In `engine/mod.rs`: `BEFORE_READ_SNAPSHOT`, `FAIL_NEXT_INDEX_REFRESH`.
  - In `engine/watch.rs`: `PANIC_NEXT_READ`.
  - In `engine/derive/sorted.rs`: `FAIL_NEXT_NEW`.
  - In `store/backup.rs`: `HOOK`, with its points.
  - Not hooks: `sorted.rs`'s `TEST_BUDGET`, a size override, and `watch.rs`'s `CALLBACKS`, which is production state.
- **Done when:**
  - Each test that used a hook passes through its failpoint, unchanged in what it asserts. The model and crash harnesses
    pass (the model harness's `RefuseNext` uses `FAIL_COMMIT_AT`; the crash harness's child configures no fault).
  - A spec shows V2: a failpoint configured on one thread doesn't fire on another, and one keyed to a path doesn't fire
    for another path; a mutation of the helper to configure process-wide fails it.
  - V3 is shown: `cargo tree -e features -p elmdb-node` and a symbol check of the release binding.
  - `cargo test --workspace` passes repeatedly on a loaded machine with the default parallelism (three runs), since a
    leaked failpoint shows up as an unrelated test failing.
  - The roadmap's list of what `fail` replaces (verification-tooling.md, stage 2) matches what was replaced.

### Phase 3: tempfile publication

- **Deliverable:** V6's comparison recorded, then, where it holds, `publish_new` as `TempPath::from_path(..)
  .persist_noclobber(..)` and the backup's `publish` as the temporary path's `persist`, with `move_file`,
  `exclusive_rename_or_link` and the platform branches removed.
- **Done when:**
  - The comparison of `tempfile`'s source at the pinned version against each guarantee in V6 is in the evidence, by
    platform.
  - The publication specs pass: a target that exists is refused and left as it was; a published file is complete; on
    failure the temporary is removed. The Linux fallback's coverage is kept: if `tempfile` makes it uninjectable, the
    spec says so and a CI case on a filesystem without `RENAME_NOREPLACE` stands in, or the fallback stays and is said
    to.
  - The Windows backup case (`scripts/check-windows-backup-mutation.py` or what V7 replaces it with) passes on the
    Windows CI target, and the full eight-target CI matrix passes.

## Acceptance

| Requirement | Implementation / source | Evidence | Remaining work |
|---|---|---|---|
| cargo-mutants configured, the baseline run and every survivor triaged (P1) | missing | | all |
| The fault hooks replaced by failpoints, each test's faults its own, none in a normal build (P2) | missing | | all |
| Publication on `tempfile` where it holds the same guarantees (P3) | missing | | all |
| The docs describe what was built | missing | | all |

## Constraints

- **Commits:** logical checkpoints with conventional messages and no attribution lines; stage only this goal's files
  and read `git diff --cached` first. Never tag, publish or release.
- **Mutations:** cargo-mutants copies the tree to its own directory; any hand mutation is made in a copy-on-write clone
  (`cp -cR`), never the working tree.
- **apack:** this goal changes elmdb only. Don't touch `crates/elmdb-node` while an apack chain is running, since the
  chain freezes it as a compiler input.
- **Tests:** a test that can't fail is a gate or an assertion with a named firing edit. Don't loosen an assertion, a
  deadline or a tolerance to get a pass or to kill a mutant.
- **Machine:** the disk is close to full; cargo-mutants' copies and `target` directories are this goal's to remove when
  done. Run one expensive job at a time.
- **Failures:** reproduce the failing case before changing anything; a failure that recurs without a diagnosis goes in
  Current handoff with its evidence.
