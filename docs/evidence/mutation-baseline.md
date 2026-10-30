# The mutation baseline over the store, watches, derived data and the process claim

The rest of stage 1 of `../roadmap/verification-tooling.md`: cargo-mutants over the modules
`../archive/goals/goal-verification-tooling.md` left, `store/`, `engine/watch.rs`, `engine/derive.rs` with
`engine/derive/`, and `elmdb-node/src/process.rs`. 2026-10-28 and 29, on `session/mutation-baseline`. Machine: Apple
M1 Pro, 10 cores, cargo-mutants 27.1.0 at `-j 3`; the Linux readings in a `rust:1-bookworm` container of 3 CPUs and
6 GB.

## Method

As `../architecture/testing.md`, Mutation testing: each elmdb file against the suite without the model harness, then
the missed against the whole suite (`--iterate`, the timeouts and unviable marked done so only the missed ran again).
`process.rs` is elmdb-node's, and `.cargo/mutants.toml` tests elmdb, so it runs with `--test-package elmdb-node`.

## The first reading

On `a1ad596`, 471 elmdb mutants in 83 minutes, then the 90 missed against the whole suite in 43 minutes, which caught
none of them: the model harness asserts what `check()` and the plans answer, which none of these mutants changes.

| File | Mutants | Caught | Unviable | Missed | Timeout |
|---|---|---|---|---|---|
| `store/mod.rs` | 119 | 66 | 22 | 29 | 2 |
| `store/backup.rs` | 25 | 8 | 0 | 17 | 0 |
| `store/sync.rs` | 71 | 34 | 19 | 11 | 7 |
| `engine/watch.rs` | 70 | 61 | 3 | 5 | 1 |
| `engine/derive.rs` | 89 | 61 | 5 | 19 | 4 |
| `engine/derive/audit.rs` | 49 | 41 | 3 | 3 | 2 |
| `engine/derive/sorted.rs` | 48 | 20 | 9 | 6 | 13 |
| `elmdb-node/src/process.rs` | 19 | 3 | 1 | 15 | 0 |

A timeout is a mutant that makes a loop never end, which the suite's deadline catches.

## What became of each survivor

Of elmdb's 90: 47 killed by new specs, 13 gone with the dead code they were in, 11 excluded as answering what the code
answers, 2 left as a finding, and 17 in code this Mac doesn't compile (`backup.rs`'s publication): its Linux 3
killed in a Linux container, and its Windows 14 read on Windows below.

**What the survivors showed untested**, each now a spec: a new map's size; the read-only open's message, and a writable
open refused the same files without it; which stores run group commit, and the syncs it counts; an idle read
transaction taken by the next read, and at most `MAX_IDLE` kept; the graph databases a writable open creates and a
read-only one opens without; a unit's growth once the map is more than half used; a map another process grew past this
one's; a failed sync at exit (`sync_owed` takes the `SYNC` failpoint for it); group commit stopped at exit with its locks
free and while a sync holds one, and dropped while it owes a commit; a commit synced once writes pause; a watch's third
read under the lock; two reads in flight filing their own dependencies; `is_clean`; each of three text and three
trigram indexes built in the background and rebuilt by name from its own fields, and each of two vector indexes;
`check()`'s endpoint and one-target violations and every holder of a unique value in one; a spill's last write winning
and its runs merging by level; and the claim refused while held and made once free.

**Dead code removed**: `Tracks::all_watches`, unused since `af141ad` replaced it with `touched(None)`, and
`derive::derive`, which nothing called beside its `derive_into`.

**Excluded** (`.cargo/mutants.toml`, each with its reason): `check_stamp`'s newer-format arm at the format's own value;
the sync worker's owed check and a manual clock's wait; a build error other than a refusal, which the fallback's walk
returns; the spill budget; `buildable_entry`'s and `build_chunk`'s skip of an entity's own entry; and `begin`'s growth at
exactly half the map.

**Findings:**

- **`MAX_GROWTHS`** (`store/mod.rs`, `write_committed`): a write that never fits grows the map 16 times before it is
  refused, and a mutant that removes the bound grows it until the OS refuses. A spec would map 2^16 times a step of
  64 MB — 2 TB of address space, and on Windows, where LMDB extends the file to the map, as much disk. Left as survivors.
- **An acquisition error other than an occupied claim** (`process.rs`, `claim_process`): the guard that tells them
  apart is mutated to treat every error as occupied, and nothing a test can do makes the kernel refuse a claim
  otherwise. Caught since 2026-10-30: the guard is `refusal`'s, which a test hands an error of any kind, and its four
  mutants are caught.
- **`pin_module`** (`process.rs`, 1): replaced with `Ok(())`, the Rust suite passes, and the binding's
  `test/process.test.mjs` fails "a claim outlives its first worker" with `ALREADY_LOADED`; a suite cargo-mutants doesn't
  run, as for `claim`'s mapping. Excluded in `.cargo/mutants.toml` since 2026-10-30, with that suite named.
- **The claim for any other platform** (`process.rs`, 1): compiled by no target elmdb builds, so no reading reaches it.
- **`claim`'s mapping** (`process.rs`, 1): its napi error links only in Node, so the Rust suite can't call it, and the
  binding's suite that does (`test/process.test.mjs`, refused as `ALREADY_LOADED`, run by CI in every target's binding
  job) isn't one cargo-mutants runs. Excluded in `.cargo/mutants.toml` since 2026-10-30, with that suite named.

## `process.rs`

`claim` makes a napi error, which a Rust test binary can't link outside Node, so its decision is `claim_process`,
answering a code and a message, and `claim` maps it. A spec in a child process holds a claim, finds `claim_process`
refused as `ALREADY_LOADED`, drops it and claims. On this Mac, 20 mutants: 6 caught, 1 unviable, 13 missed:

- `claim`'s mapping and the acquisition error, the findings above;
- Linux's claim (3), read below;
- Windows' named event and module pin (7), read on Windows below;
- the claim for any other platform (1), which no target elmdb builds compiles.

**The Linux reading**, in the container on `23d5515`: `process.rs` against elmdb-node's suite, 20 mutants, 5 caught,
2 unviable, 13 missed. Linux's claim is all caught or unviable (`acquire_claim`'s `AddrInUse` check caught; its and
`claim_address`'s whole answers don't compile), and the 13 missed are the macOS (3), Windows (7) and other-platform
(1) code and the two findings. `backup.rs`' Linux fallback, `exclusive_rename_or_link`, against the fast suite: 3
mutants, 3 caught. `backup.rs`' other 14 are `cfg(windows)`, read on Windows below.

CI on `4d5d0d5` (run 40782911891), whose source is the branch's final source, passed all eight targets and the Linux
checks: the Windows target ran the Windows claim, pin and publication code under every suite and the attribute mutation
script, every target's binding job ran `test/process.test.mjs` over `claim`'s mapping, and the Linux job ran
`check-process-publication-mutations.py`.

## The end state

On `01ea879`, the branch's final source, every file of the first reading's with the configuration's exclusions,
against the fast suite: 447 mutants in 84 minutes, 335 caught, 61 unviable, 19 missed, 32 timeouts.

| File | Caught | Unviable | Missed | Timeout |
|---|---|---|---|---|
| `store/mod.rs` | 89 | 22 | 2 | 4 |
| `store/backup.rs` | 8 | 0 | 17 | 0 |
| `store/sync.rs` | 43 | 19 | 0 | 7 |
| `engine/watch.rs` | 62 | 3 | 0 | 2 |
| `engine/derive.rs` | 67 | 5 | 0 | 4 |
| `engine/derive/audit.rs` | 44 | 3 | 0 | 2 |
| `engine/derive/sorted.rs` | 22 | 9 | 0 | 13 |

The 19 missed are the ones triaged above as compiled elsewhere or a finding: `backup.rs`' Linux fallback (3, caught in
the Linux container) and Windows code (14), and `write_committed`'s `MAX_GROWTHS` bound (2).

## The Windows reading

On the Windows development VM (`../references/guides/windows-development-vm.md` in apack; Windows NT 10.0.26100, two
cores, rustc 1.99.1, cargo-mutants 27.1.0 at `-j 2`), on `8cba243`, `backup.rs` against the fast suite and `process.rs`
against elmdb-node's. Of the Windows code's 21 mutants, 15 caught and 6 missed; every other missed mutant is code for
another platform or a finding above.

| Windows code | Mutants | Caught | Missed |
|---|---|---|---|
| `backup.rs`' `move_file` and `publish` | 14 | 10 | 4 |
| `process.rs`' named event, claim and module pin | 7 | 5 | 2 |

**What became of the 6:**

- `move_file`'s temporary-attribute check, `&` to `^`, and its `NORMAL` fallback, `==` to `!=`: killed by
  `publishing_clears_the_temporary_attribute_and_keeps_every_other`, a Windows spec publishing a file temporary, temporary
  and hidden, and hidden alone. A re-run of lines 103 and 106 on the branch: 4 mutants, 4 caught.
- `move_file`'s check, `&` to `|`, which always takes the branch, and for a file without the attribute sets the ones it
  already has: excluded.
- `MoveFileExW`'s and `GetModuleHandleExW`'s flags, `|` to `^` (2): disjoint bits, which both join alike. Excluded.
- `pin_module` replaced with `Ok(())`: the finding above, which the binding's suite catches.
