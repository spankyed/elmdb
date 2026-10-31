//! Derived data and schema changes: `check()` and `rebuild()`, an index built in the background, and each row of
//! `docs/architecture/schema.md`'s change table as an open applies it, with the admin operations a migration runs.

use serde_json::{Value as Json, json};

use super::{open, row, write};
use crate::engine::admin::Purge;
use crate::engine::keys;
use crate::engine::unit::Read;
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
    db.wait_for_index_builds().unwrap();
    let status = db.index_status().unwrap();
    assert!(status.iter().any(|s| s.entity_type == "Note" && s.index == "byKind" && s.built), "{status:?}");
    let (after, visited) = ids_where(&db, "kind", json!("odd"));
    assert_eq!(after, before);
    assert_eq!(visited, 1250, "once built, the index's matches are read, not the type's 2,500 rows");
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn waiting_for_index_builds_with_a_unit_open_is_refused_rather_than_waiting_for_good() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
        let ops: Vec<Json> = (0..20_000)
            .map(|i| json!({ "op": "create", "type": "Note", "fields": { "kind": format!("k{i}") } }))
            .collect();
        write(&db, ops).unwrap();
        db.close().unwrap();
    }
    let db =
        with_schema(&dir, notes(base_fields(), json!({ "byKind": { "kind": "value", "fields": ["kind"] } }))).unwrap();
    // The build waits for the unit to end, and the unit is this thread's: a wait here would never end
    let unit = db.begin().unwrap();
    let built = |db: &Database| db.index_status().unwrap().iter().any(|s| s.index == "byKind" && s.built);
    assert!(!built(&db), "the build is still under way while the unit is open");
    assert_eq!(db.wait_for_index_builds().unwrap_err().code, ErrorCode::UnitOpen);
    drop(unit);
    db.wait_for_index_builds().unwrap();
    assert!(built(&db));
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
        db.wait_for_index_builds().unwrap();
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
fn a_unique_index_is_enforced_while_it_is_still_being_built() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
        let ops: Vec<Json> = (0..20_000)
            .map(|i| json!({ "op": "create", "type": "Note", "id": format!("Note-{i:05}"), "fields": { "kind": format!("k{i}") } }))
            .collect();
        write(&db, ops).unwrap();
        db.close().unwrap();
    }
    let db = with_schema(
        &dir,
        notes(base_fields(), json!({ "one": { "kind": "value", "fields": ["kind"], "unique": true } })),
    )
    .unwrap();
    // The unit holds the build off, so the last entity's value is in no entry yet
    let mut unit = db.begin().unwrap();
    assert!(!db.index_status().unwrap().iter().any(|s| s.index == "one" && s.built));
    let e = unit
        .write(&super::batch(vec![json!({ "op": "create", "type": "Note", "fields": { "kind": "k19999" } })]))
        .unwrap_err();
    assert_eq!(e.code, ErrorCode::Unique, "{}", e.message);
    assert!(e.message.contains("Note-19999"), "{}", e.message);
    unit.write(&super::batch(vec![json!({ "op": "create", "type": "Note", "fields": { "kind": "new" } })])).unwrap();
    unit.commit(false).unwrap();
    db.wait_for_index_builds().unwrap();
    let one = db.index_status().unwrap().into_iter().find(|s| s.index == "one").unwrap();
    assert!(one.built, "{one:?}");
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn rebuilding_one_index_refuses_one_the_schema_doesnt_declare() {
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
    write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "kind": "a" } })]).unwrap();
    assert_eq!(db.rebuild(Some(("Note", "later"))).unwrap_err().code, ErrorCode::UnknownIndex);
    assert_eq!(db.rebuild(Some(("Nope", "later"))).unwrap_err().code, ErrorCode::UnknownType);
    // Declared later, the index is built over the rows, not read as built and empty
    db.set_schema(
        Schema::from_json(&notes(base_fields(), json!({ "later": { "kind": "value", "fields": ["kind"] } }))).unwrap(),
    )
    .unwrap();
    db.wait_for_index_builds().unwrap();
    assert_eq!(ids_where(&db, "kind", json!("a")).0, json!(["Note-1"]));
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_rebuild_leaves_unbuilt_a_value_index_a_build_couldnt_finish() {
    let dir = tempfile::tempdir().unwrap();
    let mut fields = base_fields();
    fields["other"] = json!({ "type": "string", "optional": true });
    let indexes = json!({
        "one": { "kind": "value", "fields": ["kind"], "unique": true },
        "pair": { "kind": "value", "fields": ["title", "other"] }
    });
    let db = with_schema(&dir, notes(fields.clone(), json!({}))).unwrap();
    // Each value within what an index holds, the pair past LMDB's key, and a value two rows hold
    let long = "x".repeat(256);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "kind": "same", "title": long, "other": long } }),
            json!({ "op": "create", "type": "Note", "id": "Note-2", "fields": { "kind": "same" } }),
        ],
    )
    .unwrap();
    db.set_schema(Schema::from_json(&notes(fields, indexes)).unwrap()).unwrap();
    db.wait_for_index_builds().unwrap();
    let status = db.index_status().unwrap();
    let pair = status.iter().find(|s| s.index == "pair").unwrap();
    assert!(!pair.built);
    assert!(pair.failure.as_deref().is_some_and(|f| f.contains("511 bytes")), "the build says why: {pair:?}");

    let one = db.rebuild(Some(("Note", "one"))).unwrap_err();
    assert_eq!(one.code, ErrorCode::Unique, "{}", one.message);
    let pair = db.rebuild(Some(("Note", "pair"))).unwrap_err();
    assert_eq!(pair.code, ErrorCode::ValueInvalid, "{}", pair.message);
    // A rebuild of everything, which a rename or a replace runs, goes on and leaves both unbuilt
    db.rebuild(None).unwrap();
    let status = db.index_status().unwrap();
    assert!(status.iter().filter(|s| s.index == "one" || s.index == "pair").all(|s| !s.built), "{status:?}");
    let report = db.check().unwrap();
    assert!(report.differences.is_empty(), "{report:?}");
    assert!(report.violations.iter().any(|v| v.subject == "Note-1, Note-2"), "the duplicates are reported: {report:?}");
    assert_eq!(
        ids_where(&db, "kind", json!("same")).0,
        json!(["Note-1", "Note-2"]),
        "an unbuilt index's rows are walked"
    );
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
    db.wait_for_index_builds().unwrap();
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

#[test]
fn an_index_declared_on_an_open_database_is_built_and_read_and_writes_go_on_to_the_new_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
    let rows: Vec<Json> = (0..2500)
        .map(|i| json!({ "op": "create", "type": "Note", "fields": { "kind": if i % 2 == 0 { "even" } else { "odd" } } }))
        .collect();
    write(&db, rows).unwrap();
    let changes = db
        .set_schema(
            Schema::from_json(&notes(base_fields(), json!({ "ix": { "kind": "value", "fields": ["kind"] } }))).unwrap(),
        )
        .unwrap();
    assert_eq!(changes[0].effect, crate::schema::Effect::Builds);
    db.wait_for_index_builds().unwrap();
    assert_eq!(ids_where(&db, "kind", json!("even")).1, 1250, "read from the index built after the change");
    // A write after the change keeps the index
    write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-x", "fields": { "kind": "even" } })]).unwrap();
    assert_eq!(ids_where(&db, "kind", json!("even")).1, 1251);
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn rename_type_rewrites_a_type_s_refs_to_itself_and_moves_its_embeddings() {
    let dir = tempfile::tempdir().unwrap();
    let folder = |indexes: Json| {
        json!({ "fields": {
            "parent": { "type": "ref", "to": ["Folder", "Dir"], "optional": true },
            "text": { "type": "string", "optional": true }
        }, "indexes": indexes })
    };
    let meaning = json!({ "meaning": { "kind": "vector", "from": ["text"], "dims": 2, "model": "m" } });
    let schema = json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "types": { "Folder": folder(meaning.clone()), "Dir": folder(meaning) }
    });
    let db = with_schema(&dir, schema).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Folder", "id": "Folder-1", "fields": { "text": "root" } }),
            json!({ "op": "create", "type": "Folder", "id": "Folder-2", "fields": { "parent": "Folder-1" } }),
        ],
    )
    .unwrap();
    let pending: crate::engine::plan::Reads =
        serde_json::from_value(json!({ "reads": [{ "read": "pendingEmbeddings", "type": "Folder" }] })).unwrap();
    let answer: Json =
        serde_json::to_value(rmpv::decode::read_value(&mut &db.read(&pending).unwrap()[..]).unwrap()).unwrap();
    let chunk = &answer[0]["chunks"][0];
    write(&db, vec![json!({ "op": "embed", "id": "Folder-1", "chunk": chunk["chunk"], "hash": chunk["hash"], "model": "m", "vector": [1.0, 0.0] })]).unwrap();

    db.rename_type("Folder", "Dir").unwrap();
    assert_eq!(row(&db, "Dir-2").unwrap()["parent"], "Dir-1", "a ref to its own type follows the rename");
    let near: crate::engine::plan::Reads = serde_json::from_value(json!({ "reads": [{ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Dir"] }, "steps": [{ "step": "near", "vector": [1.0, 0.0], "k": 1 }],
        "terminal": { "answer": "ids" } } }] }))
    .unwrap();
    let found: Json =
        serde_json::to_value(rmpv::decode::read_value(&mut &db.read(&near).unwrap()[..]).unwrap()).unwrap();
    assert_eq!(found[0]["ids"], json!(["Dir-1"]), "its embedding moved with it");
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn rename_type_into_a_type_with_entities_rewrites_their_refs_and_keeps_its_numbering() {
    let dir = tempfile::tempdir().unwrap();
    let folder = |prefix: &str| {
        json!({ "shortCode": { "prefix": prefix }, "fields": {
            "parent": { "type": "ref", "to": ["Folder", "Dir"], "optional": true }
        } })
    };
    let schema = json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "types": { "Folder": folder("FLD"), "Dir": folder("DIR") }
    });
    let db = with_schema(&dir, schema).unwrap();
    let take = |n: usize| (0..n).map(|_| json!({ "op": "takeLabel", "type": "Dir", "base": "Untitled" }));
    let mut ops = vec![
        json!({ "op": "create", "type": "Folder", "id": "Folder-1" }),
        json!({ "op": "create", "type": "Dir", "id": "Dir-x", "fields": { "parent": "Folder-1" } }),
        json!({ "op": "create", "type": "Dir", "id": "Dir-y" }),
        json!({ "op": "create", "type": "Dir", "id": "Dir-z" }),
        json!({ "op": "takeLabel", "type": "Folder", "base": "Untitled" }),
    ];
    ops.extend(take(3));
    write(&db, ops).unwrap();
    // DIR-3 and the labels handed out are Dir's numbers for good, though nothing holds them now
    write(&db, vec![json!({ "op": "destroy", "id": "Dir-z" })]).unwrap();

    db.rename_type("Folder", "Dir").unwrap();
    assert_eq!(row(&db, "Dir-x").unwrap()["parent"], "Dir-1", "a ref Dir held before the rename follows it");
    let results = write(
        &db,
        vec![json!({ "op": "create", "type": "Dir" }), json!({ "op": "takeLabel", "type": "Dir", "base": "Untitled" })],
    )
    .unwrap();
    let crate::engine::batch::OpResult::Entity { short_code, .. } = &results[0] else { panic!("{results:?}") };
    assert_eq!(short_code.as_deref(), Some("DIR-4"), "a number Dir handed out isn't handed out again");
    let crate::engine::batch::OpResult::Label { label } = &results[1] else { panic!("{results:?}") };
    assert_eq!(label, "Untitled 4");
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn an_indexed_string_is_held_to_bytes_and_a_key_past_lmdb_s_limit_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut fields = base_fields();
    fields["other"] = json!({ "type": "string", "optional": true });
    let indexes = json!({ "kind": { "kind": "value", "fields": ["kind"] }, "pair": { "kind": "value", "fields": ["title", "other"] } });
    let db = with_schema(&dir, notes(fields, indexes)).unwrap();
    // 200 characters, 600 bytes: past what an index holds, so it is stored and found by walking the rows
    let wide = "中".repeat(200);
    write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-w", "fields": { "kind": wide } })]).unwrap();
    assert_eq!(ids_where(&db, "kind", json!(wide)).0, json!(["Note-w"]));
    // Each half within the limit, the pair past LMDB's key: refused, saying why
    let long = "x".repeat(256);
    let refused =
        write(&db, vec![json!({ "op": "create", "type": "Note", "fields": { "title": long, "other": long } })])
            .unwrap_err();
    assert_eq!(refused.code, ErrorCode::ValueInvalid, "{}", refused.message);
    assert!(refused.message.contains("pair"), "{}", refused.message);
}

#[test]
fn a_where_on_a_compound_index_s_first_field_answers_as_a_scan_does() {
    let dir = tempfile::tempdir().unwrap();
    let mut fields = base_fields();
    fields["title"] = json!({ "type": "any", "optional": true });
    let db =
        with_schema(&dir, notes(fields, json!({ "ab": { "kind": "value", "fields": ["title", "kind"] } }))).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "title": "x" } }),
            json!({ "op": "create", "type": "Note", "id": "Note-2", "fields": { "title": "x", "kind": "k" } }),
            json!({ "op": "create", "type": "Note", "id": "Note-3", "fields": { "title": ["x", "y"], "kind": "k" } }),
        ],
    )
    .unwrap();
    assert_eq!(ids_where(&db, "title", json!("x")).0, json!(["Note-1", "Note-2", "Note-3"]));
}

#[test]
fn a_plan_past_the_step_limit_is_refused_rather_than_overflowing_the_stack() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let plan_of = |n: usize| -> crate::engine::plan::Reads {
        let steps: Vec<Json> = (0..n).map(|_| json!({ "step": "ofType", "type": "Task" })).collect();
        serde_json::from_value(json!({ "reads": [{ "read": "query", "plan": {
            "start": { "from": "all" }, "steps": steps, "terminal": { "answer": "ids" } } }] }))
        .unwrap()
    };
    // On a stack the size of Node's main thread's, 8 MB
    let at_limit = plan_of(crate::engine::reads::MAX_STEPS);
    let held = std::thread::scope(|s| {
        std::thread::Builder::new().stack_size(8 << 20).spawn_scoped(s, || db.read(&at_limit).is_ok()).unwrap().join()
    });
    assert!(held.unwrap(), "the limit itself is held");
    assert_eq!(db.read(&plan_of(crate::engine::reads::MAX_STEPS + 1)).unwrap_err().code, ErrorCode::BatchInvalid);
}

/// The paths and effects an open reported
fn reported(db: &Database) -> Vec<(String, crate::schema::Effect)> {
    db.open_report().schema_changes.iter().map(|c| (c.path.clone(), c.effect)).collect()
}

#[test]
fn a_declaration_added_over_rows_it_breaks_is_enforced() {
    use crate::schema::Effect;
    let dir = tempfile::tempdir().unwrap();
    {
        let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
        write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-1" })]).unwrap();
        write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-2" })]).unwrap();
        write(&db, vec![json!({ "op": "link", "kind": "cites", "source": "Note-1", "target": "Note-2" })]).unwrap();
        db.close().unwrap();
    }
    // A required field on a type with rows, which none of them holds; one on a type with none breaks nothing
    let mut schema = notes(base_fields(), json!({}));
    schema["types"]["Note"]["fields"]["due"] = json!({ "type": "string" });
    schema["types"]["Note"]["fields"]["later"] = json!({ "type": "string", "optional": true });
    schema["types"]["Tag"]["fields"]["name"] = json!({ "type": "string" });
    let db = with_schema(&dir, schema).unwrap();
    assert_eq!(
        reported(&db),
        [
            ("types.Note.fields.due".to_owned(), Effect::Enforced),
            ("types.Note.fields.later".to_owned(), Effect::Applied),
            ("types.Tag.fields.name".to_owned(), Effect::Applied),
        ]
    );
    assert!(!db.check().unwrap().violations.is_empty(), "check() reports the rows the change enforces a rule on");
    db.close().unwrap();
    // A kind and a type removed keep their data, and declared again hold it to their rules
    let mut without = notes(base_fields(), json!({}));
    without["relations"].as_object_mut().unwrap().remove("cites");
    without["types"].as_object_mut().unwrap().remove("Note");
    without["types"]["Tag"]["fields"]["of"]["to"] = json!(["Tag"]);
    with_schema(&dir, without).unwrap().close().unwrap();
    let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
    assert_eq!(
        reported(&db),
        [
            ("relations.cites".to_owned(), Effect::Enforced),
            ("types.Note".to_owned(), Effect::Enforced),
            ("types.Tag.fields.of".to_owned(), Effect::Enforced),
        ]
    );
}

#[test]
fn a_short_code_prefix_stored_codes_hold_stays_with_their_type() {
    let schema = |types: Json| json!({ "$schema": crate::schema::SCHEMA_LANGUAGE, "types": types });
    let dir = tempfile::tempdir().unwrap();
    {
        let db = with_schema(&dir, schema(json!({ "Note": { "shortCode": { "prefix": "NTE" } } }))).unwrap();
        write(&db, vec![json!({ "op": "create", "type": "Note" })]).unwrap();
        db.close().unwrap();
    }
    // The type removed keeps its entities and their codes
    with_schema(&dir, schema(json!({}))).unwrap().close().unwrap();
    for (types, path) in [
        (json!({ "Note": { "shortCode": { "prefix": "NOT" } } }), "types.Note.shortCode.prefix"),
        (json!({ "Note": {} }), "types.Note.shortCode"),
        (json!({ "Memo": { "shortCode": { "prefix": "NTE" } } }), "types.Memo.shortCode.prefix"),
    ] {
        let error = with_schema(&dir, schema(types)).err().unwrap();
        assert_eq!(error.code, ErrorCode::SchemaChangeRefused, "{path}");
        assert!(error.message.contains(path), "{path}: {}", error.message);
    }
    // Declared again as it was, it takes its codes back
    with_schema(&dir, schema(json!({ "Note": { "shortCode": { "prefix": "NTE" } } }))).unwrap().close().unwrap();
}

#[test]
fn check_reports_missing_references_through_nested_lists_before_and_after_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(
        &dir,
        json!({
            "$schema": crate::schema::SCHEMA_LANGUAGE,
            "types": {"Node": {"fields": {
                "links": {"type":"list", "of":{"type":"list", "of":{"type":"ref", "to":["Node"]}}},
                "opaque": {"type":"any", "optional":true}
            }}}
        }),
    )
    .unwrap();
    write(
        &db,
        vec![
            json!({"op":"create", "type":"Node", "id":"Node-present", "fields":{"links":[]}}),
            json!({"op":"create", "type":"Node", "id":"Node-root", "fields":{
            "links":[["Node-present", "Node-missing"], ["Node-gone"]], "opaque":["Node-opaque"]}}),
        ],
    )
    .unwrap();
    for rebuilt in [false, true] {
        if rebuilt {
            db.rebuild(None).unwrap();
        }
        let report = db.check().unwrap();
        assert_eq!(report.violations.len(), 2, "{report:?}");
        assert!(report.violations.iter().all(|v| v.subject == "Node-root"));
        assert!(report.violations.iter().any(|v| v.message.contains("links[0][1] names Node-missing")));
        assert!(report.violations.iter().any(|v| v.message.contains("links[1][0] names Node-gone")));
    }
}

#[test]
fn relation_audit_uses_primary_rows_for_every_declared_rule_even_after_rebuild() {
    use crate::engine::{keys, relations::Stored};
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(
        &dir,
        json!({
            "$schema": crate::schema::SCHEMA_LANGUAGE,
            "types": {"Node":{"fields":{}}, "Other":{"fields":{}}},
            "relations": {
                "strict": {"cardinality":"many-to-many", "unique":true, "acyclic":true,
                    "endpoints":[{"source":"Node", "target":"Node"}], "info":{"type":"integer"}},
                "refs": {"cardinality":"many-to-many", "info":{
                    "type":"list", "of":{"type":"list", "of":{"type":"ref", "to":["Node"]}}}}
            }
        }),
    )
    .unwrap();
    write(
        &db,
        vec![
            json!({"op":"create", "type":"Node", "id":"Node-a"}),
            json!({"op":"create", "type":"Node", "id":"Node-b"}),
            json!({"op":"create", "type":"Node", "id":"Node-c"}),
            json!({"op":"create", "type":"Other", "id":"Other-o"}),
        ],
    )
    .unwrap();
    let rows = [
        ("forward", "strict", "Node-a", "Node-b", Some(1.into())),
        ("cycle", "strict", "Node-b", "Node-a", Some(2.into())),
        ("duplicate", "strict", "Node-a", "Node-b", Some(3.into())),
        ("self", "strict", "Node-a", "Node-a", Some(4.into())),
        ("endpoint", "strict", "Other-o", "Node-a", Some(5.into())),
        ("required", "strict", "Node-a", "Node-c", None),
        ("typed", "strict", "Node-c", "Node-b", Some("wrong".into())),
        ("missing", "strict", "Node-a", "Node-missing", Some(8.into())),
        (
            "nested",
            "refs",
            "Node-a",
            "Node-b",
            Some(rmpv::Value::Array(vec![rmpv::Value::Array(vec!["Node-missing".into()])])),
        ),
    ];
    damage(&db, |txn, d| {
        for (seq, (id, kind, source, target, info)) in rows.iter().enumerate() {
            let stored = Stored {
                kind: (*kind).into(),
                source: (*source).into(),
                target: (*target).into(),
                info: info.clone(),
                weight: None,
                seq: seq as u64 + 1,
            };
            d.put(txn, &keys::relation(&format!("Relation-{id}")), &rmp_serde::to_vec_named(&stored).unwrap())?;
        }
        Ok(())
    });
    // No derived edge was written: auditing must find these directly in primary rows.
    let before = db.check().unwrap().violations;
    for (subject, message) in [
        ("Relation-cycle", "cycle"),
        ("Relation-duplicate", "unique pair"),
        ("Relation-self", "itself"),
        ("Relation-endpoint", "doesn't join"),
        ("Relation-required", "required info"),
        ("Relation-typed", "integer"),
        ("Relation-missing", "no entity"),
        ("Relation-nested", "info[0][0]"),
    ] {
        assert!(
            before.iter().any(|v| v.subject.contains(subject) && v.message.contains(message)),
            "missing {subject}: {message}; {before:?}"
        );
    }
    db.rebuild(None).unwrap();
    assert_eq!(db.check().unwrap().violations, before, "rebuild cannot hide primary violations");
}

#[test]
fn uniqueness_audit_reads_primary_values_including_unindexed_values() {
    use crate::engine::{keys, values};
    use rmpv::Value;
    let cases = [
        ("long string", Value::from("x".repeat(300)), Value::from("x".repeat(300)), true),
        (
            "unordered map",
            Value::Map(vec![("a".into(), 1.into()), ("b".into(), true.into())]),
            Value::Map(vec![("b".into(), true.into()), ("a".into(), Value::F64(1.0))]),
            true,
        ),
        (
            "duplicate map keys",
            Value::Map(vec![("a".into(), 1.into()), ("a".into(), 2.into())]),
            Value::Map(vec![("a".into(), 2.into()), ("a".into(), 1.into())]),
            true,
        ),
        ("bytes", Value::Binary(vec![1, 2, 3]), Value::Binary(vec![1, 2, 3]), true),
        ("timestamp", Value::Ext(-1, vec![0, 0, 0, 1]), Value::Ext(-1, vec![0, 0, 0, 1]), true),
        ("numeric representation", 1.into(), Value::F64(1.0), true),
        ("signed zero", Value::F64(-0.0), Value::F64(0.0), true),
        ("null", Value::Nil, Value::Nil, true),
        ("list elements", Value::Array(vec![7.into(), 7.into()]), 7.into(), false),
    ];
    for (name, first, second, compound) in cases {
        let dir = tempfile::tempdir().unwrap();
        let db = with_schema(
            &dir,
            notes(
                json!({
                    "v":{"type":"any", "optional":true, "nullable":true},
                    "other":{"type":"any", "optional":true}
                }),
                json!({
                    "one":{"kind":"value", "fields":["v"], "unique":true},
                    "pair":{"kind":"value", "fields":["v","other"], "unique":true}
                }),
            ),
        )
        .unwrap();
        write(
            &db,
            vec![
                json!({"op":"create", "type":"Note", "id":"Note-1"}),
                json!({"op":"create", "type":"Note", "id":"Note-2"}),
            ],
        )
        .unwrap();
        damage(&db, |txn, d| {
            for (id, value) in [("Note-1", &first), ("Note-2", &second)] {
                d.put(txn, &keys::value(id, "v"), &values::encode(value))?;
                d.put(txn, &keys::value(id, "other"), &values::encode(&"tuple".into()))?;
            }
            Ok(())
        });
        for rebuilt in [false, true] {
            if rebuilt {
                db.rebuild(None).unwrap();
            }
            let report = db.check().unwrap();
            assert_eq!(report.violations.len(), if compound { 2 } else { 1 }, "{name}: {report:?}");
            assert!(report.violations.iter().all(|v| v.subject == "Note-1, Note-2"), "{name}: {report:?}");
            assert!(report.violations.iter().any(|v| v.message.contains("Note.one")), "{name}: {report:?}");
            assert_eq!(report.violations.iter().any(|v| v.message.contains("Note.pair")), compound, "{name}");
        }
    }
}

#[test]
fn chunked_index_build_resumes_across_mutations_and_reopening() {
    use crate::engine::{
        derive, keys,
        unit::{Read, Txn, UnitState},
    };
    fn clear(db: &Database) {
        let store = db.store().unwrap();
        let schema = db.engine_schema();
        store
            .write(
                |txn, d| {
                    let mut state = UnitState::new(false);
                    let mut t = Txn { txn, db: d, state: &mut state, schema: &schema, now: 0.0 };
                    for (key, _) in t.scan(&keys::index_range("Task", "a", &[]))? {
                        t.del(&key)?;
                    }
                    t.del(&keys::index_built("Task", "a"))?;
                    Ok(())
                },
                false,
            )
            .unwrap();
    }
    fn chunk(db: &Database, after: u64) -> Option<u64> {
        let store = db.store().unwrap();
        let schema = db.engine_schema();
        store
            .write(
                |txn, d| {
                    let mut state = UnitState::new(false);
                    derive::build_chunk(
                        &mut Txn { txn, db: d, state: &mut state, schema: &schema, now: 0.0 },
                        "Task",
                        "a",
                        after,
                        3,
                    )
                },
                false,
            )
            .unwrap()
    }
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        (1..=12).map(|i| json!({"op":"create", "type":"Task", "id":format!("Task-{i}"), "fields":{"a":i}})).collect(),
    )
    .unwrap();
    clear(&db);
    let mut after = chunk(&db, 0).unwrap();
    write(
        &db,
        vec![
            json!({"op":"trash", "id":"Task-2"}),
            json!({"op":"trash", "id":"Task-8"}),
            json!({"op":"destroy", "id":"Task-5"}),
            json!({"op":"create", "type":"Task", "id":"Task-13", "fields":{"a":13}}),
            json!({"op":"set", "id":"Task-9", "fields":{"a":999}}),
        ],
    )
    .unwrap();
    while let Some(next) = chunk(&db, after) {
        assert!(next > after);
        after = next;
    }
    assert!(db.check().unwrap().is_clean());
    clear(&db);
    assert!(chunk(&db, 0).is_some());
    db.close().unwrap();
    let reopened = Database::open(dir.path(), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    reopened.wait_for_index_builds().unwrap();
    let report = reopened.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert!(reopened.index_status().unwrap().iter().find(|s| s.entity_type == "Task" && s.index == "a").unwrap().built);
}

#[test]
fn unchecked_field_audit_covers_every_declared_value_type_and_presence_rule() {
    use crate::engine::maintenance::Load;
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(
        &dir,
        json!({
            "$schema": crate::schema::SCHEMA_LANGUAGE,
            "types": {"Other":{"fields":{}}, "Node":{"fields":{
                "required":{"type":"string"}, "s":{"type":"string"}, "n":{"type":"number"},
                "i":{"type":"integer"}, "b":{"type":"boolean"}, "bytes":{"type":"bytes"},
                "choice":{"type":"enum", "values":["yes"]}, "reference":{"type":"ref", "to":["Node"]},
                "list":{"type":"list", "of":{"type":"number"}}, "object":{"type":"object"},
                "nonnull":{"type":"string"}, "anything":{"type":"any"}
            }}}
        }),
    )
    .unwrap();
    db.bulk_load(&[
        Load::Entity { id: "Other-present".into(), fields: Default::default(), trashed: false },
        Load::Entity {
            id: "Node-root".into(),
            fields: serde_json::from_value(json!({
                "s":true, "n":"x", "i":1.5, "b":0, "bytes":[], "choice":"no", "reference":"Other-present",
                "list":[false], "object":[], "nonnull":null, "anything":{"opaque":["Node-missing"]}, "extra":true
            }))
            .unwrap(),
            trashed: false,
        },
    ])
    .unwrap();
    let report = db.check().unwrap();
    assert_eq!(report.violations.len(), 12, "{report:?}");
    for field in ["required", "s", "n", "i", "b", "bytes", "choice", "reference", "list", "object", "nonnull", "extra"]
    {
        assert!(
            report.violations.iter().any(|v| v.subject == "Node-root" && v.message.contains(field)),
            "missing {field}: {report:?}"
        );
    }
    assert!(report.violations.iter().all(|v| !v.message.contains("anything")), "any is opaque");
}

#[test]
fn unchecked_cardinality_audit_counts_distinct_far_ends_for_all_four_kinds() {
    use crate::engine::maintenance::Load;
    for (cardinality, expected) in [("many-to-many", 0), ("one-to-many", 1), ("many-to-one", 1), ("one-to-one", 2)] {
        let dir = tempfile::tempdir().unwrap();
        let db = with_schema(
            &dir,
            json!({"$schema":crate::schema::SCHEMA_LANGUAGE,
                "types":{"Node":{"fields":{}}}, "relations":{"links":{"cardinality":cardinality}}
            }),
        )
        .unwrap();
        let mut loads: Vec<Load> = ["Node-a", "Node-b", "Node-c"]
            .into_iter()
            .map(|id| Load::Entity { id: id.into(), fields: Default::default(), trashed: false })
            .collect();
        for (i, (source, target)) in
            [("Node-a", "Node-b"), ("Node-a", "Node-c"), ("Node-b", "Node-c"), ("Node-a", "Node-b")]
                .into_iter()
                .enumerate()
        {
            loads.push(Load::Relation {
                id: format!("Relation-{i}"),
                kind: "links".into(),
                source: source.into(),
                target: target.into(),
                info: None,
                weight: None,
            });
        }
        db.bulk_load(&loads).unwrap();
        let report = db.check().unwrap();
        assert_eq!(report.violations.len(), expected, "{cardinality}: {report:?}");
        db.rebuild(None).unwrap();
        assert_eq!(db.check().unwrap().violations, report.violations);
    }
}

#[test]
fn duplicate_map_key_multiplicity_is_preserved_by_writes_and_audits() {
    use crate::engine::batch::{Batch, Op};
    use rmpv::Value;
    let distinct = Value::Map(vec![("a".into(), 1.into()), ("b".into(), 2.into())]);
    let repeated = Value::Map(vec![("a".into(), 1.into()), ("a".into(), 1.into())]);
    for values in [[distinct.clone(), repeated.clone()], [repeated, distinct]] {
        let dir = tempfile::tempdir().unwrap();
        let db = with_schema(
            &dir,
            notes(
                json!({"u":{"type":"any","optional":true}}),
                json!({"u":{"kind":"value","fields":["u"],"unique":true}}),
            ),
        )
        .unwrap();
        let create = |id: &str, value: Value| Batch {
            ops: vec![Op::Create {
                entity_type: "Note".into(),
                id: Some(id.into()),
                fields: [("u".into(), value)].into(),
                short_code: None,
                created_at: None,
                updated_at: None,
            }],
            now: Some(1000.0),
        };
        db.write(&create("Note-0", values[0].clone()), false).unwrap();
        db.write(&create("Note-1", values[1].clone()), false).unwrap();
        assert!(db.check().unwrap().is_clean(), "distinct maps must not be reported as duplicates: {values:?}");
        db.rebuild(None).unwrap();
        assert!(db.check().unwrap().is_clean());
        assert_eq!(db.write(&create("Note-2", values[0].clone()), false).unwrap_err().code, crate::ErrorCode::Unique);
    }
}

#[test]
fn a_removed_type_takes_its_indexes_data_with_it_and_keeps_its_rows() {
    let doc = json!({
        "fields": { "kind": { "type": "string" }, "body": { "type": "string" } },
        "indexes": {
            "kind": { "kind": "value", "fields": ["kind"] },
            "words": { "kind": "text", "fields": { "body": {} } },
            "grams": { "kind": "trigram", "fields": ["body"] },
            "meaning": { "kind": "vector", "from": ["body"], "dims": 4, "model": "m" }
        }
    });
    let schema = |types: Json| json!({ "$schema": crate::schema::SCHEMA_LANGUAGE, "types": types });
    let held = |db: &Database| {
        db.snapshot(|r| {
            let mut ranges = vec![keys::index_range("Doc", "kind", &[]), keys::trigram_index("Doc", "grams")];
            ranges.extend(keys::text_index_ranges("Doc", "words"));
            ranges.extend(
                [Prefix::VectorChunk, Prefix::VectorPending].map(|p| keys::vector_index_range(p, "Doc", "meaning")),
            );
            ranges.extend(["kind", "words", "grams", "meaning"].map(|index| keys::index_built("Doc", index)));
            let mut n = 0;
            for range in ranges {
                n += Read::scan(r, &range)?.len();
            }
            Ok(n + usize::from(Read::get(r, &keys::text_stats("Doc", "words"))?.is_some()))
        })
        .unwrap()
    };
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(&dir, schema(json!({ "Doc": doc.clone() }))).unwrap();
    write(&db, vec![json!({ "op": "create", "type": "Doc", "id": "Doc-1", "fields": { "kind": "memo", "body": "the error text" } })])
        .unwrap();
    assert!(held(&db) > 0);
    db.close().unwrap();

    // Removed: its declared indexes' entries and markers go, its row stays, and check() finds nothing to report
    let db = with_schema(&dir, schema(json!({}))).unwrap();
    let mut effects = reported(&db);
    effects.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        effects,
        [
            ("types.Doc".to_owned(), crate::schema::Effect::KeepsData),
            ("types.Doc.indexes.grams".to_owned(), crate::schema::Effect::Drops),
            ("types.Doc.indexes.kind".to_owned(), crate::schema::Effect::Drops),
            ("types.Doc.indexes.meaning".to_owned(), crate::schema::Effect::Drops),
            ("types.Doc.indexes.words".to_owned(), crate::schema::Effect::Drops),
        ]
    );
    assert_eq!(held(&db), 0);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    db.purge(&Purge::Type("Doc".into())).unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    assert!(row(&db, "Doc-1").is_none());
    db.close().unwrap();

    // Declared again over nothing, its indexes are built empty and hold what is written next
    let db = with_schema(&dir, schema(json!({ "Doc": doc }))).unwrap();
    db.wait_for_index_builds().unwrap();
    write(&db, vec![json!({ "op": "create", "type": "Doc", "id": "Doc-2", "fields": { "kind": "memo", "body": "more error text" } })])
        .unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
}

/// The ids a plan over Notes with one step answers
fn note_ids(db: &Database, step: Json) -> Json {
    let reads: crate::engine::plan::Reads = serde_json::from_value(json!({ "reads": [{ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Note"] }, "steps": [step], "terminal": { "answer": "ids" } } }] }))
    .unwrap();
    let bytes = db.read(&reads).unwrap();
    serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()[0]["ids"].clone()
}

#[test]
fn each_of_a_types_text_and_trigram_indexes_is_built_and_rebuilt_from_its_own_fields() {
    let dir = tempfile::tempdir().unwrap();
    let mut fields = base_fields();
    fields["body"] = json!({ "type": "string", "optional": true });
    let db = with_schema(&dir, notes(fields.clone(), json!({}))).unwrap();
    let words = [("title", "alpha"), ("kind", "gamma"), ("body", "delta")];
    let row: serde_json::Map<String, Json> = words.iter().map(|(f, w)| (f.to_string(), json!(w))).collect();
    write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": row })]).unwrap();
    // Three of each kind, so that an index built or rebuilt from another's fields leaves one of them without its own
    let mut indexes = serde_json::Map::new();
    for (field, _) in words {
        indexes.insert(format!("{field}Text"), json!({ "kind": "text", "fields": { field: {} } }));
        indexes.insert(format!("{field}Grams"), json!({ "kind": "trigram", "fields": [field] }));
    }
    db.set_schema(Schema::from_json(&notes(fields, Json::Object(indexes))).unwrap()).unwrap();
    db.wait_for_index_builds().unwrap();
    let each_finds_its_word = |db: &Database, after: &str| {
        for (field, word) in words {
            let searched = note_ids(db, json!({ "step": "search", "query": word, "index": format!("{field}Text") }));
            assert_eq!(searched, json!(["Note-1"]), "{field}Text finds {word} {after}");
            let gram = &word[1..4];
            let contained =
                note_ids(db, json!({ "step": "containing", "text": gram, "index": format!("{field}Grams") }));
            assert_eq!(contained, json!(["Note-1"]), "{field}Grams finds {gram} {after}");
        }
    };
    each_finds_its_word(&db, "built in the background");
    for (field, _) in words {
        for kind in ["Text", "Grams"] {
            let index = format!("{field}{kind}");
            db.rebuild(Some(("Note", &index))).unwrap();
            each_finds_its_word(&db, &format!("after {index} is rebuilt"));
        }
    }
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
}
