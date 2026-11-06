//! What changed since a position: an entity moves to its commit's place in its type's change order whatever wrote it, a
//! reader pages through a commit of any size, a destroy is reported once and then, trimmed, makes an old reader start
//! again, and the order is derived data that `check()` holds and `rebuild()` makes.

use serde_json::json;

use super::{batch, write};
use crate::engine::changed::{Changed, Position};
use crate::engine::{changed, keys};
use crate::error::ErrorCode;
use crate::store::meta_key;
use crate::{Database, OpenOptions};

const TYPES: [&str; 2] = ["Note", "Task"];

fn types() -> Vec<String> {
    TYPES.iter().map(|t| (*t).to_owned()).collect()
}

fn since(db: &Database, from: &Position, limit: usize) -> Changed {
    db.changed_since(&types(), from, limit).unwrap()
}

fn from(sequence: u64) -> Position {
    Position { sequence, after: None }
}

/// The suites' schema, Tasks and Notes keeping a change order
fn ordered() -> crate::schema::Schema {
    let mut json = super::schema_json();
    for entity_type in TYPES {
        json["types"][entity_type]["changeOrder"] = json!(true);
    }
    crate::schema::Schema::from_json(&json).unwrap()
}

fn open(dir: &tempfile::TempDir) -> Database {
    Database::open(dir.path(), OpenOptions { schema: Some(ordered()), syncs: false, ..Default::default() }).unwrap()
}

fn task(id: &str) -> serde_json::Value {
    json!({ "op": "create", "type": "Task", "id": id, "fields": { "title": "t" } })
}

#[test]
fn a_write_moves_what_it_wrote_to_its_commit_and_a_reader_reads_only_that() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1"), task("Task-2")]).unwrap();
    let after_create = db.sequence().unwrap();
    write(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "title": "a" } })]).unwrap();

    let all = since(&db, &from(0), 100);
    assert_eq!(all.ids, ["Task-2", "Task-1"], "change order: Task-2 last changed at the create, Task-1 after");
    assert!(all.done && all.complete);
    assert_eq!(all.next, from(db.sequence().unwrap()));

    assert!(db.check().unwrap().is_clean());
    let later = since(&db, &from(after_create), 100);
    assert_eq!(later.ids, ["Task-1"], "only what changed after the cursor");
    assert_eq!(since(&db, &all.next, 100).ids, Vec::<String>::new(), "nothing since");
    assert_eq!(db.changed_since(&types(), &from(0), 0).unwrap_err().code, ErrorCode::BatchInvalid);
}

#[test]
fn a_trash_a_restore_and_a_relation_at_either_end_move_an_entity() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1"), task("Task-2"), task("Task-3")]).unwrap();
    let moved = |ops: Vec<serde_json::Value>| {
        let cursor = db.sequence().unwrap();
        write(&db, ops).unwrap();
        since(&db, &from(cursor), 100).ids
    };
    assert_eq!(moved(vec![json!({ "op": "trash", "id": "Task-1" })]), ["Task-1"]);
    assert_eq!(moved(vec![json!({ "op": "restore", "id": "Task-1" })]), ["Task-1"]);
    assert_eq!(
        moved(vec![json!({ "op": "link", "source": "Task-2", "kind": "refs", "target": "Task-3" })]),
        ["Task-2", "Task-3"]
    );
    assert_eq!(
        moved(vec![json!({ "op": "unlink", "source": "Task-2", "kind": "refs", "target": "Task-3" })]),
        ["Task-2", "Task-3"]
    );
    assert!(db.check().unwrap().is_clean(), "each move left one place per entity");
}

#[test]
fn a_destroy_is_reported_once_and_leaves_no_entity_behind() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1"), task("Task-2")]).unwrap();
    let cursor = db.sequence().unwrap();
    write(&db, vec![json!({ "op": "destroy", "id": "Task-1" })]).unwrap();
    let changed = since(&db, &from(cursor), 100);
    assert_eq!((changed.ids.clone(), changed.destroyed.clone()), (vec![], vec!["Task-1".to_owned()]));
    assert_eq!(since(&db, &changed.next, 100).destroyed, Vec::<String>::new(), "once");
    assert!(db.check().unwrap().is_clean(), "the destroy took its change order entry with it");
}

#[test]
fn a_reader_pages_through_one_commit_larger_than_a_page() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": {} })]).unwrap();
    let cursor = db.sequence().unwrap();
    write(
        &db,
        (0..10)
            .map(|i| task(&format!("Task-{i}")))
            .chain([json!({ "op": "link", "source": "Note-1", "kind": "refs", "target": "Task-0" })])
            .collect(),
    )
    .unwrap();
    let mut position = from(cursor);
    let mut read = Vec::new();
    loop {
        let page = since(&db, &position, 3);
        assert!(page.ids.len() <= 3);
        read.extend(page.ids);
        position = page.next;
        if page.done {
            break;
        }
        assert!(position.after.is_some(), "a page that ended within the commit says where");
    }
    let mut expected: Vec<String> = (0..10).map(|i| format!("Task-{i}")).collect();
    expected.insert(0, "Note-1".to_owned());
    assert_eq!(
        read, expected,
        "every one once, in change order: the commit's Notes before its Tasks, each in its place"
    );
    assert_eq!(position, from(db.sequence().unwrap()));
}

#[test]
fn a_reader_older_than_the_kept_tombstones_is_told_it_may_have_missed_a_destroy() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1"), task("Task-2")]).unwrap();
    let old = db.sequence().unwrap();
    write(&db, vec![json!({ "op": "destroy", "id": "Task-1" })]).unwrap();
    let store = db.store().unwrap();
    for _ in 0..changed::TOMBSTONE_COMMITS + changed::TRIM_EVERY {
        store.write(|_, _| Ok(()), false).unwrap();
    }
    let stale = since(&db, &from(old), 100);
    assert!(!stale.complete, "its tombstone was trimmed");
    assert!(db.changed_since(&["Note".to_owned()], &from(old), 100).unwrap().complete, "no Note was destroyed");
    assert!(since(&db, &from(db.sequence().unwrap() - 1), 100).complete, "a reader since then missed nothing");
    assert!(since(&db, &from(old + 1), 100).complete, "a reader from the destroy's own commit has read it");
    // A reader exactly at what trimming dropped has every tombstone after it, a destroy since included
    let floor = store
        .read(|txn, d| Ok(d.get(txn, &meta_key(crate::engine::changed::DESTROYED_FLOOR))?.map(<[u8]>::to_vec)))
        .unwrap()
        .map(|bytes| u64::from_be_bytes(bytes.try_into().unwrap()))
        .expect("trimmed");
    write(&db, vec![json!({ "op": "destroy", "id": "Task-2" })]).unwrap();
    assert!(since(&db, &from(floor), 100).complete, "from the floor itself");
}

#[test]
fn the_tombstones_stay_within_their_bound() {
    use changed::{TOMBSTONE_COMMITS, TRIM_EVERY};
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-0")]).unwrap();
    // A destroy in every commit, stopped between trims
    for n in 1..TOMBSTONE_COMMITS + 3 * TRIM_EVERY + 5 {
        write(&db, vec![json!({ "op": "destroy", "id": format!("Task-{}", n - 1) }), task(&format!("Task-{n}"))])
            .unwrap();
    }
    let held = db
        .snapshot(|s| {
            let mut held = 0u64;
            crate::engine::unit::Read::visit(s, &crate::engine::keys::destroyed_prefix(), |_, _| {
                held += 1;
                true
            })?;
            Ok(held)
        })
        .unwrap();
    // Trimmed at the last multiple of TRIM_EVERY to the TOMBSTONE_COMMITS before it, and none trimmed since
    let sequence = db.sequence().unwrap();
    assert_eq!(
        held,
        TOMBSTONE_COMMITS + sequence % TRIM_EVERY + 1,
        "what it promises, and every destroy since it last trimmed"
    );
}

#[test]
fn the_change_order_is_derived_check_finds_it_damaged_and_rebuild_makes_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1"), task("Task-2")]).unwrap();
    let before = since(&db, &from(0), 100).ids;
    db.store()
        .unwrap()
        .write(
            |txn, d| {
                let keys = d
                    .main
                    .prefix_iter(txn, &crate::format::Prefix::Changed.key().to_vec())?
                    .map(|item| item.map(|(key, _)| key.to_vec()))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                d.main.delete(txn, &keys[0])?;
                d.main.put(txn, &keys::changed("Task", 999, 1), &crate::engine::values::encode(&"Task-1".into()))?;
                Ok(())
            },
            false,
        )
        .unwrap();
    let report = db.check().unwrap();
    let kinds: Vec<&str> = report.differences.iter().map(|d| d.kind.as_str()).collect();
    assert!(kinds.contains(&"missing") && kinds.contains(&"extra"), "{report:?}");
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean());
    assert_eq!(since(&db, &from(0), 100).ids, before);
}

#[test]
fn a_database_without_the_index_says_so_and_builds_it_as_it_opens() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(&dir);
        write(&db, vec![task("Task-1")]).unwrap();
        db.store()
            .unwrap()
            .write(
                |txn, d| {
                    d.main.delete(txn, &meta_key("changeOrder:Task"))?;
                    let order = d
                        .main
                        .prefix_iter(txn, &crate::format::Prefix::Changed.key().to_vec())?
                        .map(|item| item.map(|(key, _)| key.to_vec()))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    for key in order {
                        d.main.delete(txn, &key)?;
                    }
                    Ok(())
                },
                false,
            )
            .unwrap();
        assert!(!since(&db, &from(0), 100).complete, "what it would answer is missing entities");
        db.close().unwrap();
    }
    let db = open(&dir);
    assert!(db.check().unwrap().is_clean(), "the open built the order again");
    let built = db.sequence().unwrap();
    assert!(
        !db.changed_since(&["Task".to_owned()], &from(0), 100).unwrap().complete,
        "it can't say what changed before"
    );
    write(&db, vec![task("Task-2")]).unwrap();
    let since_built = db.changed_since(&["Task".to_owned()], &from(built), 100).unwrap();
    assert!(since_built.complete);
    assert_eq!(since_built.ids, ["Task-2"]);
}

#[test]
fn a_load_and_a_replacement_stamp_what_they_wrote_with_their_commit() {
    let (dir, source) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    {
        let other = open(&source);
        for i in 0..20 {
            write(&other, vec![task(&format!("Task-other-{i}"))]).unwrap();
        }
        other.close().unwrap();
    }
    let db = open(&dir);
    let cursor = db.sequence().unwrap();
    db.bulk_load(&[crate::engine::maintenance::Load::Entity {
        id: "Task-loaded".into(),
        fields: Default::default(),
        trashed: false,
    }])
    .unwrap();
    assert_eq!(since(&db, &from(cursor), 100).ids, ["Task-loaded"]);

    db.replace_with(source.path()).unwrap();
    let all = since(&db, &from(0), 100);
    assert_eq!(all.ids.len(), 20);
    assert!(all.next.sequence <= db.sequence().unwrap(), "no entity carries the other database's sequences");
    let replaced = db.sequence().unwrap();
    assert!(since(&db, &from(replaced - 1), 100).ids.len() == 20, "every one at the replacement's commit");
}

#[test]
fn a_unit_knows_the_sequence_it_commits_as_and_stamps_once_whatever_it_wrote() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1")]).unwrap();
    let cursor = db.sequence().unwrap();
    let mut unit = db.begin().unwrap();
    let next = unit.next_sequence().unwrap();
    unit.write(&batch(vec![json!({ "op": "set", "id": "Task-1", "fields": { "title": "a" } })])).unwrap();
    unit.write(&batch(vec![json!({ "op": "set", "id": "Task-1", "fields": { "title": "b" } })])).unwrap();
    assert_eq!(
        unit.changed_since(&types(), &from(cursor), 100).unwrap().ids,
        Vec::<String>::new(),
        "stamped only as it commits"
    );
    let committed = unit.commit(false).unwrap();
    assert_eq!(committed.sequence, next);
    assert_eq!(since(&db, &from(cursor), 100).ids, ["Task-1"]);
    assert!(since(&db, &from(next), 100).ids.is_empty(), "nothing after the commit that stamped it");
}

#[test]
fn a_type_keeps_an_order_only_as_its_schema_says_and_a_reader_of_one_that_doesnt_is_told_so() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Database::open(
            dir.path(),
            OpenOptions { schema: Some(super::schema()), syncs: false, ..Default::default() },
        )
        .unwrap();
        write(&db, vec![task("Task-1")]).unwrap();
        assert!(!since(&db, &from(0), 100).complete, "Task keeps no order");
        let order = db
            .store()
            .unwrap()
            .read(|txn, d| Ok(d.main.prefix_iter(txn, &crate::format::Prefix::Changed.key().to_vec())?.count()))
            .unwrap();
        assert_eq!(order, 0, "and pays for none");
        db.close().unwrap();
    }
    let db = open(&dir);
    let built_at = db.sequence().unwrap();
    write(&db, vec![task("Task-2")]).unwrap();
    assert!(!db.changed_since(&["Task".to_owned()], &from(0), 100).unwrap().complete, "built after a reader from 0");
    let since_built = db.changed_since(&["Task".to_owned()], &from(built_at), 100).unwrap();
    assert!(since_built.complete);
    assert_eq!(since_built.ids, ["Task-2"]);
    db.close().unwrap();

    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(super::schema()), syncs: false, ..Default::default() })
            .unwrap();
    assert!(db.check().unwrap().is_clean(), "the order went with the schema that kept it");
}

fn note(id: &str) -> serde_json::Value {
    json!({ "op": "create", "type": "Note", "id": id, "fields": {} })
}

#[test]
fn pages_that_end_within_one_commit_across_types_read_each_entity_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let start = db.sequence().unwrap();
    // Tasks first, so the Notes' places are past theirs though the Notes come first in the order
    write(&db, vec![task("Task-1"), task("Task-2"), task("Task-3"), note("Note-1"), note("Note-2"), note("Note-3")])
        .unwrap();
    for limit in 1..=4 {
        let (mut position, mut read) = (from(start), Vec::new());
        for _ in 0..20 {
            let page = since(&db, &position, limit);
            assert!(page.ids.len() <= limit, "a page holds at most its limit");
            read.extend(page.ids);
            position = page.next;
            if page.done {
                break;
            }
        }
        assert_eq!(read, ["Note-1", "Note-2", "Note-3", "Task-1", "Task-2", "Task-3"], "in pages of {limit}");
    }
}

#[test]
fn a_page_ending_within_a_commit_names_what_that_commit_destroyed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-gone")]).unwrap();
    let start = db.sequence().unwrap();
    write(&db, vec![json!({ "op": "destroy", "id": "Task-gone" }), task("Task-1"), task("Task-2"), task("Task-3")])
        .unwrap();
    let page = since(&db, &from(start), 2);
    assert!(!page.done);
    assert_eq!(page.next.sequence, db.sequence().unwrap(), "the page ends within the destroying commit");
    assert_eq!(page.destroyed, ["Task-gone"]);
}

#[test]
fn a_reader_from_the_commit_an_order_was_built_at_is_told_everything() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1")]).unwrap();
    let built = db
        .store()
        .unwrap()
        .read(|txn, d| Ok(d.get(txn, &meta_key("changeOrder:Task"))?.map(<[u8]>::to_vec)))
        .unwrap()
        .map(|bytes| u64::from_be_bytes(bytes.try_into().unwrap()))
        .expect("Task keeps an order");
    assert!(since(&db, &from(built), 10).complete, "from the commit it was built at");
}

#[test]
fn a_type_that_keeps_no_order_is_given_none_at_open() {
    let dir = tempfile::tempdir().unwrap();
    let mut json = super::schema_json();
    json["types"]["Task"]["changeOrder"] = json!(true);
    let schema = crate::schema::Schema::from_json(&json).unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap();
    write(&db, vec![note("Note-1"), task("Task-1")]).unwrap();
    db.close().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions {
            schema: Some(crate::schema::Schema::from_json(&json).unwrap()),
            syncs: false,
            ..Default::default()
        },
    )
    .unwrap();
    let held = |entity_type: &str| {
        db.snapshot(|s| {
            let mut held = 0;
            crate::engine::unit::Read::visit(s, &keys::changed_of_type(entity_type), |_, _| {
                held += 1;
                true
            })?;
            Ok(held)
        })
        .unwrap()
    };
    assert_eq!((held("Note"), held("Task")), (0, 1), "only the type that asks keeps one");
}

#[test]
fn what_the_binding_reads_is_what_changed_in_msgpack_from_the_position_it_gave() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1")]).unwrap();
    let start = db.sequence().unwrap();
    write(&db, vec![task("Task-2")]).unwrap();
    let position = rmp_serde::to_vec_named(&from(start)).unwrap();
    let decoded: Changed = rmp_serde::from_slice(&db.changed_since_bytes(&types(), &position, 10).unwrap()).unwrap();
    assert_eq!(decoded, since(&db, &from(start), 10));
    assert_eq!(decoded.ids, ["Task-2"], "from the position given, not the start");
    let mut unit = db.begin().unwrap();
    let read = unit.changed_since(&types(), &from(start), 10).unwrap();
    assert_eq!(read.ids, ["Task-2"], "a unit reads what changed too");
    let decoded: Changed = rmp_serde::from_slice(&unit.changed_since_bytes(&types(), &position, 10).unwrap()).unwrap();
    assert_eq!(decoded, read);
}
