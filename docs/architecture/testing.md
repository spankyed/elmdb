# Testing

What proves elmdb correct, and where each proof lives. `cargo test --workspace` runs the Rust suites; `npm test` runs
the binding's.

## The model harness

The acceptance suite is a model harness (`crates/elmdb/src/engine/tests/model.rs`), ported from apack's
(`@apackdev/ears`, `tests/_harness/`): random workloads (proptest) over a small pool of ids, so writes collide, refer
to each other and are refused, checked after every operation against a plain reference model. The engine must hold
what the model holds, refuse what it refuses, and answer every derived read as the model does: a type's entities,
count and trash, value index lookups, roles, each edge's far ends, and a text index's matches for a few words. A unit,
a savepoint and a refused commit roll back with it. Ranking is checked separately, against fixed corpora
(`engine/text/tests.rs`, `engine/vector/tests.rs`).

It runs in two places:

- in Rust, against the engine directly;
- in apack, against the engine through the binding and `@apackdev/ears` (`tests/engine/model-based.spec.ts`), where it
  also checks a watch on every derived read, every row and the whole state against the read run afresh after every
  op; and during the move to elmdb it drove apack's JavaScript engine with the same workloads and compared the two
  (`docs/evidence/phase-9.md`).

## Crashes and failed syncs

The crash harness (`engine/tests/model.rs`, `mod crash`): a child process, the test binary run again, applies a seeded
workload with syncs on and acknowledges each step; the parent kills it with SIGKILL mid-workload, reopens the files and
compares them with the model after the acknowledged steps, or one more. The failed-sync harness
(`store/sync/tests.rs`) runs random sequences of commits, `durable()` waits, failing syncs and clock advances, and
checks that a wait resolves only once a sync covering its commit succeeded and rejects when the sync it waited on
failed.

## Derived data

`check()` runs after every model-harness case, so an index that drifts from its primary data fails the case that
caused it. `engine/tests/derived.rs` corrupts derived keys directly and checks that `check()` reports them and
`rebuild()` repairs them; text and vector indexes are checked and rebuilt in their own specs (`engine/text/tests.rs`,
`engine/vector/tests.rs`).

## Tracked reads

`engine/tracking/tests.rs` holds the interval index to a brute-force scan of every read set, through random adds,
removals and written keys; `engine/tests/tracked.rs` checks that a commit names exactly the tokens it wrote into,
that a stopped scan is tracked as far as it went, the commit sequence, and `Database::watch`.

## The binding

`crates/elmdb-node/test/`: each operation through JavaScript, and fuzzing (`fuzz.test.mjs`): random bytes and random
msgpack as batches, reads and loads, flipped bytes, arguments of the wrong type, a closed database and an ended unit.
Each must apply or throw: an error carrying an elmdb code when the payload reached elmdb, and N-API's own for an
argument of the wrong JavaScript type; and `check()` stays clean. The TypeScript types are generated
from the Rust ones (`cargo xtask generate`), so a mismatch fails the typecheck before a test runs.

## Platforms

CI (`.github/workflows/ci.yml`): format, clippy and tests on every push, on Linux; on dispatch and pull requests, the
full matrix — macOS (arm64, x64), Linux (x64, arm64; glibc and musl) and Windows x64 — each job building the binding
and running every suite, the model and crash harnesses included, on it.

Filesystem support requirements and the limits of this platform coverage are in [filesystems.md](filesystems.md).
