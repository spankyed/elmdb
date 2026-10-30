# Own-unit vector visibility, 2026-10-30

Base: `4d558a27543a74e2ea4c131aff1cb32455708994`. Before this change, the primary embedding and
journal were updated within a unit but nearest-vector search consulted only the graph built at the last commit.
It could return the previous nearest owner after replacement, or no owner before the first graph build.

Reads now suppress journaled graph items and score their current primary embeddings with hannoy's own distance
implementation. Scoped graph candidates exclude journaled items, including newly allocated items not present in
the graph yet. Unscoped searches request extra graph candidates so stale versions cannot hide unchanged neighbors.
Graph writes remain deferred until the outer commit; savepoints undo primary data and journal changes together.
No format, binding or protocol change is introduced. Additional scoring visits pending changes, not the full index.

The regression exercises cosine and L2, first uncommitted embeddings, committed-graph replacement, scoped newly
allocated items, nested savepoint rollback, outer rollback, deletion/abort and committed results. All 23 vector
cases pass. The original code restored in a disposable copy fails that regression at the first uncommitted read
(expected `Doc-a`, got no owner). The tested source is unchanged by the mutation.

All required checks pass in disposable Linux ARM64, Rust 1.99.1 / Node 23.11.1, capped at two CPUs and 6 GiB with
no swap and all capabilities dropped. No Mac compilation, shared target or working-agent checkout was used:

- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings`.
- `cargo nextest run --workspace --profile ci`: 348 passed, ten existing ignored helper/opt-in cases skipped.
- `cargo test --workspace --doc`, `cargo xtask check-generated` and `cargo xtask check-one-lmdb`.
- `npm ci`, `npm run build`, `npm run typecheck` and `npm test`: 33 passed, four macOS semaphore cases skipped.

`checks.json` preserves commands, exits and elapsed times; `mutation.json` records the negative proof. Its patch
SHA-256 names the two Rust files as tested, before documentation was added and the existing nearest-function rustdoc was moved back to that function.
That final adjustment changes comments only. Full logs are retained in the isolated
session at `/private/tmp/ears-native-full-check-results` and `/private/tmp/ears-native-overlay-mutation-results`.
Cross-platform CI and the consuming app's original regression are separate qualification steps.
