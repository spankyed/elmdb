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
            // One place, the first, under a value no task holds
            &crate::engine::keys::index_range("Task", "a", &[crate::format::key::Part::Number(99.0)]),
            &[1, 1],
        )?;
        Ok(())
    });
    let report = db.check().unwrap();
    let kinds: Vec<&str> = report.differences.iter().map(|d| d.kind.as_str()).collect();
    assert!(kinds.contains(&"missing") && kinds.contains(&"differs") && kinds.contains(&"extra"), "{report:?}");
    // A rebuild of the one index clears what it held: the entry nothing derives goes, and the rest stays wrong
    db.rebuild(Some(("Task", "a"))).unwrap();
    let kinds: Vec<String> = db.check().unwrap().differences.iter().map(|d| d.kind.clone()).collect();
    assert!(kinds.iter().any(|k| k == "missing") && !kinds.iter().any(|k| k == "extra"), "{kinds:?}");
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
    assert!(
        status.iter().any(|s| s.entity_type.as_deref() == Some("Note") && s.index == "byKind" && s.built),
        "{status:?}"
    );
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
    fields["more"] = json!({ "type": "string", "optional": true });
    let indexes = json!({
        "one": { "kind": "value", "fields": ["kind"], "unique": true },
        "pair": { "kind": "value", "fields": ["title", "other"] },
        // Its first note holds no `more`: where the index files it among those without is the key too long
        "trio": { "kind": "value", "fields": ["title", "other", "more"] }
    });
    let db = with_schema(&dir, notes(fields.clone(), json!({}))).unwrap();
    // Each value within what an index holds, the pair past LMDB's key, and a value two rows hold
    let long = "x".repeat(256);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "kind": "same", "title": long, "other": long } }),
            json!({ "op": "create", "type": "Note", "id": "Note-2", "fields": { "kind": "same" } }),
            json!({ "op": "create", "type": "Note", "id": "Note-3", "fields": { "title": "t", "other": "o" } }),
        ],
    )
    .unwrap();
    db.set_schema(Schema::from_json(&notes(fields, indexes)).unwrap()).unwrap();
    // The wait says a build failed, and the status below says why
    assert_eq!(db.wait_for_index_builds().unwrap_err().code, ErrorCode::IndexBuildFailed);
    let status = db.index_status().unwrap();
    let pair = status.iter().find(|s| s.index == "pair").unwrap();
    assert!(!pair.built);
    assert!(pair.failure.as_deref().is_some_and(|f| f.contains("511 bytes")), "the build says why: {pair:?}");

    let one = db.rebuild(Some(("Note", "one"))).unwrap_err();
    assert_eq!(one.code, ErrorCode::Unique, "{}", one.message);
    let pair = db.rebuild(Some(("Note", "pair"))).unwrap_err();
    assert_eq!(pair.code, ErrorCode::ValueInvalid, "{}", pair.message);
    let trio = status.iter().find(|s| s.index == "trio").unwrap();
    assert!(!trio.built && trio.failure.as_deref().is_some_and(|f| f.contains("Note.trio")), "{trio:?}");
    let trio = db.rebuild(Some(("Note", "trio"))).unwrap_err();
    assert_eq!(trio.code, ErrorCode::ValueInvalid, "{}", trio.message);
    // Where it files the note besides its entries is refused as an entry is, by the note and the index
    assert!(trio.message.contains("Note-1") && trio.message.contains("Note.trio"), "{}", trio.message);
    // A rebuild of everything, which a rename or a replace runs, goes on and leaves the three unbuilt
    db.rebuild(None).unwrap();
    let status = db.index_status().unwrap();
    let unbuilt = ["one", "pair", "trio"];
    assert!(status.iter().filter(|s| unbuilt.contains(&s.index.as_str())).all(|s| !s.built), "{status:?}");
    // Unbuilt, the index keeps what it files that fits, as a build that failed part way does: the third note,
    // among those without a `more`
    let without = crate::engine::keys::index_others(crate::format::Prefix::IndexWithout, "Note", "trio", &[]);
    let filed: Vec<String> =
        db.snapshot(|s| Ok(super::filed_under(s, "Note", &without).into_iter().map(|(_, id)| id).collect())).unwrap();
    assert_eq!(filed, ["Note-3"]);
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
    // The wait says a build failed, and the status below says why
    assert_eq!(db.wait_for_index_builds().unwrap_err().code, ErrorCode::IndexBuildFailed);
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
fn every_write_to_an_entity_of_a_removed_type_is_refused_until_it_is_declared_again() {
    let dir = tempfile::tempdir().unwrap();
    let (from_doc, between_tasks) = {
        let db = open(&dir);
        let made = write(
            &db,
            vec![
                json!({ "op": "create", "type": "Doc", "id": "Doc-1", "fields": { "title": "t", "body": "b" } }),
                json!({ "op": "create", "type": "Doc", "id": "Doc-2", "fields": { "title": "u" } }),
                json!({ "op": "create", "type": "Task", "id": "Task-1" }),
                json!({ "op": "create", "type": "Task", "id": "Task-2" }),
                json!({ "op": "grant", "id": "Doc-1", "role": "r1" }),
                json!({ "op": "link", "source": "Doc-1", "kind": "refs", "target": "Task-1" }),
                json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" }),
                json!({ "op": "link", "source": "Task-2", "kind": "refs", "target": "Doc-1" }),
            ],
        )
        .unwrap();
        db.close().unwrap();
        let relation = |made: &crate::OpResult| match made {
            crate::OpResult::Relation { id, .. } => id.clone(),
            other => panic!("not a relation: {other:?}"),
        };
        (relation(&made[5]), relation(&made[6]))
    };
    let mut schema = serde_json::to_value(super::schema()).unwrap();
    schema["types"].as_object_mut().unwrap().remove("Doc");
    let db = with_schema(&dir, schema).unwrap();
    let before = (row(&db, "Doc-1").unwrap(), super::edges(&db), db.stats().unwrap());
    let writes = [
        ("set", json!({ "op": "set", "id": "Doc-1", "fields": { "title": "x" } })),
        ("append", json!({ "op": "append", "id": "Doc-1", "field": "body", "value": "c" })),
        ("merge", json!({ "op": "merge", "id": "Doc-1", "field": "body", "value": { "k": 1 } })),
        ("delete", json!({ "op": "delete", "id": "Doc-1", "field": "body" })),
        ("removeAt", json!({ "op": "removeAt", "id": "Doc-1", "field": "body", "index": 0 })),
        ("removeValue", json!({ "op": "removeValue", "id": "Doc-1", "field": "body", "value": "b" })),
        ("trash", json!({ "op": "trash", "id": "Doc-1" })),
        ("restore", json!({ "op": "restore", "id": "Doc-1" })),
        ("grant", json!({ "op": "grant", "id": "Doc-1", "role": "r2" })),
        ("revoke", json!({ "op": "revoke", "id": "Doc-1", "role": "r1" })),
        ("grantExclusive", json!({ "op": "grantExclusive", "id": "Doc-1", "role": "r2", "among": [] })),
        ("link from it", json!({ "op": "link", "source": "Doc-1", "kind": "refs", "target": "Task-2" })),
        ("link to it", json!({ "op": "link", "source": "Task-2", "kind": "refs", "target": "Doc-1" })),
        ("unlink", json!({ "op": "unlink", "source": "Doc-1" })),
        ("unlink to it", json!({ "op": "unlink", "source": "Task-1", "target": "Doc-1" })),
        ("unlink every edge of one linked to it", json!({ "op": "unlink", "source": "Task-2" })),
        ("unlink a kind's edges of one linked to it", json!({ "op": "unlink", "source": "Task-2", "kind": "refs" })),
        (
            "its role taken for another",
            json!({ "op": "grantExclusive", "id": "Task-1", "role": "r1", "among": ["Doc-1"] }),
        ),
        ("its relation removed", json!({ "op": "removeRelation", "relation": from_doc })),
        ("its relation changed", json!({ "op": "updateRelation", "relation": from_doc, "info": { "k": 1 } })),
        ("its relation moved off it", json!({ "op": "updateRelation", "relation": from_doc, "source": "Task-2" })),
        ("a relation moved from it", json!({ "op": "updateRelation", "relation": between_tasks, "source": "Doc-2" })),
        ("a relation moved to it", json!({ "op": "updateRelation", "relation": between_tasks, "target": "Doc-2" })),
        ("destroy", json!({ "op": "destroy", "id": "Doc-1" })),
    ];
    for (what, op) in writes {
        let error = write(&db, vec![op]).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnknownType, "{what}: {error}");
        assert!(error.message.contains("Doc"), "{what}: {error}");
    }
    assert_eq!((row(&db, "Doc-1").unwrap(), super::edges(&db), db.stats().unwrap()), before, "and none wrote anything");
    // An edge of a declared kind between two entities of declared types moves and goes as ever
    write(&db, vec![json!({ "op": "removeRelation", "relation": between_tasks })]).unwrap();
    db.close().unwrap();

    // Declared again, its entities are written as before
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "set", "id": "Doc-1", "fields": { "title": "x" } }),
            json!({ "op": "trash", "id": "Doc-2" }),
        ],
    )
    .unwrap();
    assert_eq!(row(&db, "Doc-1").unwrap()["title"], "x");
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
fn purge_and_rename_field_refuse_the_engine_s_own_fields() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "title": "t" } }),
            json!({ "op": "create", "type": "Task", "id": "Task-2" }),
            json!({ "op": "trash", "id": "Task-2" }),
            json!({ "op": "grant", "id": "Task-1", "role": "r1" }),
        ],
    )
    .unwrap();
    let before = (row(&db, "Task-1").unwrap(), row(&db, "Task-2").unwrap());
    // None is a field the schema declares, so each passed for what a removed declaration left: purging `trashedAt`
    // restored every row, and renaming `shortCode` took every code
    for name in ["id", "entityType", "shortCode", "createdAt", "updatedAt", "trashedAt", "role"] {
        let purge = db.purge(&Purge::Field { entity_type: "Task".into(), field: name.into() }).unwrap_err();
        assert_eq!(purge.code, ErrorCode::ReservedField, "purge {name}: {purge}");
        let from = db.rename_field("Task", name, "title").unwrap_err();
        assert_eq!(from.code, ErrorCode::ReservedField, "rename {name}: {from}");
        let to = db.rename_field("Task", "title", name).unwrap_err();
        assert_eq!(to.code, ErrorCode::ReservedField, "rename to {name}: {to}");
    }
    assert_eq!((row(&db, "Task-1").unwrap(), row(&db, "Task-2").unwrap()), before);
    let stats = db.stats().unwrap();
    assert_eq!((stats.types["Task"].live, stats.types["Task"].trashed), (1, 1), "the trash is as it was");
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn rename_relation_refuses_an_edge_that_would_make_a_second_of_the_new_kind_and_moves_nothing() {
    // An edge of the new kind with the same ends is one identity already; renaming onto it would leave two, which the
    // engine's check reports and a link would refuse
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": "x" } }),
            json!({ "op": "create", "type": "Task", "id": "Task-2", "fields": { "a": "x" } }),
        ],
    )
    .unwrap();
    write(
        &db,
        vec![
            json!({ "op": "link", "source": "Task-1", "kind": "tags", "target": "Task-2" }),
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" }),
        ],
    )
    .unwrap();
    let error = db.rename_relation("refs", "tags").unwrap_err();
    assert_eq!(error.code, ErrorCode::DuplicateEdge, "{error}");
    assert!(error.message.contains("Task-1") && error.message.contains("Task-2"), "{error}");
    assert!(db.check().unwrap().is_clean(), "a refused rename leaves the graph as it was");
    write(&db, vec![json!({ "op": "unlink", "source": "Task-1", "kind": "tags", "target": "Task-2" })]).unwrap();
    db.rename_relation("refs", "tags").expect("with no edge of the new kind joining the ends, the rename goes through");
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn rename_relation_moves_same_keyed_edges_between_different_ends_without_refusing() {
    // Two edges of one key value are one identity only between the same ends: between different ends they rename fine
    let keyed = |key: Json| json!({ "cardinality": "many-to-many", "info": { "type": "object", "optional": true }, "key": key });
    let schema = json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "relations": { "old": keyed(json!(["a"])), "fresh": keyed(json!(["a"])) },
        "types": { "Note": { "fields": { "title": { "type": "string", "optional": true } } } }
    });
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(&dir, schema).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "title": "t" } }),
            json!({ "op": "create", "type": "Note", "id": "Note-2", "fields": { "title": "t" } }),
            json!({ "op": "create", "type": "Note", "id": "Note-3", "fields": { "title": "t" } }),
            json!({ "op": "link", "source": "Note-1", "kind": "old", "target": "Note-2", "info": { "a": 1 } }),
            json!({ "op": "link", "source": "Note-1", "kind": "old", "target": "Note-3", "info": { "a": 1 } }),
        ],
    )
    .unwrap();
    db.rename_relation("old", "fresh").expect("the same key between different ends is one identity each");
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn renaming_a_kind_to_itself_moves_nothing_and_refuses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(&dir, notes(base_fields(), json!({}))).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "title": "t" } }),
            json!({ "op": "create", "type": "Note", "id": "Note-2", "fields": { "title": "t" } }),
            json!({ "op": "link", "source": "Note-1", "kind": "refs", "target": "Note-2" }),
        ],
    )
    .unwrap();
    db.rename_relation("refs", "refs").expect("a kind renamed to itself is a no-op");
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn rename_relation_refuses_two_moving_edges_that_would_be_one_identity_in_the_new_kind() {
    // Under the old kind's key, (a 1, b "x") and (a 1.0, b "y") are two edges; under the new kind's key, a alone, the
    // same value, so renaming both would leave one identity twice
    let keyed = |key: Json| json!({ "cardinality": "many-to-many", "info": { "type": "object", "optional": true }, "key": key });
    let schema = json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "relations": { "old": keyed(json!(["a", "b"])), "fresh": keyed(json!(["a"])) },
        "types": { "Note": { "fields": { "title": { "type": "string", "optional": true } } } }
    });
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(&dir, schema).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "title": "t" } }),
            json!({ "op": "create", "type": "Note", "id": "Note-2", "fields": { "title": "t" } }),
            json!({ "op": "link", "source": "Note-1", "kind": "old", "target": "Note-2", "info": { "a": 1, "b": "x" } }),
            json!({ "op": "link", "source": "Note-1", "kind": "old", "target": "Note-2", "info": { "a": 1.0, "b": "y" } }),
        ],
    )
    .unwrap();
    let error = db.rename_relation("old", "fresh").unwrap_err();
    assert_eq!(error.code, ErrorCode::DuplicateEdge, "{error}");
    assert!(db.check().unwrap().is_clean(), "a refused rename leaves the graph as it was");
}

/// Each relation's id and row, in the order of their ids
fn relations(db: &Database) -> Vec<(String, crate::engine::relations::Stored)> {
    db.snapshot(|s| {
        let mut out = Vec::new();
        for (key, bytes) in s.scan(&keys::all_relations())? {
            let Some(crate::format::key::Part::String(id)) = crate::format::key::decode(&key)?.pop() else {
                panic!("a relation's key ends in its id")
            };
            out.push((id, rmp_serde::from_slice(&bytes).unwrap()));
        }
        Ok(out)
    })
    .unwrap()
}

#[test]
fn rename_relation_holds_each_edge_to_the_new_kind_s_rules_and_moves_none_when_one_is_refused() {
    let entity =
        |id: &str| json!({ "op": "create", "type": id.split('-').next().unwrap(), "id": id, "fields": { "a": "x" } });
    let link = |source: &str, kind: &str, target: &str| json!({ "op": "link", "source": source, "kind": kind, "target": target });
    let entities = ["Task-1", "Task-2", "Task-3", "Task-4", "Note-1"].map(entity).to_vec();
    // Each: the edges there are, the rename, and what refuses it. `parent_of` gives a target one source and holds no
    // cycle; `pins` joins a Note to a Task and gives a source one target; `cites` reads its key from info's fields
    let plain = json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2", "info": "plain" });
    // The last of each: the ends of the relation the refusal names, the one that can't join those moved before it
    let refused = [
        (
            vec![link("Task-1", "refs", "Task-3"), link("Task-2", "refs", "Task-3")],
            "refs",
            "parent_of",
            ErrorCode::Cardinality,
            ("Task-2", "Task-3"),
        ),
        // With an edge the kind holds already
        (
            vec![link("Task-1", "parent_of", "Task-3"), link("Task-2", "refs", "Task-3")],
            "refs",
            "parent_of",
            ErrorCode::Cardinality,
            ("Task-2", "Task-3"),
        ),
        (
            vec![link("Task-1", "refs", "Task-2"), link("Task-2", "refs", "Task-3"), link("Task-3", "refs", "Task-1")],
            "refs",
            "parent_of",
            ErrorCode::Cycle,
            ("Task-3", "Task-1"),
        ),
        // A cycle one edge of the kind and one moved edge close between them
        (
            vec![link("Task-1", "parent_of", "Task-2"), link("Task-2", "refs", "Task-1")],
            "refs",
            "parent_of",
            ErrorCode::Cycle,
            ("Task-2", "Task-1"),
        ),
        (vec![link("Task-1", "refs", "Task-2")], "refs", "pins", ErrorCode::Endpoint, ("Task-1", "Task-2")),
        (
            vec![link("Note-1", "refs", "Task-1"), link("Note-1", "refs", "Task-2")],
            "refs",
            "pins",
            ErrorCode::Cardinality,
            ("Note-1", "Task-2"),
        ),
        (vec![plain.clone()], "refs", "cites", ErrorCode::FieldType, ("Task-1", "Task-2")),
        // Info that is no map reads as a null key, the identity of the edge without info the kind holds already: the
        // info is what is refused, and not a second edge of one identity
        (vec![link("Task-1", "cites", "Task-2"), plain], "refs", "cites", ErrorCode::FieldType, ("Task-1", "Task-2")),
    ];
    for (links, from, to, code, (source, target)) in refused {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        write(&db, entities.clone()).unwrap();
        write(&db, links.clone()).unwrap();
        let before = super::edges(&db);
        let (named, _) = relations(&db)
            .into_iter()
            .find(|(_, row)| row.kind == from && row.source == source && row.target == target)
            .expect("the relation the refusal names");
        let error = db.rename_relation(from, to).unwrap_err();
        assert_eq!(error.code, code, "{from} to {to} over {links:?}: {error}");
        assert_eq!(super::edges(&db), before, "{from} to {to}: no edge moved");
        assert!(db.check().unwrap().is_clean(), "{from} to {to}: a refused rename leaves the graph as it was");
        // The relation, the rule as a link is told it, and what to do. A count or a cycle is of the relation beside
        // others, of the kind already or moved before it, any of which may be the one to remove; any other rule is of
        // the relation alone
        let (as_a_link, to_do) = match code {
            ErrorCode::Cardinality | ErrorCode::Cycle => (
                format!(
                    "Beside the relations of kind {to} and those of kind {from} the rename moves before it, a link of \
                     it would be refused"
                ),
                format!(
                    "Remove or change that relation, or do the same to one it can't be of kind {to} beside, which is \
                     of that kind already or of kind {from} with it"
                ),
            ),
            _ => ("A link of it is refused".to_owned(), "Remove or change that relation".to_owned()),
        };
        let rule = match (code, to) {
            (ErrorCode::Cardinality, "parent_of") => {
                "Task-3 already has a parent_of source, and a parent_of target has one; unlink it first"
            }
            (ErrorCode::Cardinality, _) => {
                "Note-1 already has a pins target, and a pins source has one; unlink it first"
            }
            (ErrorCode::Cycle, _) => "Cannot create a parent_of relation that would form a cycle",
            (ErrorCode::Endpoint, _) => "A pins relation doesn't join a Task to a Task",
            _ => "A cites relation's info holds its key (n), so it is a map, not \"plain\"",
        };
        let told = format!(
            "renameRelation({from:?} to {to:?}): the relation {named}, from {source} to {target}, can't be of kind {to}, \
             and nothing was moved. {as_a_link}: \"{rule}\". {to_do}, then rename again"
        );
        assert_eq!(error.message, told);
    }
    // Edges the new kind can hold move, and are held to its rules from then on as any of its edges are
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, entities.clone()).unwrap();
    write(
        &db,
        vec![link("Task-1", "refs", "Task-2"), link("Task-1", "refs", "Task-3"), link("Task-2", "refs", "Task-4")],
    )
    .unwrap();
    // An end in the trash is an end still
    write(&db, vec![json!({ "op": "trash", "id": "Task-4" })]).unwrap();
    db.rename_relation("refs", "parent_of").unwrap();
    assert!(db.check().unwrap().is_clean());
    let moved = super::edges(&db);
    assert!(moved.len() == 3 && moved.iter().all(|edge| edge.contains("|parent_of|")), "{moved:?}");
    let second_parent = write(&db, vec![link("Task-3", "parent_of", "Task-2")]).unwrap_err();
    assert_eq!(second_parent.code, ErrorCode::Cardinality, "{second_parent}");
    let cycle = write(&db, vec![link("Task-3", "parent_of", "Task-1")]).unwrap_err();
    assert_eq!(cycle.code, ErrorCode::Cycle, "{cycle}");
    // The end in the trash has its one source, as the edge moved to it gave it
    let second_of_trashed = write(&db, vec![link("Task-3", "parent_of", "Task-4")]).unwrap_err();
    assert_eq!(second_of_trashed.code, ErrorCode::Cardinality, "{second_of_trashed}");
    // And an edge to an end in the trash is refused as one to any end is: a second source of it
    write(&db, vec![link("Task-1", "refs", "Task-4")]).unwrap();
    let error = db.rename_relation("refs", "parent_of").unwrap_err();
    assert_eq!(error.code, ErrorCode::Cardinality, "{error}");
    write(&db, vec![json!({ "op": "unlink", "source": "Task-1", "kind": "refs", "target": "Task-4" })]).unwrap();
    // Two edges of one pair, told apart by their key, are one far end: a kind that gives an end one holds both
    let cited =
        |n: u8| json!({ "op": "link", "source": "Task-1", "kind": "cites", "target": "Task-2", "info": { "n": n } });
    write(&db, vec![cited(1), cited(2)]).unwrap();
    db.rename_relation("cites", "holds").unwrap();
    assert!(db.check().unwrap().is_clean());
    // A weight goes with its edge: to a kind that carries one, and to one that reads none, where it stays in the row
    // as a weight does whose kind stops being weighted. An edge with none is one of weight 1 in a kind that carries them
    write(&db, vec![json!({ "op": "link", "source": "Task-1", "kind": "weighs", "target": "Task-2", "weight": 3 })])
        .unwrap();
    let weight_of = |db: &Database, kind: &str| {
        relations(db).into_iter().find(|(_, row)| row.kind == kind).map(|(_, row)| row.weight)
    };
    db.rename_relation("weighs", "rates").unwrap();
    assert_eq!(weight_of(&db, "rates"), Some(Some(3.0)));
    db.rename_relation("rates", "tags").unwrap();
    assert_eq!(weight_of(&db, "tags"), Some(Some(3.0)));
    write(&db, vec![link("Task-2", "blocks", "Task-3")]).unwrap();
    db.rename_relation("blocks", "weighs").unwrap();
    assert_eq!(weight_of(&db, "weighs"), Some(None));
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn the_edge_mirror_holds_a_loaded_weight_below_zero_as_the_store_does() {
    use crate::engine::maintenance::Load;
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let task = |id: &str| json!({ "op": "create", "type": "Task", "id": id, "fields": { "a": "x" } });
    write(&db, vec![task("Task-1"), task("Task-2"), task("Task-3")]).unwrap();
    let load = |id: &str, target: &str, weight: Option<f64>| Load::Relation {
        id: id.into(),
        kind: "weighs".into(),
        source: "Task-1".into(),
        target: target.into(),
        info: None,
        weight,
    };
    // A load keeps any finite weight, -1 among them, and an edge with none beside it
    db.bulk_load(&[load("Relation-light", "Task-2", Some(-1.0)), load("Relation-plain", "Task-3", None)]).unwrap();
    let mirror = db.store().unwrap().mirror();
    let at = db.sequence().unwrap();
    mirror.lock().at(at);
    let mut held = Vec::new();
    db.snapshot(|s| {
        mirror.lock().visit_far_ends(s, at, "Task-1", true, "weighs", |far, weight| held.push((far.to_owned(), weight)))
    })
    .unwrap();
    // Read again, from the list the first read left in the mirror
    let mut again = Vec::new();
    db.snapshot(|s| {
        mirror
            .lock()
            .visit_far_ends(s, at, "Task-1", true, "weighs", |far, weight| again.push((far.to_owned(), weight)))
    })
    .unwrap();
    assert_eq!(mirror.lock().held(), 1);
    assert_eq!(held, [("Task-2".to_owned(), Some(-1.0)), ("Task-3".to_owned(), None)]);
    assert_eq!(again, held);
    // The row is reported, a weight no link is given, and the mirror's list is the store's
    let reported = db.check().unwrap();
    let subjects: Vec<&str> = reported.violations.iter().map(|v| v.subject.as_str()).collect();
    assert_eq!(subjects, ["p/r Relation-light"], "{reported:?}");
    assert!(reported.differences.is_empty(), "{reported:?}");
}

#[test]
fn rename_relation_refuses_what_a_load_brought_that_no_link_of_the_new_kind_is_given() {
    use crate::engine::maintenance::Load;
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let task = |id: &str| json!({ "op": "create", "type": "Task", "id": id, "fields": { "a": "x" } });
    write(&db, vec![task("Task-1"), task("Task-2")]).unwrap();
    let load = |id: &str, kind: &str, target: &str, weight: Option<f64>| Load::Relation {
        id: id.into(),
        kind: kind.into(),
        source: "Task-1".into(),
        target: target.into(),
        info: None,
        weight,
    };
    // A load keeps a weight below zero and an entity joined to itself, each of which `check()` reports where it is
    db.bulk_load(&[
        load("Relation-light", "tags", "Task-2", Some(-1.0)),
        load("Relation-own", "blocks", "Task-1", None),
    ])
    .unwrap();
    let before = relations(&db);
    let reported = db.check().unwrap();
    let subjects: Vec<&str> = reported.violations.iter().map(|v| v.subject.as_str()).collect();
    assert_eq!(subjects, ["p/r Relation-light", "p/r Relation-own"], "{reported:?}");
    // Into a kind whose weights are read, the weight is held to a link's range
    let error = db.rename_relation("tags", "weighs").unwrap_err();
    assert_eq!(error.code, ErrorCode::ValueInvalid, "{error}");
    let told = "renameRelation(\"tags\" to \"weighs\"): the relation Relation-light, from Task-1 to Task-2, can't be of \
                kind weighs, and nothing was moved. A link of it is refused: \"A weighs relation's weight is a number \
                from 0 to 2^960, and was given -1\". Remove that relation, or bulkLoad it again under its id with a \
                weight in that range, then rename again";
    assert_eq!(error.message, told);
    // No kind joins an entity to itself
    let error = db.rename_relation("blocks", "refs").unwrap_err();
    assert_eq!(error.code, ErrorCode::SelfLink, "{error}");
    let told = "renameRelation(\"blocks\" to \"refs\"): the relation Relation-own, from Task-1 to Task-1, can't be of \
                kind refs, and nothing was moved. A link of it is refused: \"Task-1 can't be linked to itself (refs)\". \
                Remove or change that relation, then rename again";
    assert_eq!(error.message, told);
    assert_eq!(relations(&db), before, "neither rename moved an edge");
    assert_eq!(db.check().unwrap(), reported);
    // Into a kind that reads no weight, the weight goes as it is
    db.rename_relation("tags", "refs").unwrap();
    let light = |db: &Database| relations(db).into_iter().find(|(id, _)| id == "Relation-light").unwrap().1;
    assert_eq!((light(&db).kind.as_str(), light(&db).weight), ("refs", Some(-1.0)));
    // What each refusal says to do lets the rename through: the relation loaded again with a weight in a link's
    // range, and the entity joined to itself removed
    assert_eq!(db.rename_relation("refs", "weighs").unwrap_err().code, ErrorCode::ValueInvalid);
    db.bulk_load(&[load("Relation-light", "refs", "Task-2", Some(2.0))]).unwrap();
    db.rename_relation("refs", "weighs").unwrap();
    assert_eq!((light(&db).kind.as_str(), light(&db).weight), ("weighs", Some(2.0)));
    write(
        &db,
        vec![
            json!({ "op": "removeRelation", "relation": "Relation-own" }),
            json!({ "op": "link", "source": "Task-2", "kind": "blocks", "target": "Task-1" }),
        ],
    )
    .unwrap();
    db.rename_relation("blocks", "refs").unwrap();
    assert_eq!(super::edges(&db), ["Task-1|weighs|Task-2", "Task-2|refs|Task-1"]);
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn rename_relation_answers_a_read_that_failed_as_it_is_and_not_as_a_rule_of_the_new_kind() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let task = |id: &str| json!({ "op": "create", "type": "Task", "id": id, "fields": { "a": "x" } });
    let link = |source: &str, target: &str| json!({ "op": "link", "source": source, "kind": "refs", "target": target });
    write(&db, vec![task("Task-1"), task("Task-2"), task("Task-3"), task("Task-4")]).unwrap();
    // The first moves before the read of the second's fails
    write(&db, vec![link("Task-3", "Task-4"), link("Task-1", "Task-2")]).unwrap();
    // An entry among Task-2's `parent_of` edges in that holds no relation and far end: the rule that counts a
    // target's sources reads it
    damage(&db, |txn, d| Ok(d.put(txn, &keys::edge(false, "Task-2", "parent_of", 9), &[0xc0])?));
    let error = db.rename_relation("refs", "parent_of").unwrap_err();
    assert_eq!(error.code, ErrorCode::Corrupted, "{error}");
    assert_eq!(error.message, "An edge key's value doesn't read");
    assert_eq!(
        super::edges(&db),
        ["Task-1|refs|Task-2", "Task-3|refs|Task-4"],
        "the edge moved first is of its kind again"
    );
}

#[test]
fn rename_relation_holds_an_edge_to_a_one_to_one_kind_and_to_info_a_kind_requires() {
    let schema = json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "relations": {
            "plain": { "cardinality": "many-to-many" },
            "solo": { "cardinality": "one-to-one" },
            "noted": { "cardinality": "many-to-many", "info": { "type": "object" } }
        },
        "types": { "Note": { "fields": { "title": { "type": "string", "optional": true } } } }
    });
    let link =
        |source: &str, target: &str| json!({ "op": "link", "source": source, "kind": "plain", "target": target });
    // A one-to-one kind gives each end one far end, either way; a kind whose info is required takes no edge without
    let refused = [
        (vec![link("Note-1", "Note-2"), link("Note-1", "Note-3")], "solo", ErrorCode::Cardinality),
        (vec![link("Note-1", "Note-3"), link("Note-2", "Note-3")], "solo", ErrorCode::Cardinality),
        (vec![link("Note-1", "Note-2")], "noted", ErrorCode::RequiredField),
    ];
    for (links, to, code) in refused {
        let dir = tempfile::tempdir().unwrap();
        let db = with_schema(&dir, schema.clone()).unwrap();
        let notes = ["Note-1", "Note-2", "Note-3"].map(|id| json!({ "op": "create", "type": "Note", "id": id }));
        write(&db, notes.to_vec()).unwrap();
        write(&db, links.clone()).unwrap();
        let error = db.rename_relation("plain", to).unwrap_err();
        assert_eq!(error.code, code, "plain to {to} over {links:?}: {error}");
        if code == ErrorCode::RequiredField {
            let (named, _) = relations(&db).pop().unwrap();
            let told = format!(
                "renameRelation(\"plain\" to \"noted\"): the relation {named}, from Note-1 to Note-2, can't be of kind \
                 noted, and nothing was moved. A link of it is refused: \"A noted relation's info is required\". Remove \
                 or change that relation, then rename again"
            );
            assert_eq!(error.message, told);
        }
        assert!(db.check().unwrap().is_clean(), "plain to {to}");
        assert!(super::edges(&db).iter().all(|edge| edge.contains("|plain|")), "plain to {to}: no edge moved");
    }
    // Of a kind the schema no longer declares, which a migration renames from, no write changes a relation: the
    // refusal says to remove it or load it again
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(&dir, schema.clone()).unwrap();
    let notes = ["Note-1", "Note-2", "Note-3"].map(|id| json!({ "op": "create", "type": "Note", "id": id }));
    write(&db, notes.to_vec()).unwrap();
    write(&db, vec![link("Note-1", "Note-2"), link("Note-1", "Note-3")]).unwrap();
    db.close().unwrap();
    let mut without = schema.clone();
    without["relations"].as_object_mut().unwrap().remove("plain");
    let db = with_schema(&dir, without).unwrap();
    let (named, _) = relations(&db).pop().unwrap();
    let error = db.rename_relation("plain", "solo").unwrap_err();
    assert_eq!(error.code, ErrorCode::Cardinality, "{error}");
    let told = format!(
        "renameRelation(\"plain\" to \"solo\"): the relation {named}, from Note-1 to Note-3, can't be of kind solo, and \
         nothing was moved. Beside the relations of kind solo and those of kind plain the rename moves before it, a link \
         of it would be refused: \"Note-1 already has a solo target, and a solo source has one; unlink it first\". Remove \
         that relation, or bulkLoad it again under its id, changed, or do the same to one it can't be of kind solo \
         beside, which is of that kind already or of kind plain with it, then rename again"
    );
    assert_eq!(error.message, told);
    // Which a caller can do: the relation of the undeclared kind removed, the rename goes on
    write(&db, vec![json!({ "op": "removeRelation", "relation": named })]).unwrap();
    db.rename_relation("plain", "solo").unwrap();
    assert_eq!(super::edges(&db), ["Note-1|solo|Note-2"]);
    assert!(db.check().unwrap().is_clean());
    // Pairs that share no end are a one-to-one kind's to hold
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(&dir, schema).unwrap();
    let notes = ["Note-1", "Note-2", "Note-3", "Note-4"].map(|id| json!({ "op": "create", "type": "Note", "id": id }));
    write(&db, notes.to_vec()).unwrap();
    write(&db, vec![link("Note-1", "Note-2"), link("Note-3", "Note-4")]).unwrap();
    db.rename_relation("plain", "solo").unwrap();
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn rename_field_refuses_to_replace_a_value_the_new_name_holds() {
    let dir = tempfile::tempdir().unwrap();
    let mut fields = base_fields();
    fields["heading"] = json!({ "type": "string", "optional": true });
    let db = with_schema(&dir, notes(fields, json!({}))).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "title": "only a title" } }),
            json!({ "op": "create", "type": "Note", "id": "Note-2", "fields": { "title": "t", "heading": "h" } }),
        ],
    )
    .unwrap();
    let error = db.rename_field("Note", "title", "heading").unwrap_err();
    assert_eq!(error.code, ErrorCode::SchemaChangeRefused, "{error}");
    assert!(error.message.contains("Note-2"), "{error}");
    let one = row(&db, "Note-1").unwrap();
    assert_eq!((one["title"].clone(), one.get("heading")), (json!("only a title"), None), "none of it was moved");
    let two = row(&db, "Note-2").unwrap();
    assert_eq!((two["title"].clone(), two["heading"].clone()), (json!("t"), json!("h")));
}

#[test]
fn a_write_of_several_fields_of_a_unique_index_is_held_to_the_values_it_leaves() {
    let dir = tempfile::tempdir().unwrap();
    let mut fields = base_fields();
    for name in ["shelf", "row"] {
        fields[name] = json!({ "type": "string", "optional": true });
    }
    let indexes = json!({
        "pair": { "kind": "value", "fields": ["title", "kind"], "unique": true },
        "place": { "kind": "value", "fields": ["shelf", "row"] }
    });
    let db = with_schema(&dir, notes(fields, indexes)).unwrap();
    let note = |id: &str, title: &str, kind: &str| json!({ "op": "create", "type": "Note", "id": id, "fields": { "title": title, "kind": kind } });
    write(
        &db,
        vec![note("Note-1", "b", "w"), note("Note-2", "b", "x"), note("Note-3", "b", "y"), note("Note-4", "c", "y")],
    )
    .unwrap();
    let pair = |id: &str| {
        let held = row(&db, id).unwrap();
        (held["title"].as_str().unwrap().to_owned(), held["kind"].as_str().unwrap().to_owned())
    };
    let set = |id: &str, title: &str, kind: &str| json!({ "op": "set", "id": id, "fields": { "title": title, "kind": kind } });

    // A write's fields are written in name order. By way of (b, x), which Note-2 holds, once its `kind` is written and
    // its `title` not yet, to (c, x), which no row does
    write(&db, vec![set("Note-1", "c", "x")]).unwrap();
    assert_eq!(pair("Note-1"), ("c".to_owned(), "x".to_owned()));
    // An upsert of a row that is there writes the same way: by way of Note-4's (c, y) to (d, y)
    let upsert = json!({ "op": "upsert", "type": "Note", "id": "Note-1", "fields": { "title": "d", "kind": "y" } });
    write(&db, vec![upsert]).unwrap();
    assert_eq!(pair("Note-1"), ("d".to_owned(), "y".to_owned()));
    // Where it ends on values another row holds it is refused, having met none on the way
    let taken = write(&db, vec![set("Note-1", "b", "x")]).unwrap_err();
    assert_eq!(taken.code, ErrorCode::Unique, "{taken}");
    assert!(taken.message.contains("Note-2"), "{taken}");
    assert_eq!(pair("Note-1"), ("d".to_owned(), "y".to_owned()));
    // The values it holds already are no other row's
    write(&db, vec![set("Note-1", "d", "y")]).unwrap();
    // An index that isn't unique takes the values another row holds, written together as written apart
    let placed = |id: &str| json!({ "op": "set", "id": id, "fields": { "shelf": "s", "row": "r" } });
    write(&db, vec![placed("Note-1"), placed("Note-2")]).unwrap();

    // In a unit that goes on after a refusal, the next write is held to the index as every write is
    let mut unit = db.begin().unwrap();
    assert_eq!(unit.write(&super::batch(vec![set("Note-1", "b", "x")])).unwrap_err().code, ErrorCode::Unique);
    let one_field = json!({ "op": "set", "id": "Note-1", "fields": { "title": "b" } });
    assert_eq!(unit.write(&super::batch(vec![one_field])).unwrap_err().code, ErrorCode::Unique, "(b, y) is Note-3's");
    unit.abort();
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
fn rename_type_refuses_a_rename_whose_rows_the_schema_refuses_under_the_new_name_and_moves_nothing() {
    use crate::engine::maintenance::Load;
    let plain =
        || json!({ "title": { "type": "string", "optional": true }, "slug": { "type": "string", "optional": true } });
    let schema = json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "relations": {
            "pins": { "cardinality": "many-to-one", "endpoints": [{ "source": "Note", "target": "Task" }] },
            "blocks": { "cardinality": "many-to-many" },
            "cites": { "cardinality": "many-to-many", "info": { "type": "ref", "to": ["Note", "Page"] } }
        },
        "types": {
            "Note": { "fields": plain() },
            "Page": { "fields": plain() },
            "Memo": { "fields": { "title": { "type": "string" }, "slug": { "type": "string", "optional": true } } },
            "Card": { "fields": plain(), "indexes": { "slug": { "kind": "value", "fields": ["slug"], "unique": true } } },
            "Task": { "fields": {
                "about": { "type": "ref", "to": ["Note"], "optional": true },
                "on": { "type": "ref", "to": ["Note", "Page"], "optional": true },
                "n": { "type": "number", "optional": true }
            } }
        }
    });
    let create = |id: &str, fields: Json| json!({ "op": "create", "type": id.split('-').next().unwrap(), "id": id, "fields": fields });
    // Each case: the rows written, the type renamed to, and what the refusal says of the row the schema refuses
    let cases = [
        (
            vec![create("Note-1", json!({ "slug": "s" }))],
            "Memo",
            vec![
                "renameType(\"Note\" to \"Memo\"): renamed, Memo-1 (now Note-1) lacks title, which every Memo holds, \
                 which the schema refuses, and nothing was moved. Change what is named so that the rule holds, or \
                 change the declaration the rule is of, then rename again",
            ],
        ),
        (
            vec![create("Note-1", json!({ "slug": "s" })), create("Card-9", json!({ "slug": "s" }))],
            "Card",
            vec!["renamed, Card-1 (now Note-1), Card-9 hold the same Card.slug value, which is unique, which"],
        ),
        (
            vec![
                create("Note-1", json!({})),
                create("Task-1", json!({})),
                json!({ "op": "link", "source": "Note-1", "kind": "pins", "target": "Task-1" }),
            ],
            "Page",
            vec!["renamed, the relation Relation-", " pins doesn't join a Page to a Task, which the schema refuses"],
        ),
        (
            vec![create("Note-1", json!({})), create("Task-1", json!({ "about": "Note-1" }))],
            "Page",
            vec!["renamed, Task-1 ", "\"Page-1\" is an id of Page"],
        ),
    ];
    for (ops, to, told) in cases {
        let dir = tempfile::tempdir().unwrap();
        let db = with_schema(&dir, schema.clone()).unwrap();
        write(&db, ops).unwrap();
        let before = db.sequence().unwrap();
        let error = db.rename_type("Note", to).unwrap_err();
        assert_eq!(error.code, ErrorCode::SchemaChangeRefused, "{error}");
        assert!(told.iter().all(|part| error.message.contains(part)), "{error}");
        assert!(row(&db, "Note-1").is_some() && row(&db, &format!("{to}-1")).is_none(), "nothing moved");
        assert_eq!(db.sequence().unwrap(), before, "and nothing was committed");
        let report = db.check().unwrap();
        assert!(report.is_clean(), "{report:?}");
    }
    // A rule broken as the rename began is left to `check()`, whatever the rename writes of its row: a load's edge
    // from an entity to itself, which no link is given, and a loaded entity, whose `ref` the rename rewrites, holding
    // a value its field doesn't take. A relation's info that is a `ref` to the type follows the rename as a field does
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(&dir, schema.clone()).unwrap();
    write(
        &db,
        vec![
            create("Note-1", json!({})),
            create("Task-1", json!({})),
            json!({ "op": "link", "source": "Task-1", "kind": "cites", "target": "Task-2", "info": "Note-1" }),
            create("Task-2", json!({})),
        ],
    )
    .unwrap();
    db.bulk_load(&[
        Load::Relation {
            id: "Relation-own".into(),
            kind: "blocks".into(),
            source: "Task-1".into(),
            target: "Task-1".into(),
            info: None,
            weight: None,
        },
        Load::Entity {
            id: "Task-3".into(),
            fields: serde_json::from_value(json!({ "on": "Note-1", "n": "many" })).unwrap(),
            trashed: false,
        },
    ])
    .unwrap();
    let broken =
        |db: &Database| -> Vec<String> { db.check().unwrap().violations.into_iter().map(|v| v.subject).collect() };
    assert_eq!(broken(&db), ["Task-3", "p/r Relation-own"]);
    db.rename_type("Note", "Page").unwrap();
    assert!(row(&db, "Page-1").is_some());
    assert_eq!(row(&db, "Task-3").unwrap()["on"], "Page-1");
    let cited: Vec<_> = relations(&db).into_iter().filter(|(_, stored)| stored.kind == "cites").collect();
    assert_eq!(cited[0].1.info, Some(rmpv::Value::from("Page-1")));
    let report = db.check().unwrap();
    assert!(report.differences.is_empty(), "{report:?}");
    assert_eq!(broken(&db), ["Task-3", "p/r Relation-own"], "what was broken before, and nothing more");
    // A rule a load left broken that names a moved entity is reported in other words once the entity has another
    // id, and the rename is held to it: an edge from a Page to itself
    db.bulk_load(&[Load::Relation {
        id: "Relation-self".into(),
        kind: "blocks".into(),
        source: "Page-1".into(),
        target: "Page-1".into(),
        info: None,
        weight: None,
    }])
    .unwrap();
    let error = db.rename_type("Page", "Note").unwrap_err();
    assert_eq!(error.code, ErrorCode::SchemaChangeRefused, "{error}");
    assert!(error.message.contains("the relation Relation-self a blocks relation links Note-1 to itself"), "{error}");
    assert!(row(&db, "Page-1").is_some());
    // A row of a type the schema doesn't declare is changed by a load, which the refusal says
    db.bulk_load(&[Load::Entity { id: "Old-1".into(), fields: Default::default(), trashed: false }]).unwrap();
    let error = db.rename_type("Old", "Memo").unwrap_err();
    assert_eq!(error.code, ErrorCode::SchemaChangeRefused, "{error}");
    let told = "Change what is named so that the rule holds, a row of type Old by a bulkLoad of it under its id, the \
                schema not declaring the type, or change the declaration the rule is of, then rename again";
    assert!(error.message.contains(told), "{error}");
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
                "strict": {"cardinality":"many-to-many", "acyclic":true,
                    "endpoints":[{"source":"Node", "target":"Node"}], "info":{"type":"integer"}},
                "refs": {"cardinality":"many-to-many", "info":{
                    "type":"list", "of":{"type":"list", "of":{"type":"ref", "to":["Node"]}}}},
                "keyed": {"cardinality":"many-to-many", "key":["n"]}
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
        ("keyedOne", "keyed", "Node-b", "Node-c", Some(rmpv::Value::Map(vec![("n".into(), 1.into())]))),
        ("keyedTwo", "keyed", "Node-b", "Node-c", Some(rmpv::Value::Map(vec![("n".into(), 1.into())]))),
        ("keyedOther", "keyed", "Node-b", "Node-c", Some(rmpv::Value::Map(vec![("n".into(), 2.into())]))),
        ("keyedNumber", "keyed", "Node-c", "Node-a", Some(5.into())),
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
        ("Relation-duplicate", "duplicate strict edges join Node-a to Node-b"),
        ("Relation-keyedTwo", "with the key 1"),
        ("Relation-keyedNumber", "holds its key, so it is a map"),
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
    assert!(!before.iter().any(|v| v.subject.contains("keyedOther")), "another key value is another edge: {before:?}");
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
    assert!(
        reopened
            .index_status()
            .unwrap()
            .iter()
            .find(|s| s.entity_type.as_deref() == Some("Task") && s.index == "a")
            .unwrap()
            .built
    );
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
                "types":{"Node":{"fields":{}}}, "relations":{"links":{"cardinality":cardinality, "key":["n"]}}
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
                // A key value each, so the second a-to-b edge is another edge, not a duplicate of the first
                info: Some(rmpv::Value::Map(vec![("n".into(), (i as i64).into())])),
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

#[test]
fn a_rebuild_clears_what_a_retired_tag_left_and_a_document_naming_no_field_of_its_index_is_refused() {
    let schema = json!({ "$schema": crate::schema::SCHEMA_LANGUAGE, "types": { "Doc": {
        "fields": { "body": { "type": "string" } },
        "indexes": { "words": { "kind": "text", "fields": { "body": {} } } }
    } } });
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(&dir, schema).unwrap();
    let body = |text: &str| json!({ "op": "set", "id": "Doc-1", "fields": { "body": text } });
    write(&db, vec![json!({ "op": "create", "type": "Doc", "id": "Doc-1", "fields": { "body": "the error text" } })])
        .unwrap();
    // A key as the layout before this one kept a term's positions under, which nothing reads now
    let retired = Prefix::retired().next().unwrap();
    let left = crate::format::key::with_number(&retired, 7.0);
    damage(&db, |txn, d| Ok(d.put(txn, &left, &[1])?));
    // A commit of the database's own, after which a snapshot is of the store as the damage left it
    write(&db, vec![body("the error text")]).unwrap();
    let under = |db: &Database| db.snapshot(|r| Ok(Read::scan(r, &retired)?.len())).unwrap();
    assert_eq!(under(&db), 1);
    db.rebuild(Some(("Doc", "words"))).unwrap();
    assert_eq!(under(&db), 1, "one index's rebuild clears that index's keys alone");
    db.rebuild(None).unwrap();
    assert_eq!(under(&db), 0);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());

    // The entity's document saying it holds a term in a tenth field of an index of one
    let document = db.snapshot(|r| Read::scan(r, &keys::text_index_ranges("Doc", "words")[2])).unwrap();
    let (key, _) = &document[0];
    let places = |field: u64| {
        let term = rmpv::Value::Array(vec![
            "error".into(),
            rmpv::Value::Array(vec![rmpv::Value::Array(vec![field.into(), rmpv::Value::Array(vec![1.into()])])]),
        ]);
        crate::engine::values::encode(&rmpv::Value::Array(vec![
            rmpv::Value::Array(vec![term]),
            rmpv::Value::Array(vec![]),
        ]))
    };
    damage(&db, |txn, d| Ok(d.put(txn, key, &places(9))?));
    let refused = write(&db, vec![body("another text")]).unwrap_err();
    assert_eq!(refused.code, ErrorCode::Corrupted);
    assert!(refused.message.contains("rebuild()"), "{}", refused.message);
    // Its only field is one it may name, and the rebuild the message asks for makes the index whole
    damage(&db, |txn, d| Ok(d.put(txn, key, &places(0))?));
    write(&db, vec![body("another text")]).unwrap();
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
}

#[test]
fn statistics_and_a_words_count_that_say_less_than_a_text_index_holds_are_refused_not_counted_down_from() {
    let schema = json!({ "$schema": crate::schema::SCHEMA_LANGUAGE, "types": { "Doc": {
        "fields": { "body": { "type": "string" } },
        "indexes": { "words": { "kind": "text", "fields": { "body": {} }, "stem": "en" } }
    } } });
    let dir = tempfile::tempdir().unwrap();
    let db = with_schema(&dir, schema).unwrap();
    let body = |text: &str| json!({ "op": "set", "id": "Doc-1", "fields": { "body": text } });
    write(&db, vec![json!({ "op": "create", "type": "Doc", "id": "Doc-1", "fields": { "body": "the error text" } })])
        .unwrap();
    let stats = keys::text_stats("Doc", "words");
    let counting = |documents: u64, length: u64| {
        crate::engine::values::encode(&rmpv::Value::Array(vec![
            documents.into(),
            rmpv::Value::Array(vec![length.into()]),
        ]))
    };
    let held = db.snapshot(|r| Read::get(r, &stats)).unwrap().unwrap();
    assert_eq!(held, counting(1, 3));
    // No document where the index holds one, and a field shorter in all than the entity's is: a write that refiles
    // the entity has less to take it from than it holds
    for damaged in [counting(0, 3), counting(1, 2)] {
        damage(&db, |txn, d| Ok(d.put(txn, &stats, &damaged)?));
        let refused = write(&db, vec![body("another text")]).unwrap_err();
        assert_eq!(refused.code, ErrorCode::Corrupted);
        assert!(refused.message.contains("statistics"), "{}", refused.message);
    }
    damage(&db, |txn, d| Ok(d.put(txn, &stats, &held)?));
    // A word as written the entity holds and no entity is counted under
    let word = keys::text_word("Doc", "words", "error");
    let counted = db.snapshot(|r| Read::get(r, &word)).unwrap().unwrap();
    damage(&db, |txn, d| Ok(d.delete(txn, &word).map(|_| ())?));
    let refused = write(&db, vec![body("another text")]).unwrap_err();
    assert_eq!(refused.code, ErrorCode::Corrupted);
    assert!(refused.message.contains("word"), "{}", refused.message);
    // As they were, the write goes through
    damage(&db, |txn, d| Ok(d.put(txn, &word, &counted)?));
    write(&db, vec![body("another text")]).unwrap();
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
