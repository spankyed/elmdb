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
fn a_reader_paging_from_the_start_across_types_reads_each_unstamped_entity_once() {
    let dir = tempfile::tempdir().unwrap();
    {
        let unordered = crate::schema::Schema::from_json(&super::schema_json()).unwrap();
        let db =
            Database::open(dir.path(), OpenOptions { schema: Some(unordered), syncs: false, ..Default::default() })
                .unwrap();
        let note = |id: &str| json!({ "op": "create", "type": "Note", "id": id, "fields": {} });
        write(&db, vec![note("Note-1"), note("Note-2"), task("Task-1"), task("Task-2"), task("Task-3")]).unwrap();
        db.close().unwrap();
    }
    let db = open(&dir);
    let mut position = from(0);
    let mut read = Vec::new();
    for _ in 0..10 {
        let page = since(&db, &position, 2);
        read.extend(page.ids);
        position = page.next;
        if page.done {
            break;
        }
    }
    read.sort();
    assert_eq!(read, ["Note-1", "Note-2", "Task-1", "Task-2", "Task-3"], "each once, in pages of two");
}

#[test]
fn each_entity_read_comes_with_the_sequence_of_the_commit_that_last_changed_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1")]).unwrap();
    let first = db.sequence().unwrap();
    write(&db, vec![task("Task-2"), task("Task-3")]).unwrap();
    let second = db.sequence().unwrap();
    write(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "title": "moved" } })]).unwrap();
    let third = db.sequence().unwrap();
    let all = since(&db, &from(first - 1), 100);
    assert_eq!(all.ids, ["Task-2", "Task-3", "Task-1"]);
    assert_eq!(all.sequences, [second, second, third], "one each, in the same order");
    assert_eq!(since(&db, &from(first - 1), 2).sequences.len(), 2, "as many as the page holds");
}

#[test]
fn a_reader_from_the_start_reads_the_entities_a_type_held_before_it_kept_an_order() {
    let dir = tempfile::tempdir().unwrap();
    {
        let unordered = crate::schema::Schema::from_json(&super::schema_json()).unwrap();
        let db =
            Database::open(dir.path(), OpenOptions { schema: Some(unordered), syncs: false, ..Default::default() })
                .unwrap();
        write(&db, vec![task("Task-before-1"), task("Task-before-2")]).unwrap();
        db.close().unwrap();
    }
    // Kept in order only now: what it held then no commit stamped
    let db = open(&dir);
    write(&db, vec![task("Task-after")]).unwrap();
    let all = since(&db, &from(0), 100);
    assert_eq!(all.ids, ["Task-before-1", "Task-before-2", "Task-after"], "everything, the unstamped first");
    assert!(!all.complete, "a reader from before the order was built can't know what was destroyed");
    assert_eq!(
        since(&db, &from(1), 100).ids,
        ["Task-after"],
        "a reader past the start has read what no commit stamped"
    );
}

#[test]
fn a_reset_makes_a_reader_from_before_it_incomplete_and_one_from_before_its_next_commit_complete() {
    // A reset removes the entities with no tombstone, so a reader from before it is told the answer is incomplete; the next
    // commit is the first thing a reader from after it reads
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1")]).unwrap();
    db.reset().unwrap();
    write(&db, vec![task("Task-2")]).unwrap();
    let written = db.sequence().unwrap();
    let after = since(&db, &from(written - 1), 100);
    assert_eq!(after.ids, ["Task-2"], "from before the commit after the reset, that commit is what is read");
    assert_eq!(after.sequences, [written]);
    assert!(after.done && after.complete);
    assert!(!since(&db, &from(written - 2), 100).complete, "a reader from before the reset is told it is incomplete");
    assert!(!since(&db, &from(0), 100).complete, "a reader from the start is told it is incomplete");
}

#[test]
fn a_rebuild_takes_a_sequence_and_stamps_no_entity() {
    // The testing note relies on this: a rebuild between a reset and the next commit can take a sequence, and a reader from
    // before the rebuild reads nothing it changed
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1")]).unwrap();
    let before = db.sequence().unwrap();
    db.rebuild(None).unwrap();
    assert!(db.sequence().unwrap() > before, "the rebuild consumed a sequence");
    let since = since(&db, &from(before), 100);
    assert_eq!(since.ids, Vec::<String>::new(), "a rebuild stamps no entity");
    assert!(since.complete);
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

#[test]
fn a_schema_change_leaves_an_order_built_from_when_it_was_built() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1")]).unwrap();
    let before = db.sequence().unwrap();
    write(&db, vec![task("Task-2")]).unwrap();
    db.close().unwrap();
    // Reopened under a schema changed elsewhere, which settles every type's order again
    let mut json = super::schema_json();
    for entity_type in TYPES {
        json["types"][entity_type]["changeOrder"] = json!(true);
    }
    json["types"]["Kept"] = json!({ "fields": {} });
    let changed = crate::schema::Schema::from_json(&json).unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(changed), syncs: false, ..Default::default() }).unwrap();
    let read = since(&db, &from(before), 10);
    assert!(read.complete, "the order reaches back as far as it did before the schema changed");
    assert_eq!(read.ids, ["Task-2"]);
}

/// The relation a link answered
fn linked(db: &Database, source: &str, target: &str) -> String {
    let op = json!({ "op": "link", "source": source, "kind": "refs", "target": target });
    match &write(db, vec![op]).unwrap()[0] {
        crate::OpResult::Relation { id, .. } => id.clone(),
        other => panic!("not a relation: {other:?}"),
    }
}

#[test]
fn a_relation_moved_to_other_ends_moves_the_ends_it_left_and_its_change_names_them() {
    use crate::engine::changes::Ends;
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1"), task("Task-2"), task("Task-3"), task("Task-4")]).unwrap();
    let relation = linked(&db, "Task-1", "Task-2");
    let ends = |source: &str, target: &str| Ends { source: source.into(), target: target.into() };
    let moved = |ops: Vec<serde_json::Value>| {
        let cursor = db.sequence().unwrap();
        let changes = super::write_changes(&db, ops);
        (since(&db, &from(cursor), 100).ids, changes.relations[&relation].clone())
    };

    // Its target alone: the target it left has one edge fewer, as an unlink would leave it
    let (ids, change) = moved(vec![json!({ "op": "updateRelation", "relation": relation, "target": "Task-3" })]);
    assert_eq!(ids, ["Task-1", "Task-2", "Task-3"]);
    assert_eq!((change.source.as_str(), change.target.as_str()), ("Task-1", "Task-3"));
    assert_eq!(change.moved_from, Some(ends("Task-1", "Task-2")));

    // Both ends, in two moves of one commit: the ends it had as the commit began
    let (ids, change) = moved(vec![
        json!({ "op": "updateRelation", "relation": relation, "source": "Task-4" }),
        json!({ "op": "updateRelation", "relation": relation, "target": "Task-2" }),
    ]);
    assert_eq!(ids, ["Task-1", "Task-2", "Task-3", "Task-4"]);
    assert_eq!((change.source.as_str(), change.target.as_str()), ("Task-4", "Task-2"));
    assert_eq!(change.moved_from, Some(ends("Task-1", "Task-3")));

    // Its info alone moves the ends it has and names no move
    let info = json!({ "op": "updateRelation", "relation": relation, "info": { "n": 1 } });
    let (ids, change) = moved(vec![info]);
    assert_eq!(ids, ["Task-2", "Task-4"]);
    assert_eq!(change.moved_from, None);
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_reader_from_before_every_row_was_replaced_is_told_to_start_again() {
    let (dir, source) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    {
        let other = open(&source);
        write(&other, vec![task("Task-other")]).unwrap();
        other.close().unwrap();
    }
    let db = open(&dir);
    write(&db, vec![task("Task-1"), task("Task-2")]).unwrap();

    // A reset destroys what the reader read and keeps no tombstone of it
    let before = db.sequence().unwrap();
    db.reset().unwrap();
    let reset = db.sequence().unwrap();
    let read = since(&db, &from(before), 100);
    assert!(read.ids.is_empty() && read.destroyed.is_empty());
    assert!(!read.complete, "what the reset removed is named nowhere");
    assert!(since(&db, &from(reset), 100).complete, "a reader from the reset on has missed nothing");

    // A replacement stamps what it brought, and says as little of what it took away
    write(&db, vec![task("Task-3")]).unwrap();
    let before = db.sequence().unwrap();
    db.replace_with(source.path()).unwrap();
    let replaced = db.sequence().unwrap();
    let read = since(&db, &from(before), 100);
    assert_eq!(read.ids, ["Task-other"]);
    assert!(!read.complete, "Task-3 went without a tombstone");
    assert!(since(&db, &from(replaced), 100).complete);
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_reader_from_before_a_type_was_renamed_is_told_to_start_again() {
    let mut json = super::schema_json();
    json["types"]["Memo"] = json["types"]["Note"].clone();
    for entity_type in ["Note", "Memo", "Task"] {
        json["types"][entity_type]["changeOrder"] = json!(true);
    }
    json["types"]["Task"]["fields"]["about"] = json!({ "type": "ref", "to": ["Note", "Memo"], "optional": true });
    let schema = crate::schema::Schema::from_json(&json).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap();
    write(&db, vec![note("Note-1"), note("Note-2"), task("Task-1"), task("Task-2"), task("Task-3")]).unwrap();
    // Two tasks name a note, one by an edge and one by a field declared to hold its id
    write(
        &db,
        vec![
            json!({ "op": "link", "source": "Task-2", "kind": "refs", "target": "Note-1" }),
            json!({ "op": "set", "id": "Task-3", "fields": { "about": "Note-2" } }),
        ],
    )
    .unwrap();
    let before = db.sequence().unwrap();
    db.rename_type("Note", "Memo").unwrap();
    let renamed = db.sequence().unwrap();
    let of = |types: &[&str], sequence: u64| {
        let types: Vec<String> = types.iter().map(|t| (*t).to_owned()).collect();
        db.changed_since(&types, &from(sequence), 100).unwrap()
    };
    // The ids a reader held are gone and the ones in their place carry the stamps the old ones had
    assert!(!of(&["Note"], before).complete, "Note-1 and Note-2 went without a tombstone");
    assert!(!of(&["Memo"], before).complete, "Memo-1 and Memo-2 came without a stamp");
    // A type the rename left alone reads on, and is told of its entities whose edge or field named a renamed one
    let tasks = of(&["Task"], before);
    assert!(tasks.complete);
    assert_eq!(tasks.ids, ["Task-2", "Task-3"]);
    assert_eq!(super::row(&db, "Task-3").unwrap()["about"], json!("Memo-2"));
    assert!(of(&["Note", "Memo"], renamed).complete, "a reader from the rename on has missed nothing");
    let all = of(&["Memo"], 0);
    // Memo-1 is an end of the edge the rename rewrote, so it was written last
    assert_eq!(all.ids, ["Memo-2", "Memo-1"]);
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn an_entity_destroyed_and_made_again_under_its_id_is_read_as_changed_and_not_as_destroyed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![task("Task-1"), task("Task-2")]).unwrap();
    let cursor = db.sequence().unwrap();
    write(&db, vec![json!({ "op": "destroy", "id": "Task-1" })]).unwrap();
    write(&db, vec![task("Task-1")]).unwrap();
    let changed = since(&db, &from(cursor), 100);
    assert_eq!(changed.ids, ["Task-1"]);
    assert_eq!(changed.destroyed, Vec::<String>::new(), "it is there: a reader that dropped it would lose it");

    // Destroyed again, and once more after another life: named once
    write(&db, vec![json!({ "op": "destroy", "id": "Task-1" })]).unwrap();
    write(&db, vec![task("Task-1")]).unwrap();
    write(&db, vec![json!({ "op": "destroy", "id": "Task-1" })]).unwrap();
    let changed = since(&db, &from(cursor), 100);
    assert_eq!((changed.ids, changed.destroyed), (vec![], vec!["Task-1".to_owned()]));
}
