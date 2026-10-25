# Phase 0: the repository

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Rust 1.99.1, Node 23.11.1.

## The checks fail when they should

Each was run on a copy of the workspace with one deliberate break, never on the working tree.

**A Rust type changed without regenerating** (a variant added to `ErrorCode`):

```
$ cargo xtask check-generated
The generated TypeScript is stale; run `cargo xtask generate` and commit the result:
  ErrorCode.ts: changed
exit 1
```

**A second LMDB `-sys` crate** (`lmdb-master3-sys = "0.2.6"` added to `crates/elmdb`):

```
$ cargo xtask check-one-lmdb
The dependency tree holds 2 LMDB -sys crates, and a binary may link only one LMDB:
  lmdb-master-sys v0.2.6
  lmdb-master3-sys v0.2.6
exit 1
```

## The checks pass on the committed source

`cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` (6
tests), `cargo xtask check-generated`, `cargo xtask check-one-lmdb`, and the binding's `npm test` (3 tests: open and
close, `ALREADY_OPEN` on a second open, `NO_DATABASE` on a read-only open of an empty directory).
