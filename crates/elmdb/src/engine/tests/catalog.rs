//! The catalog (`docs/architecture/engine.md`, Reads): what the database holds, from what it keeps as it writes, which a
//! watch can follow because it reads one key per name.

use serde_json::{Value as Json, json};

use super::{batch, open, write};
use crate::Database;
use crate::engine::plan::{ReadOp, Reads};

fn reads(track: Option<u32>) -> Reads {
    Reads { reads: vec![serde_json::from_value::<ReadOp>(json!({ "read": "catalog" })).unwrap()], track }
}

fn catalog(db: &Database) -> Json {
    let bytes = db.read(&reads(None)).unwrap();
    serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()[0].clone()
}

fn type_of(catalog: &Json, name: &str) -> Json {
    catalog["types"].as_array().unwrap().iter().find(|t| t["name"] == name).unwrap().clone()
}

fn count_of(catalog: &Json, kind: &str) -> u64 {
    catalog["relations"].as_array().unwrap().iter().find(|r| r["kind"] == kind).unwrap()["count"].as_u64().unwrap()
}

fn holds_field(catalog: &Json, name: &str, field: &str) -> bool {
    type_of(catalog, name)["fields"].as_array().unwrap().iter().any(|f| f == field)
}

#[test]
fn it_answers_each_declared_name_with_its_live_count_and_the_fields_written() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let empty = catalog(&db);
    assert_eq!(type_of(&empty, "Task")["count"], 0);
    assert_eq!(count_of(&empty, "refs"), 0);
    assert_eq!(empty["roles"], json!(["r1", "r2"]));
    assert!(holds_field(&empty, "Task", "a"), "a declared field, before any value");
    assert!(!holds_field(&empty, "Bag", "unheard"));

    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": 1 } }),
            json!({ "op": "create", "type": "Task", "id": "Task-2", "fields": {} }),
            json!({ "op": "create", "type": "Bag", "id": "Bag-1", "fields": { "unheard": 1 } }),
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" }),
            json!({ "op": "link", "source": "Task-2", "kind": "refs", "target": "Bag-1" }),
            json!({ "op": "link", "source": "Task-1", "kind": "blocks", "target": "Bag-1" }),
        ],
    )
    .unwrap();
    let held = catalog(&db);
    assert_eq!(type_of(&held, "Task")["count"], 2);
    assert_eq!(type_of(&held, "Bag")["count"], 1);
    assert!(holds_field(&held, "Bag", "unheard"));
    assert!(!holds_field(&held, "Task", "unheard"), "a field is registered for its own type");
    assert_eq!(count_of(&held, "refs"), 2);
    assert_eq!(count_of(&held, "blocks"), 1);
    assert_eq!(count_of(&held, "tags"), 0);
}

#[test]
fn a_destroy_takes_its_relations_from_the_counts() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": {} }),
            json!({ "op": "create", "type": "Task", "id": "Task-2", "fields": {} }),
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" }),
            json!({ "op": "link", "source": "Task-2", "kind": "refs", "target": "Task-1" }),
        ],
    )
    .unwrap();
    write(&db, vec![json!({ "op": "destroy", "id": "Task-1" })]).unwrap();
    let after = catalog(&db);
    assert_eq!(count_of(&after, "refs"), 0);
    assert_eq!(type_of(&after, "Task")["count"], 1);
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_field_stays_registered_when_its_last_value_goes_until_a_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Bag", "id": "Bag-1", "fields": { "unheard": 1, "kept": 1 } })])
        .unwrap();
    write(&db, vec![json!({ "op": "delete", "id": "Bag-1", "field": "unheard" })]).unwrap();
    assert!(holds_field(&catalog(&db), "Bag", "unheard"));
    assert!(db.check().unwrap().is_clean(), "a registered name without a value is no difference");

    db.rebuild(None).unwrap();
    let rebuilt = catalog(&db);
    assert!(!holds_field(&rebuilt, "Bag", "unheard"));
    assert!(holds_field(&rebuilt, "Bag", "kept"));
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_watch_hears_a_count_moving_or_a_new_field_and_not_a_write_that_changes_neither() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Bag", "id": "Bag-1", "fields": { "unheard": 1 } }),
            json!({ "op": "create", "type": "Bag", "id": "Bag-2", "fields": {} }),
        ],
    )
    .unwrap();
    db.read(&reads(Some(1))).unwrap();
    let touched = |ops: Vec<Json>| db.write(&batch(ops), false).unwrap().1.touched;

    assert_eq!(touched(vec![json!({ "op": "set", "id": "Bag-1", "fields": { "unheard": 2 } })]), Vec::<u32>::new());
    assert_eq!(touched(vec![json!({ "op": "set", "id": "Bag-2", "fields": { "unheard": 2 } })]), Vec::<u32>::new());
    assert_eq!(touched(vec![json!({ "op": "set", "id": "Bag-1", "fields": { "another": 1 } })]), vec![1]);
    db.read(&reads(Some(1))).unwrap();
    assert_eq!(touched(vec![json!({ "op": "create", "type": "Bag", "id": "Bag-3", "fields": {} })]), vec![1]);
    db.read(&reads(Some(1))).unwrap();
    assert_eq!(touched(vec![json!({ "op": "link", "source": "Bag-1", "kind": "refs", "target": "Bag-2" })]), vec![1]);
}
