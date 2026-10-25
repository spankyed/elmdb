//! Derived data and schema changes: `check()` and `rebuild()`, an index built in the background, and each row of
//! `docs/schema.md`'s change table as an open applies it, with the admin operations a migration runs.

use serde_json::{Value as Json, json};

use super::{open, row, write};
use crate::engine::admin::Purge;
use crate::error::ErrorCode;
use crate::format::Prefix;
use crate::schema::Schema;
use crate::{Database, OpenOptions};

fn with_schema(dir: &tempfile::TempDir, schema: Json) -> crate::Result<Database> {
    let schema = Schema::from_json(&schema).unwrap();
    Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() })
}

fn notes(fields: Json, indexes: Json) -> Json {
    json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "relations": { "refs": { "cardinality": "many-to-many" }, "cites": { "cardinality": "many-to-many" } },
        "types": { "Note": { "fields": fields, "indexes": indexes }, "Tag": { "fields": { "of": { "type": "ref", "to": ["Note"], "optional": true } } } }
    })
}

fn base_fields() -> Json {
    json!({ "title": { "type": "string", "optional": true }, "kind": { "type": "string", "optional": true } })
}

fn ids_where(db: &Database, field: &str, value: Json) -> (Json, u64) {
    let plan: crate::engine::plan::Plan = serde_json::from_value(json!({
        "start": { "from": "types", "types": ["Note"] },
        "steps": [{ "step": "where", "field": field, "value": value }],
        "terminal": { "answer": "ids" }
    }))
    .unwrap();
    let (bytes, visited) = db.snapshot(|s| crate::engine::reads::run_counted(s, &plan)).unwrap();
    let answer: Json = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    (answer["ids"].clone(), visited.entities)
}

/// Writes a derived key straight into the store, as damage would
fn damage(db: &Database, f: impl Fn(&mut heed::RwTxn<'_>, crate::store::Db) -> crate::Result<()>) {
    db.store().unwrap().write(|txn, d| f(txn, d), false).unwrap();
}

#[test]
fn check_reports_damaged_derived_keys_and_rebuild_repairs_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": 1 } }),
            json!({ "op": "create", "type": "Task", "id": "Task-2", "fields": { "a": 2 } }),
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" }),
        ],
    )
    .unwrap();
    assert!(db.check().unwrap().is_clean());
    damage(&db, |txn, d| {
        // A live-range key gone, a count wrong, and an index entry nothing derives
        let live = db
            .snapshot(|s| crate::engine::unit::Read::scan(s, &crate::engine::keys::type_range(false, "Task")))
            .unwrap();
        d.delete(txn, &live[0].0)?;
        d.put(txn, &crate::engine::keys::count("Task"), &crate::engine::values::encode(&7.into()))?;
        d.put(
            txn,
            &crate::engine::keys::index_entry("Task", "a", &[crate::format::key::Part::Number(99.0)], 1),
            &crate::engine::values::encode(&"Task-1".into()),
        )?;
        Ok(())
    });
    let report = db.check().unwrap();
    let kinds: Vec<&str> = report.differences.iter().map(|d| d.kind.as_str()).collect();
    assert!(kinds.contains(&"missing") && kinds.contains(&"differs") && kinds.contains(&"extra"), "{report:?}");
    db.rebuild(None).unwrap();
    let after = db.check().unwrap();
    assert!(after.is_clean(), "{after:?}");
    // The edge keys and the relation came through the rebuild
    assert_eq!(super::edges(&db), ["Task-1|refs|Task-2"]);
    let edge_keys =
        db.snapshot(|s| Ok(crate::engine::unit::Read::scan(s, &Prefix::Out.key().to_vec())?.len())).unwrap();
    assert_eq!(edge_keys, 1);
}

#[test]
fn check_reports_rows_that_break_a_rule_and_a_reference_to_nothing() {
    let dir = tempfile::tempdir().unwrap();
    with_schema(&dir, notes(base_fields(), json!({}))).unwrap().close().unwrap();
    {
        let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
        write(
            &db,
            vec![
                json!({ "op": "create", "type": "Note", "id": "Note-1" }),
                json!({ "op": "create", "type": "Tag", "id": "Tag-1", "fields": { "of": "Note-gone" } }),
            ],
        )
        .unwrap();
        let report = db.check().unwrap();
        assert_eq!(report.violations.len(), 1, "{report:?}");
        assert!(report.violations[0].message.contains("Note-gone"));
        db.close().unwrap();
    }
    // A field made required: enforced on writes from now on, and the rows that predate it reported, not rewritten
    let mut fields = base_fields();
    fields["title"]["optional"] = json!(false);
    let db = with_schema(&dir, notes(fields, json!({}))).unwrap();
    let change = db.open_report().schema_changes.iter().find(|c| c.path == "types.Note.fields.title").unwrap();
    assert_eq!(change.effect, crate::schema::Effect::Enforced);
    assert_eq!(write(&db, vec![json!({ "op": "create", "type": "Note" })]).unwrap_err().code, ErrorCode::RequiredField);
    let report = db.check().unwrap();
    assert!(report.violations.iter().any(|v| v.subject == "Note-1" && v.message.contains("lacks title")), "{report:?}");
    assert!(row(&db, "Note-1").unwrap().get("title").is_none(), "the old row is as it was");
}

#[test]
fn an_index_added_over_data_builds_in_the_background_and_reads_walk_until_it_has() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
        let ops: Vec<Json> = (0..2500).map(|i| json!({ "op": "create", "type": "Note", "id": format!("Note-{i:04}"), "fields": { "kind": if i % 2 == 0 { "even" } else { "odd" } } })).collect();
        write(&db, ops).unwrap();
        db.close().unwrap();
    }
    let db =
        with_schema(&dir, notes(base_fields(), json!({ "byKind": { "kind": "value", "fields": ["kind"] } }))).unwrap();
    let change = db.open_report().schema_changes.iter().find(|c| c.path == "types.Note.indexes.byKind").unwrap();
    assert_eq!(change.effect, crate::schema::Effect::Builds);
    // Answers are the same whether the build has reached them or not
    let (before, _) = ids_where(&db, "kind", json!("odd"));
    assert_eq!(before.as_array().unwrap().len(), 1250);
    db.wait_for_index_builds();
    let status = db.index_status().unwrap();
    assert!(status.iter().any(|s| s.entity_type == "Note" && s.index == "byKind" && s.built), "{status:?}");
    let (after, visited) = ids_where(&db, "kind", json!("odd"));
    assert_eq!(after, before);
    assert_eq!(visited, 1250, "once built, the index's matches are read, not the type's 2,500 rows");
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_changed_index_is_rebuilt_and_a_removed_one_dropped() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db =
            with_schema(&dir, notes(base_fields(), json!({ "ix": { "kind": "value", "fields": ["kind"] } }))).unwrap();
        write(
            &db,
            vec![json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "kind": "k", "title": "t" } })],
        )
        .unwrap();
        db.close().unwrap();
    }
    {
        // The same name over another field: rebuilt
        let db =
            with_schema(&dir, notes(base_fields(), json!({ "ix": { "kind": "value", "fields": ["title"] } }))).unwrap();
        assert_eq!(db.open_report().schema_changes[0].effect, crate::schema::Effect::Rebuilds);
        db.wait_for_index_builds();
        assert_eq!(ids_where(&db, "title", json!("t")).1, 1, "read from the rebuilt index");
        assert!(db.check().unwrap().is_clean());
        db.close().unwrap();
    }
    let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
    assert_eq!(db.open_report().schema_changes[0].effect, crate::schema::Effect::Drops);
    let entries = db
        .snapshot(|s| {
            Ok(crate::engine::unit::Read::scan(s, &crate::engine::keys::index_range("Note", "ix", &[]))?.len())
        })
        .unwrap();
    assert_eq!(entries, 0);
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_unique_index_over_rows_holding_the_same_values_isnt_built_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
        write(
            &db,
            vec![
                json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "kind": "same" } }),
                json!({ "op": "create", "type": "Note", "id": "Note-2", "fields": { "kind": "same" } }),
            ],
        )
        .unwrap();
        db.close().unwrap();
    }
    let db = with_schema(
        &dir,
        notes(base_fields(), json!({ "one": { "kind": "value", "fields": ["kind"], "unique": true } })),
    )
    .unwrap();
    db.wait_for_index_builds();
    let status = db.index_status().unwrap();
    let one = status.iter().find(|s| s.index == "one").unwrap();
    assert!(!one.built);
    let why = one.failure.as_deref().unwrap();
    assert!(why.contains("Note-1") && why.contains("Note-2"), "{why}");
}

#[test]
fn a_removed_field_keeps_its_data_until_purged() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
        write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "kind": "k" } })]).unwrap();
        assert_eq!(
            db.purge(&Purge::Field { entity_type: "Note".into(), field: "kind".into() }).unwrap_err().code,
            ErrorCode::SchemaChangeRefused
        );
        db.close().unwrap();
    }
    let db = with_schema(&dir, notes(json!({ "title": { "type": "string", "optional": true } }), json!({}))).unwrap();
    assert_eq!(db.open_report().schema_changes[0].effect, crate::schema::Effect::KeepsData);
    assert_eq!(row(&db, "Note-1").unwrap()["kind"], "k", "the data stays");
    assert_eq!(
        write(&db, vec![json!({ "op": "set", "id": "Note-1", "fields": { "kind": "x" } })]).unwrap_err().code,
        ErrorCode::UnknownField
    );
    db.purge(&Purge::Field { entity_type: "Note".into(), field: "kind".into() }).unwrap();
    assert!(row(&db, "Note-1").unwrap().get("kind").is_none());
}

#[test]
fn a_removed_type_keeps_its_entities_until_purged() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(&dir);
        write(
            &db,
            vec![
                json!({ "op": "create", "type": "Doc", "id": "Doc-1", "fields": { "title": "t" } }),
                json!({ "op": "create", "type": "Task", "id": "Task-1" }),
                json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Doc-1" }),
            ],
        )
        .unwrap();
        db.close().unwrap();
    }
    let mut schema = serde_json::to_value(super::schema()).unwrap();
    schema["types"].as_object_mut().unwrap().remove("Doc");
    let db = with_schema(&dir, schema).unwrap();
    assert!(row(&db, "Doc-1").is_some());
    assert_eq!(
        write(&db, vec![json!({ "op": "create", "type": "Doc", "fields": { "title": "t" } })]).unwrap_err().code,
        ErrorCode::UnknownType
    );
    db.purge(&Purge::Type("Doc".into())).unwrap();
    assert!(row(&db, "Doc-1").is_none());
    assert_eq!(super::edges(&db), Vec::<String>::new(), "its relations went with it");
    assert_eq!(db.purge(&Purge::Type("Task".into())).unwrap_err().code, ErrorCode::SchemaChangeRefused);
}

#[test]
fn rename_field_and_rename_relation_move_data_to_a_declared_name() {
    let dir = tempfile::tempdir().unwrap();
    let mut fields = base_fields();
    fields["heading"] = json!({ "type": "string", "optional": true });
    let db =
        with_schema(&dir, notes(fields, json!({ "byHeading": { "kind": "value", "fields": ["heading"] } }))).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "title": "t" } }),
            json!({ "op": "create", "type": "Note", "id": "Note-2" }),
            json!({ "op": "link", "source": "Note-1", "kind": "refs", "target": "Note-2" }),
        ],
    )
    .unwrap();
    db.rename_field("Note", "title", "heading").unwrap();
    let r = row(&db, "Note-1").unwrap();
    assert_eq!((r.get("title"), r["heading"].clone()), (None, json!("t")));
    assert_eq!(ids_where(&db, "heading", json!("t")).0, json!(["Note-1"]), "the moved values are indexed");
    db.rename_relation("refs", "cites").unwrap();
    assert_eq!(super::edges(&db), ["Note-1|cites|Note-2"]);
    assert_eq!(db.rename_field("Note", "kind", "nope").unwrap_err().code, ErrorCode::SchemaChangeRefused);
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn rename_type_rewrites_ids_their_relations_and_the_refs_naming_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut schema = notes(base_fields(), json!({}));
    schema["types"]["Memo"] = json!({ "fields": { "title": { "type": "string", "optional": true }, "kind": { "type": "string", "optional": true } } });
    schema["types"]["Tag"]["fields"]["of"]["to"] = json!(["Note", "Memo"]);
    let db = with_schema(&dir, schema).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-a", "fields": { "title": "t" } }),
            json!({ "op": "create", "type": "Note", "id": "Note-b" }),
            json!({ "op": "link", "source": "Note-a", "kind": "refs", "target": "Note-b" }),
            json!({ "op": "create", "type": "Tag", "id": "Tag-1", "fields": { "of": "Note-a" } }),
        ],
    )
    .unwrap();
    db.rename_type("Note", "Memo").unwrap();
    assert!(row(&db, "Note-a").is_none());
    assert_eq!(row(&db, "Memo-a").unwrap()["title"], "t");
    assert_eq!(super::edges(&db), ["Memo-a|refs|Memo-b"]);
    assert_eq!(row(&db, "Tag-1").unwrap()["of"], "Memo-a");
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
}
