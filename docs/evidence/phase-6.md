# Phase 6: derived data and schema changes

2026-10-25, MacBook Pro (Apple silicon), macOS 25.3, Rust 1.99.1.

## What was built

- **Derived data from primary data** (`engine/derive.rs`): what each type's ranges and count, each built index and each
  relation's edges should hold; `check()` compares that with what is stored and checks every row against the schema's
  rules; `rebuild()` deletes and rewrites it, for every index or one.
- **Index builds in the background** (`engine/builder.rs`): a thread per open database builds the unbuilt indexes in
  chunks of 1,000 entities, a unit each; plans read an index only once its marker says it is built, and walk the rows
  until then. An index of a type with no entities is marked built at open.
- **The change table at open** (`database.rs`, `engine::drop_index_data`): a dropped or rebuilt index's entries and
  marker deleted in the unit that stores the schema, and the indexes then unbuilt queued.
- **Admin operations** (`engine/admin.rs`): `purge`, `renameField`, `renameRelation`, `renameType`.

## Decided while building

- **A collation change is applied, not a rebuild**: an index holds values as written, and the collation orders
  `orderBy` as a query runs (`schema.md`).
- **`check()` doesn't walk LMDB's pages**: heed exposes no page walk. It compares derived data with primary data and
  rows with rules, and LMDB reports damage it reaches as `CORRUPTED` (`engine.md`).
- **`purge` refuses a name the schema still declares**, so it deletes only what a removed declaration left.

## The change table, row by row

| Row | Spec (`engine/tests/derived.rs`, and `schema/tests.rs` for the diff) |
|---|---|
| An index added over data | `an_index_added_over_data_builds_in_the_background_and_reads_walk_until_it_has`: 2,500 notes; the same answer before and after the build; after it, the index's 1,250 matches visited instead of the 2,500 rows |
| An index changed | `a_changed_index_is_rebuilt_and_a_removed_one_dropped` |
| An index removed | the same spec: its entries gone, `check()` clean |
| A field removed | `a_removed_field_keeps_its_data_until_purged`: the data stays, a write to it refused, `purge` deletes it, and refuses while it is declared |
| A type removed | `a_removed_type_keeps_its_entities_until_purged` |
| A rule tightened | `check_reports_rows_that_break_a_rule_and_a_reference_to_nothing`: a field made required is enforced on a create, and the row that predates it is reported, not rewritten |
| A unique index over duplicates | `a_unique_index_over_rows_holding_the_same_values_isnt_built_and_says_why` |
| A short code prefix changed | `database::tests::refuses_a_change_opening_cant_make_and_keeps_the_stored_schema` |
| Renames | `rename_field_and_rename_relation_move_data_to_a_declared_name`, `rename_type_rewrites_ids_their_relations_and_the_refs_naming_them` |

`check_reports_damaged_derived_keys_and_rebuild_repairs_them` deletes a live-range key, writes a wrong count and an
index entry nothing derives; `check()` reports each as missing, differing and extra, and after `rebuild()` it is clean.

## The model harness

Every case now ends with `check()`, which must be clean.

## Checks

`cargo test --workspace` (113 passed, 1 measurement ignored), `cargo clippy -- -D warnings`, `cargo fmt --check`,
`cargo xtask check-generated`.
