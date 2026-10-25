# Phase 7: admin, the upgrade and the CLI

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Rust 1.99.1, a release build.

## What was built

- **Maintenance** (`engine/maintenance.rs`): `backup` (compacted), `replaceWith`, `reset`, `stats`, `bulkLoad`.
- **The upgrade** (`upgrade.rs`): a format 4 dump read into a new database.
- **The CLI** (`crates/elmdb-cli`): `elmdb inspect`, `check` (exit 1 when it finds anything), `backup`, `upgrade`.
- **The binding**: every admin operation, its reports typed from the generated protocol.

## The sequence that aborted lmdb-js

`a_compacted_backup_of_multi_page_values_can_be_emptied_in_one_transaction` writes 1,000 entities of 20,000-byte
values into the `elmdb` database (multi-page on 16 KiB pages), takes a compacted backup, opens it and empties it in one
transaction, then writes to it and checks it. It passes. The tests build LMDB without `NDEBUG` (`lmdb-master-sys`
defines it only outside debug builds), so the assertion lmdb-js tripped (`mdb_page_alloc`'s `key.mv_size > 0`) would
abort the test process here too; stock LMDB has no reverse free-list scan to trip it
(`docs/background.md`).

## The real data

The production backup of 2026-10-17, converted to format 4 with apack's `scripts/ears-convert.ts` and dumped with
`tools/lmdb-js-export` (Phase 1), upgraded with `elmdb upgrade <dump> <dir>` (no schema):

| | Read from the dump | apack's own reading of the original (Phase 1's conversion) |
|---|---|---|
| entities | 22,816 | 22,816 |
| attributes | 178,131 | 178,131 |
| relations | 45,798 | 45,798 |

The dump's other 160,224 entries were format 4's derived keys (`b`, `c`, `d`, `e`, `n`, `o`), rebuilt rather than
copied, and its one sequence, the store's. The upgrade took 2.70 s; the database is 289 MB, which a compacted backup
reduces.

`elmdb check` on it: **no difference in derived data**, and **17 violations, all duplicate short codes the data
holds**: 8 `Note`s sharing a `NOTE-<n>` code pairwise and 9 groups of `Thread`s sharing a `T-<n>` code (one group of
three). Both prefixes are older than the ones apack declares today (`THR` for threads), so these predate apack's
`SHORT_CODE_IN_USE` refusal; the upgrade copies values as they are and doesn't rewrite them (D12). Which code each of
those entities keeps is the user's decision, for the conversion of the real data.

Two things the real data showed that the design hadn't:

- **Format 4's own rows hold msgpackr's `undefined`** (extension type 0) for a relation's unset `info`. Phase 1's
  census read only attribute values. The upgrade reads format 4's rows leniently; attribute values stay held to the
  value spec. apack's facade must encode with msgpackr's `skipValues: [undefined]`, or a nested `undefined` would reach
  elmdb as an extension and be refused (Phase 9).
- **Short codes aren't unique in the data**, so a load takes duplicates and `check()` reports them.

## Checks

`cargo test --workspace` (119 passed, 1 measurement ignored, and the CLI's spec), `cargo clippy -- -D warnings`,
`cargo fmt --check`, `cargo xtask check-generated`, `cargo xtask check-one-lmdb`, the binding's `npm run typecheck` and
`npm test` (11 passed).
