//! A database's maintenance: backups, replacing and emptying its data, what it holds, unchecked loads, and the upgrade
//! from a format 4 dump.

use std::collections::BTreeMap;

use serde_json::json;

use super::{edges, open, row, write};
use crate::engine::maintenance::Load;
use crate::{Database, OpenOptions};

#[test]
fn a_compacted_backup_of_multi_page_values_can_be_emptied_in_one_transaction() {
    // The sequence that aborted lmdb-js 3.5.9 (kriszyp/lmdb-js#381): a compacted copy of a database whose values span
    // several pages, reopened and emptied in one transaction. Tests build LMDB with its assertions compiled in
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let big = "x".repeat(20_000);
    for chunk in 0..10 {
        let ops = (0..100).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{chunk}-{i}"), "fields": { "title": big } })).collect();
        write(&db, ops).unwrap();
    }
    let copy = tempfile::tempdir().unwrap();
    let target = copy.path().join("backup");
    db.backup(&target).unwrap();
    db.close().unwrap();
    let restored = Database::open(&target, OpenOptions { syncs: false, ..Default::default() }).unwrap();
    assert_eq!(row(&restored, "Task-3-7").unwrap()["title"].as_str().unwrap().len(), 20_000);
    restored.reset().unwrap();
    assert!(row(&restored, "Task-3-7").is_none());
    assert_eq!(restored.schema(), Some(super::schema()), "a reset keeps the schema");
    write(&restored, vec![json!({ "op": "create", "type": "Task", "id": "Task-after" })]).unwrap();
    assert!(restored.check().unwrap().is_clean());
}

#[test]
fn a_backup_is_one_moment_compacted_and_opens_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let big = "y".repeat(5000);
    write(
        &db,
        (0..400)
            .map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}"), "fields": { "title": big } }))
            .collect(),
    )
    .unwrap();
    write(&db, (0..400).map(|i| json!({ "op": "destroy", "id": format!("Task-{i}") })).collect()).unwrap();
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-kept" })]).unwrap();
    let target = tempfile::tempdir().unwrap();
    db.backup(target.path().join("b")).unwrap();
    let copied = std::fs::metadata(target.path().join("b/data.mdb")).unwrap().len();
    let original = std::fs::metadata(dir.path().join("data.mdb")).unwrap().len();
    assert!(copied < original, "the copy leaves out the freed pages: {copied} against {original}");
    let restored =
        Database::open(target.path().join("b"), OpenOptions { read_only: true, ..Default::default() }).unwrap();
    assert!(row(&restored, "Task-kept").is_some());
    assert!(row(&restored, "Task-0").is_none());
}

#[test]
fn replace_with_takes_another_databases_data_and_rebuilds_under_this_schema() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let source = open(&b);
    write(
        &source,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-src", "fields": { "a": 5 } }),
            json!({ "op": "create", "type": "Task", "id": "Task-two" }),
            json!({ "op": "link", "source": "Task-src", "kind": "refs", "target": "Task-two" }),
        ],
    )
    .unwrap();
    source.close().unwrap();
    let db = open(&a);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-mine" })]).unwrap();
    db.replace_with(b.path()).unwrap();
    assert!(row(&db, "Task-mine").is_none());
    assert_eq!(row(&db, "Task-src").unwrap()["a"], 5);
    assert_eq!(edges(&db), ["Task-src|refs|Task-two"]);
    assert!(db.check().unwrap().is_clean());
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(db.replace_with(empty.path()).unwrap_err().code, crate::ErrorCode::NoDatabase);
}

#[test]
fn stats_count_entities_fields_relations_and_index_entries() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": 1 } }),
            json!({ "op": "create", "type": "Task", "id": "Task-2", "fields": { "a": 2 } }),
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" }),
            json!({ "op": "trash", "id": "Task-2" }),
        ],
    )
    .unwrap();
    let stats = db.stats().unwrap();
    assert_eq!((stats.types["Task"].live, stats.types["Task"].trashed), (1, 1));
    assert_eq!(stats.types["Task"].fields["a"].entities, 2);
    assert_eq!(stats.relations["refs"], 1);
    assert_eq!(stats.indexes["Task.a"], 2);
    assert!(stats.file_bytes > 0 && stats.map_bytes >= stats.file_bytes);
}

#[test]
fn a_bulk_load_takes_rows_the_checks_would_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let fields = |pairs: &[(&str, rmpv::Value)]| {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<BTreeMap<_, _>>()
    };
    db.bulk_load(&[
        Load::Entity { id: "Ghost-1".into(), fields: fields(&[("anything", 1.into())]), trashed: false },
        Load::Entity { id: "Task-1".into(), fields: fields(&[("shortCode", "TSK-1".into())]), trashed: false },
        Load::Entity { id: "Task-2".into(), fields: fields(&[("shortCode", "TSK-1".into())]), trashed: true },
        Load::Relation {
            id: "Relation-x".into(),
            kind: "gone".into(),
            source: "Task-1".into(),
            target: "Task-none".into(),
            info: None,
        },
        Load::Sequence { name: "shortCode:Task".into(), next: 10 },
    ])
    .unwrap();
    assert_eq!(row(&db, "Ghost-1").unwrap()["anything"], 1);
    let next = write(&db, vec![json!({ "op": "create", "type": "Task" })]).unwrap();
    assert!(matches!(&next[0], crate::OpResult::Entity { short_code: Some(c), .. } if c == "TSK-10"), "{next:?}");
    let report = db.check().unwrap();
    assert!(report.differences.is_empty(), "{report:?}");
    let said: Vec<&str> = report.violations.iter().map(|v| v.message.as_str()).collect();
    assert!(said.iter().any(|m| m.contains("which is unique")), "{said:?}");
    assert!(said.iter().any(|m| m.contains("Task-none")), "{said:?}");
}

#[test]
fn an_unset_load_removes_any_field_and_keeps_derived_data_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": 1 } }),
            json!({ "op": "trash", "id": "Task-1" }),
        ],
    )
    .unwrap();
    assert_eq!(db.stats().unwrap().types["Task"].trashed, 1);
    db.bulk_load(&[
        Load::Unset { id: "Task-1".into(), field: "a".into() },
        Load::Unset { id: "Task-1".into(), field: "trashedAt".into() },
        Load::Unset { id: "Task-1".into(), field: "createdAt".into() },
        Load::Unset { id: "Task-none".into(), field: "a".into() },
    ])
    .unwrap();
    let held = row(&db, "Task-1").unwrap();
    for gone in ["a", "trashedAt", "createdAt"] {
        assert!(held.get(gone).is_none(), "{gone} is still held: {held}");
    }
    let stats = db.stats().unwrap();
    assert_eq!((stats.types["Task"].live, stats.types["Task"].trashed), (1, 0), "out of the trash");
    assert!(row(&db, "Task-none").is_none(), "an unset makes no entity");
    assert!(db.check().unwrap().differences.is_empty());
}

/// A format 4 dump of these entries, as `tools/lmdb-js-export` writes one
fn dump(entries: &[(Vec<crate::format::key::Part>, rmpv::Value)]) -> Vec<u8> {
    let mut sorted: Vec<(Vec<u8>, Vec<u8>)> =
        entries.iter().map(|(k, v)| (crate::format::key::encode(k), crate::engine::encode_value(v))).collect();
    sorted.sort();
    let mut out = b"ELMDBF4\0".to_vec();
    out.extend(1u32.to_le_bytes());
    for (k, v) in &sorted {
        out.extend((k.len() as u32).to_le_bytes());
        out.extend(k);
        out.extend((v.len() as u32).to_le_bytes());
        out.extend(v);
    }
    out.extend(u32::MAX.to_le_bytes());
    out.extend((sorted.len() as u64).to_le_bytes());
    out
}

#[test]
fn upgrade_reads_a_format_4_dump_mapping_its_keys_and_rebuilding_what_derives() {
    use crate::format::key::Part;
    use rmpv::Value;
    let s = |x: &str| Part::from(x);
    let map = |pairs: Vec<(&str, Value)>| Value::Map(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect());
    let entries = vec![
        (vec![s("i"), s("Task-1")], map(vec![("seq", 1.into()), ("trashed", false.into())])),
        (vec![s("i"), s("Task-2")], map(vec![("seq", 2.into()), ("trashed", true.into())])),
        (vec![s("v"), s("Task-1"), s("title")], "first".into()),
        (vec![s("v"), s("Task-1"), s("shortCode")], "TSK-1".into()),
        (vec![s("v"), s("Task-2"), s("trashedAt")], 9.into()),
        (
            vec![s("r"), s("Relation-1")],
            map(vec![
                ("kind", "refs".into()),
                ("source", "Task-1".into()),
                ("target", "Task-2".into()),
                ("info", Value::Ext(0, vec![0])),
                ("seq", 3.into()),
            ]),
        ),
        (vec![s("s"), s("Task:TSK")], 2.into()),
        (vec![s("m"), s("seq")], 3.into()),
        // Format 4's derived keys, which the upgrade rebuilds rather than copies
        (vec![s("e"), s("Task"), Part::Number(1.0)], "Task-1".into()),
        (vec![s("c"), s("Task")], 1.into()),
    ];
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("f4.dump");
    std::fs::write(&path, dump(&entries)).unwrap();
    let target = work.path().join("db");
    let report = crate::upgrade::upgrade(&path, &target, Some(super::schema())).unwrap();
    assert_eq!((report.entities, report.attributes, report.relations, report.sequences), (2, 3, 1, 2));
    assert_eq!(report.skipped.values().sum::<u64>(), 2);
    let db = Database::open(&target, OpenOptions { syncs: false, ..Default::default() }).unwrap();
    assert_eq!(row(&db, "Task-1").unwrap()["title"], "first");
    assert!(row(&db, "Task-2").unwrap().get("trashedAt").is_some());
    assert_eq!(edges(&db), ["Task-1|refs|Task-2"]);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    // The short code's sequence carried over under elmdb's name
    let next = write(&db, vec![json!({ "op": "create", "type": "Task" })]).unwrap();
    assert!(matches!(&next[0], crate::OpResult::Entity { short_code: Some(c), .. } if c == "TSK-2"), "{next:?}");
    assert_eq!(
        crate::upgrade::upgrade(&path, &target, None).unwrap_err().code,
        crate::ErrorCode::IdInUse,
        "an upgrade writes a new database"
    );
}

#[test]
fn upgrade_skips_fields_with_no_entity_refuses_a_damaged_dump_and_leaves_nothing_when_it_fails() {
    use crate::format::key::Part;
    use rmpv::Value;
    let s = |x: &str| Part::from(x);
    let map = |pairs: Vec<(&str, Value)>| Value::Map(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect());
    let task = (vec![s("i"), s("Task-1")], map(vec![("seq", 1.into())]));
    let work = tempfile::tempdir().unwrap();

    let orphaned = work.path().join("orphaned.dump");
    std::fs::write(&orphaned, dump(&[task.clone(), (vec![s("v"), s("Task-9"), s("title")], "lost".into())])).unwrap();
    let report = crate::upgrade::upgrade(&orphaned, &work.path().join("a"), Some(super::schema())).unwrap();
    assert_eq!(report.attributes, 0);
    assert_eq!(report.skipped.get("v"), Some(&1), "{report:?}");

    // A length past the end of the dump
    let mut damaged = dump(std::slice::from_ref(&task));
    damaged[12..16].copy_from_slice(&(u32::MAX - 1).to_le_bytes());
    let damaged_path = work.path().join("damaged.dump");
    std::fs::write(&damaged_path, damaged).unwrap();
    let error = crate::upgrade::upgrade(&damaged_path, &work.path().join("b"), None).unwrap_err();
    assert_eq!(error.code, crate::ErrorCode::WrongFormat, "{error}");

    // An id longer than LMDB's keys can hold fails the load
    let long_id = format!("Task-{}", "x".repeat(600));
    let failing = work.path().join("failing.dump");
    std::fs::write(&failing, dump(&[task.clone(), (vec![s("i"), s(&long_id)], map(vec![("seq", 2.into())]))])).unwrap();
    let target = work.path().join("c");
    assert!(crate::upgrade::upgrade(&failing, &target, None).is_err());
    assert!(!target.join("data.mdb").exists(), "a failed upgrade leaves no database behind");
    std::fs::write(&failing, dump(&[task])).unwrap();
    crate::upgrade::upgrade(&failing, &target, None).expect("the target takes a second upgrade");
}

#[test]
fn upgrade_steps_a_format_1_database_rebuilding_its_text_indexes_with_the_words_as_written() {
    use crate::format::{Prefix, meta};
    use crate::store::{Store, StoreOptions, put_meta};
    let schema = crate::schema::Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "types": { "Doc": {
            "fields": { "body": { "type": "string", "optional": true } },
            "indexes": { "stemmed": { "kind": "text", "fields": { "body": {} }, "stem": "en" } }
        } }
    }))
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap();
    write(&db, vec![json!({ "op": "create", "type": "Doc", "id": "Doc-1", "fields": { "body": "running late" } })])
        .unwrap();
    db.close().unwrap();
    // Format 1's text index: no words as written, and a document of its id, lengths and terms only
    let store = Store::open(dir.path(), StoreOptions { syncs: false, ..Default::default() }).unwrap();
    store
        .write(
            |txn, db| {
                let under = |prefix: Prefix| crate::format::key::prefix_range(&prefix.key().to_vec());
                let (low, high) = under(Prefix::TextWord);
                let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
                let words: Vec<Vec<u8>> =
                    db.range(txn, &range)?.map(|e| e.map(|(k, _)| k.to_vec())).collect::<heed::Result<_>>()?;
                assert!(!words.is_empty());
                for key in words {
                    db.delete(txn, &key)?;
                }
                let (low, high) = under(Prefix::TextDocument);
                let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
                let documents: Vec<(Vec<u8>, Vec<u8>)> = db
                    .range(txn, &range)?
                    .map(|e| e.map(|(k, v)| (k.to_vec(), v.to_vec())))
                    .collect::<heed::Result<_>>()?;
                for (key, stored) in documents {
                    let mut items = crate::engine::values::decode(&stored)?.as_array().unwrap().clone();
                    items.truncate(3);
                    db.put(txn, &key, &crate::engine::values::encode(&rmpv::Value::Array(items)))?;
                }
                put_meta(txn, db.main, meta::FORMAT, &1u32.to_be_bytes())
            },
            false,
        )
        .unwrap();
    store.close().unwrap();

    let refused = Database::open(dir.path(), OpenOptions { syncs: false, ..Default::default() }).err().unwrap();
    assert_eq!(refused.code, crate::ErrorCode::WrongFormat);
    let report = crate::upgrade::upgrade_format(dir.path()).unwrap();
    assert_eq!((report.from, report.to, report.rebuilt.as_slice()), (1, 2, ["Doc.stemmed".to_owned()].as_slice()));
    let db = Database::open(dir.path(), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    let reads = crate::engine::plan::Reads {
        reads: vec![
            serde_json::from_value(json!({ "read": "query", "plan": {
            "start": { "from": "types", "types": ["Doc"] },
            "steps": [{ "step": "search", "query": "runni", "options": { "prefix": true } }],
            "terminal": { "answer": "ids" }
        } }))
            .unwrap(),
        ],
        track: None,
    };
    let answer = db.read(&reads).unwrap();
    let found: serde_json::Value = serde_json::to_value(rmpv::decode::read_value(&mut &answer[..]).unwrap()).unwrap();
    assert_eq!(found[0]["ids"], json!(["Doc-1"]));
}
