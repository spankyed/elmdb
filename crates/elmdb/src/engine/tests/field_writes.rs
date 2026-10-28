//! What one field write reads of the store, counted (Phase 7 of `docs/goals/goal-elmdb-next.md`): a field no index
//! covers, a field a value index covers, a field a text index reads, each inside a unit with a savepoint open, where
//! the undo log records what a write replaces. The counts are a write's cost the compiler can't show; one more read in
//! a field write fails the case naming it.

use serde_json::json;

use crate::engine::entities;
use crate::engine::unit::{Txn, UnitState};
use crate::schema::Schema;
use crate::{Database, OpenOptions};

fn schema() -> Schema {
    Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "types": { "Task": {
            "fields": {
                "note": { "type": "string", "optional": true },
                "status": { "type": "string", "optional": true },
                "body": { "type": "string", "optional": true }
            },
            "indexes": {
                "status": { "kind": "value", "fields": ["status"] },
                "text": { "kind": "text", "fields": { "body": {} } }
            }
        } }
    }))
    .unwrap()
}

/// The reads one `put_field` of `field` makes on an entity that holds every field already, in a unit with a savepoint
fn reads_of(field: &str, value: &str) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let schema = schema();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema.clone()), syncs: false, ..Default::default() })
            .unwrap();
    super::write(
        &db,
        vec![json!({ "op": "create", "type": "Task", "id": "Task-a",
        "fields": { "note": "n", "status": "open", "body": "some words here" } })],
    )
    .unwrap();
    let mut reads = 0;
    db.store()
        .unwrap()
        .write(
            |txn, store| {
                let mut state = UnitState::new(false);
                state.savepoint();
                let mut t = Txn { txn, db: store, state: &mut state, schema: &schema, now: 0.0 };
                entities::put_field(&mut t, "Task-a", field, &value.into())?;
                reads = state.reads.get();
                Ok(())
            },
            false,
        )
        .unwrap();
    reads
}

#[test]
fn a_field_write_reads_what_it_replaces_and_what_its_indexes_need_and_nothing_twice() {
    // A field no index covers: what it held, for the write and the undo log both
    assert_eq!(reads_of("note", "changed"), 1);
    // A value index's: that, the entity's place, and the undo log's reads of the index entry it removes and the one it adds
    assert_eq!(reads_of("status", "done"), 4);
    // A text index's: that, the place, the stored document and statistics; for the two terms it loses and the two it
    // gains, their postings and positions (a lost term's positions read by the undo log); and the kept term's positions
    assert_eq!(reads_of("body", "other words entirely"), 13);
}
