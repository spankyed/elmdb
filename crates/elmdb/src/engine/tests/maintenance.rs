//! A database's maintenance: backups, replacing and emptying its data, what it holds, unchecked loads, and dumps.

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

/// The live entities of `types`, in the order they were made
fn made(db: &Database, types: &[&str]) -> serde_json::Value {
    use crate::engine::plan::{ReadOp, Reads};
    let op = json!({ "read": "query", "plan": { "start": { "from": "types", "types": types }, "steps": [], "terminal": { "answer": "ids" } } });
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    let bytes = db.read(&reads).unwrap();
    serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()[0]["ids"].clone()
}

fn loads_of(db: &Database) -> Vec<Load> {
    let mut loads = Vec::new();
    db.primary_loads(|load| {
        loads.push(load);
        Ok(())
    })
    .unwrap();
    loads
}

#[test]
fn a_dump_loads_into_a_new_database_holding_what_the_first_held_in_the_order_it_was_made() {
    let work = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![
        json!({ "op": "create", "type": "Task", "id": "Task-c", "fields": { "title": "made first" } }),
        json!({ "op": "create", "type": "Task", "id": "Task-b", "fields": { "title": "made second", "slug": "b" } }),
        json!({ "op": "create", "type": "Task", "id": "Task-a", "fields": { "title": "made third", "a": [1, { "x": null }] } }),
        json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "a": "words to find", "status": "open" } }),
        json!({ "op": "link", "kind": "refs", "source": "Task-b", "target": "Note-1", "info": { "w": 2 } }),
        json!({ "op": "link", "kind": "refs", "source": "Task-a", "target": "Note-1" }),
        json!({ "op": "trash", "id": "Task-a" }),
    ])
    .unwrap();
    db.bulk_load(&[Load::Entity {
        id: "Bag-1".into(),
        fields: BTreeMap::from([("blob".to_owned(), rmpv::Value::Binary(vec![0, 255, 7]))]),
        trashed: false,
    }])
    .unwrap();
    let path = work.path().join("db.dump");
    let dumped = crate::dump::dump(&db, &path).unwrap();
    assert_eq!((dumped.entities, dumped.relations, dumped.schema), (5, 2, true), "{dumped:?}");
    let target = work.path().join("loaded");
    assert_eq!(crate::dump::load(&path, &target).unwrap(), dumped);
    let loaded = Database::open(&target, OpenOptions { syncs: false, ..Default::default() }).unwrap();
    assert_eq!(loaded.schema(), db.schema());
    assert_eq!(made(&loaded, &["Task"]), json!(["Task-c", "Task-b"]));
    assert_eq!(
        loads_of(&loaded),
        loads_of(&db),
        "the same entities, fields, relations and sequences, in the same order"
    );
    assert!(loaded.check().unwrap().is_clean(), "derived data built: {:?}", loaded.check().unwrap());
    assert_eq!(loaded.stats().unwrap().types["Task"].trashed, 1);
    // The numbering goes on where it was
    let next = write(&loaded, vec![json!({ "op": "create", "type": "Task" })]).unwrap();
    assert!(matches!(&next[0], crate::OpResult::Entity { short_code: Some(c), .. } if c == "TSK-4"), "{next:?}");
}

#[test]
fn a_dump_and_a_load_each_write_something_new_and_a_damaged_dump_or_a_failed_load_leaves_nothing() {
    let work = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })]).unwrap();
    let path = work.path().join("db.dump");
    crate::dump::dump(&db, &path).unwrap();
    assert_eq!(crate::dump::dump(&db, &path).unwrap_err().code, crate::ErrorCode::IdInUse, "a dump writes a new file");
    let target = work.path().join("a");
    crate::dump::load(&path, &target).unwrap();
    assert_eq!(crate::dump::load(&path, &target).unwrap_err().code, crate::ErrorCode::IdInUse, "a load a new database");

    let bytes = std::fs::read(&path).unwrap();
    let damaged = |at: usize, with: &[u8]| {
        let mut copy = bytes.clone();
        copy[at..at + with.len()].copy_from_slice(with);
        let damaged = work.path().join(format!("damaged-{at}.dump"));
        std::fs::write(&damaged, copy).unwrap();
        let target = work.path().join(format!("damaged-{at}"));
        let error = crate::dump::load(&damaged, &target).unwrap_err();
        assert!(!target.join("data.mdb").exists(), "nothing written for {error}");
        error
    };
    assert_eq!(damaged(0, b"ELMDBF4\0").code, crate::ErrorCode::WrongFormat);
    let other_version = damaged(8, &2u32.to_le_bytes());
    assert!(other_version.message.contains("dump version 2"), "{other_version}");
    // The schema's length, past the end of the file
    assert_eq!(damaged(12, &(u32::MAX - 1).to_le_bytes()).code, crate::ErrorCode::WrongFormat);
    let truncated = work.path().join("truncated.dump");
    std::fs::write(&truncated, &bytes[..bytes.len() - 3]).unwrap();
    assert_eq!(crate::dump::load(&truncated, &work.path().join("t")).unwrap_err().code, crate::ErrorCode::WrongFormat);

    // An id longer than LMDB's keys can hold fails the load, which leaves no database behind and can run again
    let failing = work.path().join("failing.dump");
    let mut file = bytes.clone();
    let end = file.len() - 12;
    let long = rmp_serde::to_vec_named(&Load::Entity {
        id: format!("Task-{}", "x".repeat(600)),
        fields: BTreeMap::new(),
        trashed: false,
    })
    .unwrap();
    let mut framed = (long.len() as u32).to_le_bytes().to_vec();
    framed.extend(long);
    file.splice(end..end, framed);
    let count = u64::from_le_bytes(file[file.len() - 8..].try_into().unwrap()) + 1;
    let at = file.len() - 8;
    file[at..].copy_from_slice(&count.to_le_bytes());
    std::fs::write(&failing, &file).unwrap();
    let target = work.path().join("c");
    assert!(crate::dump::load(&failing, &target).is_err());
    assert!(!target.join("data.mdb").exists(), "a failed load leaves no database behind");
    crate::dump::load(&path, &target).expect("the target takes a second load");
}
