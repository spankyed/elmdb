# Testing

What proves elmdb correct. Status: planned.

## The model harness

The acceptance suite is a model harness, ported from apack's (`@apackdev/ears`, `tests/_harness/`): random workloads
over a small pool of ids, so writes collide, refer to each other and are refused, checked after every operation
against a plain reference model. The engine must hold what the model holds, refuse what it refuses, and answer every
derived read as the model does: a type's entities, count and trash, value index lookups, roles, each edge's far ends,
and for search the set of matches (ranking is checked separately, against fixed corpora).

It runs in two places:

- in Rust (proptest), against the engine directly;
- in apack, against the engine through the binding and `@apackdev/ears`, where it also drives apack's current
  JavaScript engine with the same workload during parity and compares the two (`roadmap.md`).

## Crashes

A child process writing through the binding is killed with SIGKILL mid-workload, and the reopened database holds the
model's state after the writes it acknowledged, or one more. A second harness fails syncs on demand and checks
`durable()` waits for the right one and rejects when it fails.

## Derived data

`check()` runs after every model-harness case, so an index that drifts from its primary data fails the case that
caused it. A separate case corrupts derived keys directly and checks that `check()` reports them and `rebuild()`
repairs them.

## The binding

Wrong argument types, out-of-range options, closed databases, plans and batches that don't decode, and each must throw
an `ElmdbError` rather than crash. The TypeScript types are generated from the Rust ones in the build, so a mismatch
fails before a test runs.

## Platforms

macOS (arm64, x64), Linux (x64, arm64; glibc and musl) and Windows x64, the targets napi-rs builds. The model and crash
harnesses run on macOS, Linux and Windows.
