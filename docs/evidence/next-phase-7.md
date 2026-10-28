# Phase 7 of goal-elmdb-next: fewer reads per field write

2026-10-27, elmdb `c3c8021`.

## What a field write read

`entities::put_field` and `delete_field`, counted in a unit with a savepoint open (where the undo log records what a
write replaces) by a test-only counter on the unit's reads — every get, every visit and every undo read
(`crates/elmdb/src/engine/tests/field_writes.rs`), on an entity holding every field already:

| A write of | Before | After | What the reads are now |
|---|---:|---:|---|
| a field no index covers (`note`) | 2 | 1 | what the field held |
| a value-indexed field (`status`) | 5 | 4 | that; the entity's place; the undo log's reads of the index entry removed and the one added |
| a text-indexed field (`body`, three words to three, one kept) | 23 | 13 | that; the place; the stored document and statistics; each lost and gained term's postings and positions, a lost term's positions read by the undo log; the kept term's positions |

Removed, each a read of something the write had already read:

- **The undo log's read of a key the write had just read.** `Txn::put_over` and `del_over` take what the caller read,
  and the field's value, a text index's postings, positions, document and statistics hand theirs over. A key the write
  doesn't otherwise read — an index entry, a lost term's positions — the log still reads, since only it needs them.
- **The entity's record, once per index kind.** `reindex`, the text index's refile and the vector index's each read
  it; a field write now reads it once, and not at all when no index of the type covers the field (`place_for`).
- **The written field, read back.** The text and vector refiles read every field their index covers; they now take
  the value being written in place of reading it.

Not removed, each needed: the other fields of a compound index, which the write doesn't hold; a unique index's check;
and the stored document a text index compares the new one against.

The spec pins the three counts, so a read added to a field write fails the case naming it. A disposable mutation on a
copy-on-write clone, `put_field` writing with `put` again (so the undo log reads the value a second time), fails it
(`left: 2`). The whole engine suite (279 cases) passes, the model harness and the text index's `check()` and
`rebuild()` cases among them, so what a unit rolls back and what an index holds are unchanged.

## G4

apack's `npm run bench -w @apackdev/ears`, the engine over elmdb `e4ff088` (before, built in a copy-on-write clone) and
`c3c8021` (after), alternated twice through the `vendor/elmdb` link at a load of 10 to 17; each figure the lower of its
two medians, in ms:

| Case | Before | After | |
|---|---:|---:|---:|
| bulk load 50k entities, 200k attributes, 100k relations | 1292.4 | 1327.3 | +2.7% |
| 1,000 creates | 115.9 | 124.1 | +7.1% |
| transact: 1,000 creates as one unit | 26.7 | 24.8 | −7.1% |
| link 10k tasks to one project, then destroy it | 849.2 | 877.2 | +3.3% |
| 1,000 getRow | 5.03 | 5.31 | +5.6% |
| 1,000 where lookups by a unique value | 7.62 | 7.16 | −6.1% |
| 1,000 creates, 0 watches open on other rows | 113.2 | 118.0 | +4.2% |
| 1,000 creates, 10 watches open on other rows | 115.5 | 111.3 | −3.6% |

Every other case is within ±3%. None passes the 10% tolerance. The two create cases run the same writes, one a unit
each and one in a unit together, and moved by the same amount in opposite directions, which is the machine's load; a
create writes its fields through the batch's own path, so this phase's change reaches it only through a unit's undo
log, and `getRow` takes no write path at all.
