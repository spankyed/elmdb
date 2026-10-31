//! The write protocol's rules, case by case: ported from apack's `tests/engine/{writes,relations,transact}.spec.ts`,
//! each translated to batches. What reads answer (queries, walks) is Phase 5's; these read the store back directly.

mod commits;
mod compression;
mod derived;
mod field_writes;
mod graph;
mod lifecycle;
mod maintenance;
mod model;
mod plans;
mod rank_index;
mod ranks;
mod reads;
mod schema_versions;
mod tracked;
mod trigram;
mod walks;
mod weights;

use serde_json::{Value as Json, json};

use super::batch::{Batch, Op, OpResult};
use super::changes::ChangeSet;
use super::unit::Read;
use super::{entities, relations, values};
use crate::error::ErrorCode;
use crate::schema::Schema;
use crate::{Database, OpenOptions};

pub(super) fn schema() -> Schema {
    Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "roles": { "r1": {}, "r2": {} },
        "relations": {
            "refs": { "cardinality": "many-to-many" },
            "blocks": { "cardinality": "many-to-many" },
            "parent_of": { "cardinality": "one-to-many", "acyclic": true },
            "tags": { "cardinality": "many-to-many", "unique": true },
            "pins": { "cardinality": "many-to-one", "endpoints": [{ "source": "Note", "target": "Task" }] },
            "weighs": { "cardinality": "many-to-many", "weighted": true },
            "rates": { "cardinality": "many-to-many", "weighted": true, "unique": true }
        },
        "types": {
            "Task": {
                "shortCode": { "prefix": "TSK" },
                "fields": {
                    "a": { "type": "any", "optional": true },
                    "b": { "type": "any", "optional": true },
                    "c": { "type": "any", "optional": true },
                    "title": { "type": "string", "optional": true },
                    "label": { "type": "string", "optional": true },
                    "slug": { "type": "string", "optional": true }
                },
                "indexes": {
                    "a": { "kind": "value", "fields": ["a"] },
                    "slug": { "kind": "value", "fields": ["slug"], "unique": true }
                }
            },
            "Note": {
                "fields": {
                    "a": { "type": "any", "optional": true },
                    "b": { "type": "any", "optional": true },
                    "c": { "type": "any", "optional": true },
                    "tags": { "type": "list", "of": { "type": "string" }, "optional": true },
                    "status": { "type": "enum", "values": ["open", "done"], "optional": true, "nullable": true }
                },
                "indexes": {
                    "text": { "kind": "text", "fields": { "a": { "weight": 2 }, "b": {} } },
                    "grams": { "kind": "trigram", "fields": ["a", "b"] }
                }
            },
            "Doc": { "fields": { "title": { "type": "string" }, "body": { "type": "string", "optional": true } } },
            "Bag": { "additionalFields": "allow", "fields": {} }
        }
    }))
    .unwrap()
}

pub(super) fn open(dir: &tempfile::TempDir) -> Database {
    Database::open(dir.path(), OpenOptions { schema: Some(schema()), syncs: false, ..Default::default() }).unwrap()
}

pub(super) fn op(json: Json) -> Op {
    serde_json::from_value(json).unwrap()
}

/// The batch of these ops, at a fixed time
pub(super) fn batch(ops: Vec<Json>) -> Batch {
    Batch { ops: ops.into_iter().map(op).collect(), now: Some(1000.0) }
}

pub(super) fn write(db: &Database, ops: Vec<Json>) -> crate::Result<Vec<OpResult>> {
    db.write(&batch(ops), false).map(|(r, _)| r.results)
}

fn write_changes(db: &Database, ops: Vec<Json>) -> ChangeSet {
    db.write(&batch(ops), false).unwrap().1
}

pub(super) fn refused(db: &Database, ops: Vec<Json>) -> crate::Error {
    write(db, ops).expect_err("the batch was refused")
}

pub(super) fn created(results: &[OpResult]) -> String {
    match &results[0] {
        OpResult::Entity { id, .. } => id.clone(),
        other => panic!("not an entity: {other:?}"),
    }
}

fn to_json(value: &rmpv::Value) -> Json {
    serde_json::to_value(value).unwrap()
}

/// The entity's row as stored, its fields as JSON
pub(super) fn row(db: &Database, id: &str) -> Option<Json> {
    db.snapshot(|s| {
        if entities::record(s, id)?.is_none() {
            return Ok(None);
        }
        let mut row = serde_json::Map::new();
        for (field, bytes) in entities::fields(s, id)? {
            row.insert(field, to_json(&values::decode(&bytes)?));
        }
        Ok(Some(Json::Object(row)))
    })
    .unwrap()
}

/// Each edge as `source|kind|target`, sorted
pub(super) fn edges(db: &Database) -> Vec<String> {
    db.snapshot(|s| {
        let mut out = Vec::new();
        for (_, bytes) in s.scan(&super::keys::all_relations())? {
            let stored: relations::Stored = rmp_serde::from_slice(&bytes).unwrap();
            out.push(format!("{}|{}|{}", stored.source, stored.kind, stored.target));
        }
        out.sort();
        Ok(out)
    })
    .unwrap()
}

#[test]
fn creates_an_entity_with_a_new_id_its_times_and_a_short_code() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let results = write(&db, vec![json!({ "op": "create", "type": "Task", "fields": { "a": 1 } })]).unwrap();
    let id = created(&results);
    assert!(id.starts_with("Task-"));
    assert_eq!(results[0], OpResult::Entity { id: id.clone(), short_code: Some("TSK-1".into()), created: true });
    assert_eq!(row(&db, &id).unwrap(), json!({ "a": 1, "createdAt": 1000, "shortCode": "TSK-1", "updatedAt": 1000 }));
    let other = created(&write(&db, vec![json!({ "op": "create", "type": "Task" })]).unwrap());
    assert_ne!(id, other, "each entity has its own id");
}

#[test]
fn takes_the_id_and_times_a_row_brings() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let results =
        write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-given", "createdAt": 5, "updatedAt": 7 })])
            .unwrap();
    assert_eq!(created(&results), "Task-given");
    let r = row(&db, "Task-given").unwrap();
    assert_eq!((r["createdAt"].clone(), r["updatedAt"].clone()), (json!(5), json!(7)));
}

#[test]
fn refuses_the_engines_fields_and_types_an_unknown_type_an_id_in_use_and_one_naming_another_type() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    for (fields, field) in [(json!({ "role": ["r1"] }), "role"), (json!({ "trashedAt": 1 }), "trashedAt")] {
        let e = refused(&db, vec![json!({ "op": "create", "type": "Task", "fields": fields })]);
        assert_eq!(e.code, ErrorCode::ReservedField, "{field}");
    }
    assert_eq!(refused(&db, vec![json!({ "op": "create", "type": "Relation" })]).code, ErrorCode::ReservedType);
    let unknown = refused(&db, vec![json!({ "op": "create", "type": "Task-1" })]);
    assert_eq!(unknown.code, ErrorCode::UnknownType);
    assert!(unknown.message.contains("It looks like an id"), "{}", unknown.message);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })]).unwrap();
    assert_eq!(refused(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })]).code, ErrorCode::IdInUse);
    assert_eq!(
        refused(&db, vec![json!({ "op": "create", "type": "Task", "id": "Note-1" })]).code,
        ErrorCode::TypeMismatch
    );
    // Nothing of the refused creates was written
    assert_eq!(db.snapshot(|s| entities::count(s, "Task")).unwrap(), 1);
}

#[test]
fn numbers_short_codes_keeps_one_a_row_brings_and_refuses_one_malformed_taken_or_unnumbered() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-kept", "shortCode": "TSK-7" })]).unwrap();
    assert_eq!(row(&db, "Task-kept").unwrap()["shortCode"], "TSK-7");
    let next = write(&db, vec![json!({ "op": "create", "type": "Task" })]).unwrap();
    assert!(
        matches!(&next[0], OpResult::Entity { short_code: Some(c), .. } if c == "TSK-8"),
        "after the highest held: {next:?}"
    );
    assert_eq!(
        refused(&db, vec![json!({ "op": "create", "type": "Task", "shortCode": "TSK-x" })]).code,
        ErrorCode::InvalidShortCode
    );
    assert_eq!(
        refused(&db, vec![json!({ "op": "create", "type": "Task", "shortCode": "TSK-7" })]).code,
        ErrorCode::ShortCodeInUse
    );
    assert_eq!(
        refused(&db, vec![json!({ "op": "create", "type": "Note", "shortCode": "NTE-1" })]).code,
        ErrorCode::InvalidShortCode
    );
}

#[test]
fn never_hands_a_short_code_out_twice_and_takes_none_for_a_refused_create() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let first = created(&write(&db, vec![json!({ "op": "create", "type": "Task" })]).unwrap());
    write(&db, vec![json!({ "op": "destroy", "id": first })]).unwrap();
    // A refused batch gives back what it took
    refused(&db, vec![json!({ "op": "create", "type": "Task" }), json!({ "op": "create", "type": "Nope" })]);
    let second = write(&db, vec![json!({ "op": "create", "type": "Task" })]).unwrap();
    assert!(matches!(&second[0], OpResult::Entity { short_code: Some(c), .. } if c == "TSK-2"), "{second:?}");
}

#[test]
fn numbers_labels_per_type_and_base() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Task", "fields": { "label": "Untitled 3" } })]).unwrap();
    let labels = write(
        &db,
        vec![
            json!({ "op": "takeLabel", "type": "Task", "base": "Untitled" }),
            json!({ "op": "takeLabel", "type": "Task", "base": "Untitled" }),
            json!({ "op": "takeLabel", "type": "Task", "base": "Draft" }),
        ],
    )
    .unwrap();
    let names: Vec<&str> = labels
        .iter()
        .map(|r| match r {
            OpResult::Label { label } => label.as_str(),
            _ => "",
        })
        .collect();
    assert_eq!(names, ["Untitled 4", "Untitled 5", "Draft 1"]);
}

#[test]
fn a_number_past_javascript_s_largest_is_no_number_and_a_numbering_that_reaches_it_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let label = |r: &OpResult| match r {
        OpResult::Label { label } => label.clone(),
        other => panic!("not a label: {other:?}"),
    };
    // A label past the largest number is a label, not a numbered one
    write(&db, vec![json!({ "op": "create", "type": "Task", "fields": { "label": "Untitled 18446744073709551615" } })])
        .unwrap();
    let taken = write(&db, vec![json!({ "op": "takeLabel", "type": "Task", "base": "Untitled" })]).unwrap();
    assert_eq!(label(&taken[0]), "Untitled 1");
    // The largest number is taken; the one after it isn't handed out
    write(&db, vec![json!({ "op": "create", "type": "Task", "fields": { "label": "Draft 9007199254740991" } })])
        .unwrap();
    let e = refused(&db, vec![json!({ "op": "takeLabel", "type": "Task", "base": "Draft" })]);
    assert_eq!(e.code, ErrorCode::NumbersExhausted, "{}", e.message);
    let e = refused(&db, vec![json!({ "op": "create", "type": "Task", "shortCode": "TSK-9007199254740992" })]);
    assert_eq!(e.code, ErrorCode::InvalidShortCode, "{}", e.message);
    write(&db, vec![json!({ "op": "create", "type": "Task", "shortCode": "TSK-9007199254740991" })]).unwrap();
    // A sequence loaded past the largest number
    db.admin_load(&[crate::engine::maintenance::Load::Sequence { name: "shortCode:Task".into(), next: u64::MAX }])
        .unwrap();
    let e = refused(&db, vec![json!({ "op": "create", "type": "Task" })]);
    assert_eq!(e.code, ErrorCode::NumbersExhausted, "{}", e.message);
}

#[test]
fn keeps_a_short_code_once_written_and_an_upsert_may_only_repeat_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })]).unwrap();
    assert_eq!(
        refused(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "shortCode": "TSK-9" } })]).code,
        ErrorCode::ReservedField
    );
    assert_eq!(
        refused(&db, vec![json!({ "op": "delete", "id": "Task-1", "field": "shortCode" })]).code,
        ErrorCode::ReservedField
    );
    write(
        &db,
        vec![json!({ "op": "upsert", "type": "Task", "id": "Task-1", "shortCode": "TSK-1", "fields": { "a": 2 } })],
    )
    .unwrap();
    assert_eq!(
        refused(&db, vec![json!({ "op": "upsert", "type": "Task", "id": "Task-1", "shortCode": "TSK-2" })]).code,
        ErrorCode::InvalidShortCode
    );
}

#[test]
fn writes_to_an_existing_entity_stamping_updated_at_and_refuses_an_id_no_entity_has() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1", "createdAt": 5 })]).unwrap();
    write(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "a": 1, "b": "x" } })]).unwrap();
    let r = row(&db, "Task-1").unwrap();
    assert_eq!((r["createdAt"].clone(), r["updatedAt"].clone(), r["a"].clone()), (json!(5), json!(1000), json!(1)));
    let e = refused(&db, vec![json!({ "op": "set", "id": "Task-none", "fields": { "a": 1 } })]);
    assert_eq!(e.code, ErrorCode::NoSuchEntity);
    assert!(row(&db, "Task-none").is_none());
}

#[test]
fn refuses_a_row_without_a_required_field_and_deleting_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let e = refused(&db, vec![json!({ "op": "create", "type": "Doc", "fields": { "body": "b" } })]);
    assert_eq!(e.code, ErrorCode::RequiredField);
    assert!(e.message.contains("\"title\""), "{}", e.message);
    write(&db, vec![json!({ "op": "create", "type": "Doc", "id": "Doc-1", "fields": { "title": "t" } })]).unwrap();
    assert_eq!(
        refused(&db, vec![json!({ "op": "delete", "id": "Doc-1", "field": "title" })]).code,
        ErrorCode::RequiredField
    );
    write(&db, vec![json!({ "op": "set", "id": "Doc-1", "fields": { "title": "replaced" } })]).unwrap();
}

#[test]
fn refuses_a_value_its_field_doesnt_take_and_a_field_its_type_doesnt_declare() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    assert_eq!(
        refused(&db, vec![json!({ "op": "create", "type": "Doc", "fields": { "title": 3 } })]).code,
        ErrorCode::FieldType
    );
    assert_eq!(
        refused(&db, vec![json!({ "op": "create", "type": "Note", "fields": { "status": "late" } })]).code,
        ErrorCode::FieldType
    );
    write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "status": null } })]).unwrap();
    assert_eq!(
        refused(&db, vec![json!({ "op": "create", "type": "Note", "fields": { "z": 1 } })]).code,
        ErrorCode::UnknownField
    );
    // A type that takes fields it doesn't declare
    write(&db, vec![json!({ "op": "create", "type": "Bag", "id": "Bag-1", "fields": { "anything": { "x": 1 }, "__proto__": 1 } })]).unwrap();
    assert_eq!(row(&db, "Bag-1").unwrap()["__proto__"], 1, "a field named __proto__ is a field like any other");
    // An append is checked as the list it makes
    assert_eq!(
        refused(&db, vec![json!({ "op": "append", "id": "Note-1", "field": "tags", "value": 3 })]).code,
        ErrorCode::FieldType
    );
}

#[test]
fn upsert_creates_with_the_id_when_none_has_it_and_writes_the_row_when_one_does() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let made =
        write(&db, vec![json!({ "op": "upsert", "type": "Task", "id": "Task-u", "fields": { "a": 1 } })]).unwrap();
    assert!(matches!(&made[0], OpResult::Entity { created: true, .. }));
    let again =
        write(&db, vec![json!({ "op": "upsert", "type": "Task", "id": "Task-u", "fields": { "b": 2 } })]).unwrap();
    assert!(matches!(&again[0], OpResult::Entity { created: false, .. }));
    let r = row(&db, "Task-u").unwrap();
    assert_eq!((r["a"].clone(), r["b"].clone()), (json!(1), json!(2)));
    assert_eq!(
        refused(&db, vec![json!({ "op": "upsert", "type": "Note", "id": "Task-u" })]).code,
        ErrorCode::TypeMismatch
    );
    assert_eq!(
        refused(&db, vec![json!({ "op": "upsert", "type": "Nope", "id": "Nope-1" })]).code,
        ErrorCode::UnknownType
    );
}

#[test]
fn list_writes_append_remove_at_remove_value_and_refuse_a_value_that_isnt_a_list() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": [1, 2, 3], "b": "x" } })])
        .unwrap();
    write(
        &db,
        vec![
            json!({ "op": "append", "id": "Task-1", "field": "a", "value": 4 }),
            json!({ "op": "removeAt", "id": "Task-1", "field": "a", "index": 0 }),
            json!({ "op": "removeAt", "id": "Task-1", "field": "a", "index": 99 }),
            json!({ "op": "removeValue", "id": "Task-1", "field": "a", "value": 3 }),
            json!({ "op": "append", "id": "Task-1", "field": "c", "value": { "k": 1, "v": "a" } }),
            json!({ "op": "append", "id": "Task-1", "field": "c", "value": { "k": 2, "v": "b" } }),
            json!({ "op": "removeValue", "id": "Task-1", "field": "c", "value": { "k": 2 } }),
        ],
    )
    .unwrap();
    let r = row(&db, "Task-1").unwrap();
    assert_eq!(r["a"], json!([2, 4]));
    assert_eq!(r["c"], json!([{ "k": 1, "v": "a" }]), "removeValue matches a map holding each of its fields");
    for op_name in ["append", "removeAt", "removeValue"] {
        let mut o = json!({ "op": op_name, "id": "Task-1", "field": "b" });
        if op_name == "removeAt" {
            o["index"] = json!(0)
        } else {
            o["value"] = json!(1)
        }
        assert_eq!(refused(&db, vec![o]).code, ErrorCode::NotAList, "{op_name}");
    }
    // A one-element list reads as a list
    write(&db, vec![json!({ "op": "append", "id": "Task-1", "field": "title2", "value": 1 })]).unwrap_err();
    write(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "b": [7] } })]).unwrap();
    assert_eq!(row(&db, "Task-1").unwrap()["b"], json!([7]));
}

#[test]
fn merge_combines_maps_and_sets_the_value_when_either_isnt_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": { "x": 1, "y": 1 }, "b": 5 } })],
    )
    .unwrap();
    write(
        &db,
        vec![
            json!({ "op": "merge", "id": "Task-1", "field": "a", "value": { "y": 2, "z": 3 } }),
            json!({ "op": "merge", "id": "Task-1", "field": "b", "value": { "w": 1 } }),
        ],
    )
    .unwrap();
    let r = row(&db, "Task-1").unwrap();
    assert_eq!(r["a"], json!({ "x": 1, "y": 2, "z": 3 }));
    assert_eq!(r["b"], json!({ "w": 1 }));
}

#[test]
fn updated_at_isnt_stamped_by_links_roles_the_trash_or_a_write_that_asked_not_to() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1", "createdAt": 5 }),
            json!({ "op": "create", "type": "Task", "id": "Task-2", "createdAt": 5 }),
        ],
    )
    .unwrap();
    write(
        &db,
        vec![
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" }),
            json!({ "op": "grant", "id": "Task-1", "role": "r1" }),
            json!({ "op": "trash", "id": "Task-1" }),
            json!({ "op": "restore", "id": "Task-1" }),
            json!({ "op": "set", "id": "Task-1", "fields": { "a": 1 }, "touch": false }),
        ],
    )
    .unwrap();
    assert_eq!(row(&db, "Task-1").unwrap()["updatedAt"], 5);
}

#[test]
fn trash_keeps_the_first_time_restore_brings_it_back_and_trashed_at_is_the_engines() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })]).unwrap();
    write(&db, vec![json!({ "op": "trash", "id": "Task-1", "at": 10 })]).unwrap();
    write(&db, vec![json!({ "op": "trash", "id": "Task-1", "at": 20 })]).unwrap();
    assert_eq!(row(&db, "Task-1").unwrap()["trashedAt"], 10);
    assert_eq!(
        db.snapshot(|s| entities::count(s, "Task")).unwrap(),
        0,
        "a trashed entity leaves its type's live count"
    );
    assert_eq!(
        refused(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "trashedAt": 1 } })]).code,
        ErrorCode::ReservedField
    );
    write(&db, vec![json!({ "op": "restore", "id": "Task-1" })]).unwrap();
    assert!(row(&db, "Task-1").unwrap().get("trashedAt").is_none());
    assert_eq!(db.snapshot(|s| entities::count(s, "Task")).unwrap(), 1);
}

#[test]
fn destroy_removes_the_entity_its_fields_its_relations_and_its_type_when_emptied() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": 1 } }),
            json!({ "op": "create", "type": "Task", "id": "Task-2" }),
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" }),
            json!({ "op": "link", "source": "Task-2", "kind": "blocks", "target": "Task-1" }),
        ],
    )
    .unwrap();
    write(&db, vec![json!({ "op": "destroy", "id": "Task-1" })]).unwrap();
    assert!(row(&db, "Task-1").is_none());
    assert_eq!(edges(&db), Vec::<String>::new());
    write(&db, vec![json!({ "op": "destroy", "id": "Task-2" })]).unwrap();
    assert_eq!(db.snapshot(|s| entities::count(s, "Task")).unwrap(), 0);
    // Every derived key went with them
    let left: Vec<String> = db
        .snapshot(|s| {
            Ok(s.scan(&crate::format::Prefix::Index.key().to_vec())?
                .into_iter()
                .map(|(k, _)| format!("{:?}", crate::format::key::decode(&k).unwrap()))
                .collect())
        })
        .unwrap();
    assert_eq!(left, Vec::<String>::new());
}

#[test]
fn link_is_idempotent_on_the_whole_edge_and_another_info_is_another_edge() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1" }),
            json!({ "op": "create", "type": "Task", "id": "Task-2" }),
        ],
    )
    .unwrap();
    let link = |info: Json| {
        let mut o = json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" });
        if !info.is_null() {
            o["info"] = info
        }
        match &write(&db, vec![o]).unwrap()[0] {
            OpResult::Relation { id, linked } => (id.clone(), *linked),
            other => panic!("{other:?}"),
        }
    };
    let (first, linked) = link(json!({ "a": 1, "b": 2 }));
    assert!(linked);
    assert_eq!(
        link(json!({ "b": 2, "a": 1 })),
        (first.clone(), false),
        "the same fields in another order are the same info"
    );
    assert_eq!(link(Json::Null), (first.clone(), false), "no info matches any edge between the ends");
    let (other, linked) = link(json!({ "a": 9 }));
    assert!(linked && other != first);
    assert_eq!(edges(&db).len(), 2);
}

#[test]
fn refuses_a_self_link_an_unknown_kind_a_pair_its_endpoints_dont_allow_and_a_link_past_its_cardinality() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1" }),
            json!({ "op": "create", "type": "Task", "id": "Task-2" }),
            json!({ "op": "create", "type": "Task", "id": "Task-3" }),
            json!({ "op": "create", "type": "Note", "id": "Note-1" }),
        ],
    )
    .unwrap();
    let link = |s: &str, k: &str, t: &str| json!({ "op": "link", "source": s, "kind": k, "target": t });
    assert_eq!(refused(&db, vec![link("Task-1", "refs", "Task-1")]).code, ErrorCode::SelfLink);
    assert_eq!(refused(&db, vec![link("Task-1", "follows", "Task-2")]).code, ErrorCode::UnknownRelationKind);
    assert_eq!(refused(&db, vec![link("Task-1", "pins", "Task-2")]).code, ErrorCode::Endpoint);
    write(&db, vec![link("Note-1", "pins", "Task-1")]).unwrap();
    assert_eq!(
        refused(&db, vec![link("Note-1", "pins", "Task-2")]).code,
        ErrorCode::Cardinality,
        "a pins source has one target"
    );
    write(&db, vec![link("Task-1", "parent_of", "Task-2")]).unwrap();
    assert_eq!(
        refused(&db, vec![link("Task-3", "parent_of", "Task-2")]).code,
        ErrorCode::Cardinality,
        "a child has one parent"
    );
    assert_eq!(edges(&db).len(), 2, "nothing of the refusals was written");
}

#[test]
fn an_acyclic_kind_refuses_a_link_closing_a_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let ids = ["Task-1", "Task-2", "Task-3"];
    write(&db, ids.iter().map(|id| json!({ "op": "create", "type": "Task", "id": id })).collect()).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "link", "source": "Task-1", "kind": "parent_of", "target": "Task-2" }),
            json!({ "op": "link", "source": "Task-2", "kind": "parent_of", "target": "Task-3" }),
        ],
    )
    .unwrap();
    let e = refused(&db, vec![json!({ "op": "link", "source": "Task-3", "kind": "parent_of", "target": "Task-1" })]);
    assert_eq!(e.code, ErrorCode::Cycle);
    assert!(e.message.contains("would form a cycle"), "{}", e.message);
}

#[test]
fn a_unique_kind_keeps_one_edge_per_pair_whose_info_a_link_replaces() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1" }),
            json!({ "op": "create", "type": "Task", "id": "Task-2" }),
        ],
    )
    .unwrap();
    let link =
        |info: Json| json!({ "op": "link", "source": "Task-1", "kind": "tags", "target": "Task-2", "info": info });
    let first = write(&db, vec![link(json!(1))]).unwrap();
    let second = write(&db, vec![link(json!(2))]).unwrap();
    let id = |r: &OpResult| match r {
        OpResult::Relation { id, .. } => id.clone(),
        _ => String::new(),
    };
    assert_eq!(id(&first[0]), id(&second[0]));
    assert_eq!(edges(&db).len(), 1);
    let info = db.snapshot(|s| Ok(relations::get(s, &id(&first[0]))?.unwrap().info)).unwrap();
    assert_eq!(info, Some(rmpv::Value::from(2)));
}

#[test]
fn a_unique_kind_refuses_moving_an_edge_onto_a_pair_another_joins() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let ids = ["Task-1", "Task-2", "Task-3"];
    write(&db, ids.iter().map(|id| json!({ "op": "create", "type": "Task", "id": id })).collect()).unwrap();
    let results = write(
        &db,
        vec![
            json!({ "op": "link", "source": "Task-1", "kind": "tags", "target": "Task-2" }),
            json!({ "op": "link", "source": "Task-1", "kind": "tags", "target": "Task-3" }),
        ],
    )
    .unwrap();
    let OpResult::Relation { id: second, .. } = &results[1] else { panic!("{results:?}") };
    let e = refused(&db, vec![json!({ "op": "updateRelation", "relation": second, "target": "Task-2" })]);
    assert_eq!(e.code, ErrorCode::DuplicateEdge, "{}", e.message);
    assert_eq!(edges(&db), ["Task-1|tags|Task-2", "Task-1|tags|Task-3"]);
    // Its own pair is no other edge's: a move away and an info change on it are allowed
    write(&db, vec![json!({ "op": "updateRelation", "relation": second, "source": "Task-2", "target": "Task-1" })])
        .unwrap();
    assert_eq!(edges(&db), ["Task-1|tags|Task-2", "Task-2|tags|Task-1"]);
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn cardinality_counts_far_ends_so_check_agrees_with_the_writes_it_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let ids = ["Task-1", "Task-2", "Task-3"];
    write(&db, ids.iter().map(|id| json!({ "op": "create", "type": "Task", "id": id })).collect()).unwrap();
    // A one-to-many kind that isn't unique: a second edge to the same child leaves it one parent
    write(
        &db,
        vec![
            json!({ "op": "link", "source": "Task-1", "kind": "parent_of", "target": "Task-2", "info": 1 }),
            json!({ "op": "link", "source": "Task-1", "kind": "parent_of", "target": "Task-2", "info": 2 }),
        ],
    )
    .unwrap();
    assert_eq!(edges(&db), ["Task-1|parent_of|Task-2", "Task-1|parent_of|Task-2"]);
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
    // A second parent is past it, at a write and in the report of rows loaded without the write's checks
    let e = refused(&db, vec![json!({ "op": "link", "source": "Task-3", "kind": "parent_of", "target": "Task-2" })]);
    assert_eq!(e.code, ErrorCode::Cardinality);
    db.admin_load(&[crate::engine::maintenance::Load::Relation {
        id: "Relation-loaded".into(),
        kind: "parent_of".into(),
        source: "Task-3".into(),
        target: "Task-2".into(),
        info: None,
        weight: None,
    }])
    .unwrap();
    let report = db.check().unwrap();
    assert!(
        report.violations.iter().any(|v| v.subject == "Task-2" && v.message.contains("2 parent_of sources")),
        "{report:?}"
    );
}

#[test]
fn unlink_and_remove_relation_remove_edges_and_their_keys_update_relation_moves_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let ids = ["Task-1", "Task-2", "Task-3"];
    write(&db, ids.iter().map(|id| json!({ "op": "create", "type": "Task", "id": id })).collect()).unwrap();
    let results = write(
        &db,
        vec![
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2", "info": 1 }),
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2", "info": 2 }),
            json!({ "op": "link", "source": "Task-1", "kind": "blocks", "target": "Task-3" }),
        ],
    )
    .unwrap();
    let rel = |i: usize| match &results[i] {
        OpResult::Relation { id, .. } => id.clone(),
        _ => String::new(),
    };
    write(&db, vec![json!({ "op": "removeRelation", "relation": rel(0) })]).unwrap();
    assert_eq!(edges(&db), ["Task-1|blocks|Task-3", "Task-1|refs|Task-2"]);
    assert_eq!(
        refused(&db, vec![json!({ "op": "removeRelation", "relation": rel(0) })]).code,
        ErrorCode::NoSuchRelation
    );
    write(&db, vec![json!({ "op": "updateRelation", "relation": rel(1), "target": "Task-3" })]).unwrap();
    assert_eq!(edges(&db), ["Task-1|blocks|Task-3", "Task-1|refs|Task-3"]);
    assert_eq!(
        refused(&db, vec![json!({ "op": "updateRelation", "relation": "Relation-x", "target": "Task-3" })]).code,
        ErrorCode::NoSuchRelation
    );
    write(&db, vec![json!({ "op": "unlink", "source": "Task-1", "kind": "refs" })]).unwrap();
    assert_eq!(edges(&db), ["Task-1|blocks|Task-3"]);
    write(&db, vec![json!({ "op": "unlink", "source": "Task-1" })]).unwrap();
    assert_eq!(edges(&db), Vec::<String>::new());
    let derived = db.snapshot(|s| {
        let mut n = 0;
        for p in [crate::format::Prefix::Out, crate::format::Prefix::In, crate::format::Prefix::Between] {
            n += s.scan(&p.key().to_vec())?.len();
        }
        Ok(n)
    });
    assert_eq!(derived.unwrap(), 0, "no edge key outlives its relation");
}

#[test]
fn a_link_target_must_exist_at_commit_and_may_be_created_later_in_the_unit() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })]).unwrap();
    let e = refused(&db, vec![json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-later" })]);
    assert_eq!(e.code, ErrorCode::NoSuchEntity);
    assert_eq!(edges(&db), Vec::<String>::new());
    // A forward reference: the target arrives later in the same batch
    write(
        &db,
        vec![
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-later" }),
            json!({ "op": "create", "type": "Task", "id": "Task-later" }),
        ],
    )
    .unwrap();
    assert_eq!(edges(&db), ["Task-1|refs|Task-later"]);
    // A target destroyed later in the unit takes the link with it, which is no error
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-gone" }),
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-gone" }),
            json!({ "op": "destroy", "id": "Task-gone" }),
        ],
    )
    .unwrap();
    assert_eq!(edges(&db), ["Task-1|refs|Task-later"]);
}

#[test]
fn roles_grant_revoke_and_grant_exclusively_and_refuse_an_undeclared_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let ids = ["Task-1", "Task-2"];
    write(&db, ids.iter().map(|id| json!({ "op": "create", "type": "Task", "id": id })).collect()).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "grant", "id": "Task-1", "role": "r1" }),
            json!({ "op": "grant", "id": "Task-1", "role": "r1" }),
            json!({ "op": "grant", "id": "Task-1", "role": "r2" }),
            json!({ "op": "grantExclusive", "id": "Task-2", "role": "r1", "among": ["Task-1", "Task-2"] }),
        ],
    )
    .unwrap();
    assert_eq!(row(&db, "Task-1").unwrap()["role"], json!(["r2"]));
    assert_eq!(row(&db, "Task-2").unwrap()["role"], json!(["r1"]));
    write(&db, vec![json!({ "op": "revoke", "id": "Task-2", "role": "r1" })]).unwrap();
    assert!(row(&db, "Task-2").unwrap().get("role").is_none(), "an entity with no role holds no role field");
    assert_eq!(
        refused(&db, vec![json!({ "op": "grant", "id": "Task-1", "role": "admin" })]).code,
        ErrorCode::UnknownRole
    );
}

#[test]
fn a_unique_index_refuses_a_second_entity_with_its_value() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "slug": "a" } })]).unwrap();
    assert_eq!(
        refused(&db, vec![json!({ "op": "create", "type": "Task", "fields": { "slug": "a" } })]).code,
        ErrorCode::Unique
    );
    write(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "slug": "a" } })]).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "set", "id": "Task-1", "fields": { "slug": "b" } }),
            json!({ "op": "create", "type": "Task", "fields": { "slug": "a" } }),
        ],
    )
    .unwrap();
}

#[test]
fn a_unique_index_refuses_a_value_it_cant_hold_that_another_entity_holds() {
    let dir = tempfile::tempdir().unwrap();
    let mut schema = serde_json::to_value(schema()).unwrap();
    schema["types"]["Task"]["indexes"]["uniqueA"] = json!({ "kind": "value", "fields": ["a"], "unique": true });
    let db = Database::open(
        dir.path(),
        OpenOptions { schema: Some(Schema::from_json(&schema).unwrap()), syncs: false, ..Default::default() },
    )
    .unwrap();
    // A string past what an index holds, and a map, which no index holds: found by walking the type's rows
    let long = "x".repeat(300);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "slug": long } }),
            json!({ "op": "create", "type": "Task", "id": "Task-2", "fields": { "a": { "k": 1 } } }),
        ],
    )
    .unwrap();
    let e = refused(&db, vec![json!({ "op": "create", "type": "Task", "fields": { "slug": long } })]);
    assert_eq!(e.code, ErrorCode::Unique, "{}", e.message);
    assert!(e.message.contains("Task-1"), "{}", e.message);
    let e = refused(&db, vec![json!({ "op": "create", "type": "Task", "fields": { "a": { "k": 1 } } })]);
    assert_eq!(e.code, ErrorCode::Unique, "{}", e.message);
    // A list holding one value only a walk could find and nobody holds, then one the index finds another entity
    // holding: the index's holder is the refusal, whatever the walk finds
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-3", "fields": { "a": "held" } })]).unwrap();
    let e = refused(&db, vec![json!({ "op": "create", "type": "Task", "fields": { "a": ["z".repeat(300), "held"] } })]);
    assert_eq!(e.code, ErrorCode::Unique, "{}", e.message);
    assert!(e.message.contains("Task-3"), "{}", e.message);
    // Another such value, and the holder writing its own again, are allowed
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "fields": { "slug": format!("{long}y"), "a": { "k": 2 } } }),
            json!({ "op": "set", "id": "Task-2", "fields": { "a": { "k": 1 } } }),
        ],
    )
    .unwrap();
}

#[test]
fn a_change_set_says_what_a_commit_changed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let changes = write_changes(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": 1 } }),
            json!({ "op": "create", "type": "Note", "id": "Note-1" }),
            json!({ "op": "link", "source": "Note-1", "kind": "refs", "target": "Task-1" }),
        ],
    );
    assert_eq!(changes.types.iter().collect::<Vec<_>>(), ["Note", "Task"]);
    assert!(changes.entities["Task-1"].created);
    assert!(changes.entities["Task-1"].fields.contains("a"));
    assert_eq!(changes.kinds.iter().collect::<Vec<_>>(), ["refs"]);
    let changes = write_changes(&db, vec![json!({ "op": "trash", "id": "Task-1" })]);
    assert!(changes.entities["Task-1"].trashed && !changes.entities["Task-1"].created);
}

mod units {
    use super::*;

    #[test]
    fn a_unit_reads_its_own_writes_and_commits_them_together() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let mut unit = db.begin().unwrap();
        unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })])).unwrap();
        unit.write(&batch(vec![json!({ "op": "set", "id": "Task-1", "fields": { "a": 1 } })])).unwrap();
        assert!(row(&db, "Task-1").is_none(), "a reader sees none of a unit until it commits");
        assert_eq!(db.write(&batch(vec![]), false).unwrap_err().code, ErrorCode::UnitOpen);
        assert_eq!(db.begin().err().unwrap().code, ErrorCode::UnitOpen);
        let changes = unit.commit(false).unwrap();
        assert!(changes.entities.contains_key("Task-1"));
        assert_eq!(row(&db, "Task-1").unwrap()["a"], 1);
        db.write(&batch(vec![]), false).unwrap();
    }

    #[test]
    fn a_refused_batch_in_a_unit_keeps_none_of_itself_and_the_unit_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let mut unit = db.begin().unwrap();
        unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })])).unwrap();
        let e = unit.write(&batch(vec![
            json!({ "op": "set", "id": "Task-1", "fields": { "a": 1 } }),
            json!({ "op": "create", "type": "Nope" }),
        ]));
        assert_eq!(e.unwrap_err().code, ErrorCode::UnknownType);
        unit.commit(false).unwrap();
        assert!(row(&db, "Task-1").unwrap().get("a").is_none());
    }

    #[test]
    fn a_savepoint_rolled_back_undoes_only_its_own_writes_a_grown_list_included() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let mut unit = db.begin().unwrap();
        unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": [1] } })]))
            .unwrap();
        let depth = unit.savepoint().unwrap();
        unit.write(&batch(vec![
            json!({ "op": "append", "id": "Task-1", "field": "a", "value": 2 }),
            json!({ "op": "create", "type": "Task", "id": "Task-2" }),
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" }),
        ]))
        .unwrap();
        unit.rollback_to(depth).unwrap();
        unit.write(&batch(vec![json!({ "op": "set", "id": "Task-1", "fields": { "b": 1 } })])).unwrap();
        let changes = unit.commit(false).unwrap();
        assert!(!changes.entities.contains_key("Task-2"), "the rolled-back savepoint's events went with it");
        let r = row(&db, "Task-1").unwrap();
        assert_eq!((r["a"].clone(), r["b"].clone()), (json!([1]), json!(1)));
        assert!(row(&db, "Task-2").is_none());
        assert_eq!(edges(&db), Vec::<String>::new());
    }

    #[test]
    fn a_released_savepoint_is_undone_with_the_unit_around_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let mut unit = db.begin().unwrap();
        let outer = unit.savepoint().unwrap();
        let inner = unit.savepoint().unwrap();
        unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })])).unwrap();
        unit.release(inner).unwrap();
        unit.rollback_to(outer).unwrap();
        unit.commit(false).unwrap();
        assert!(row(&db, "Task-1").is_none());
    }

    #[test]
    fn a_link_target_missing_when_the_unit_commits_rolls_the_whole_unit_back() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })]).unwrap();
        let mut unit = db.begin().unwrap();
        unit.write(&batch(vec![json!({ "op": "set", "id": "Task-1", "fields": { "a": 1 } })])).unwrap();
        unit.write(&batch(vec![json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-never" })]))
            .unwrap();
        assert_eq!(unit.commit(false).unwrap_err().code, ErrorCode::NoSuchEntity);
        assert!(row(&db, "Task-1").unwrap().get("a").is_none());
        db.write(&batch(vec![]), false).unwrap();
    }

    #[test]
    fn a_unit_dropped_uncommitted_keeps_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        {
            let mut unit = db.begin().unwrap();
            unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })])).unwrap();
        }
        assert!(row(&db, "Task-1").is_none());
        db.begin().unwrap().commit(false).unwrap();
    }
}
