//! The order a value index keeps of its strings (`docs/roadmap/string-order.md`): a key for each string an entry is
//! for, there while the entry holds a place, one for each entity holding a string with no key of its own, and the
//! marker that says which elmdb filed them and whether they are all there.

use serde_json::{Value as Json, json};

use crate::engine::order::{self, Marker, Step};
use crate::engine::unit::{Read, Txn, UnitState};
use crate::engine::{entities, keys};
use crate::format::Prefix;
use crate::format::key::{Part, decode};
use crate::schema::Schema;
use crate::{Database, OpenOptions};

use super::write;

fn schema_json() -> Json {
    json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "types": {
            "Item": {
                "fields": {
                    "tag": { "type": "any", "optional": true },
                    "group": { "type": "string", "optional": true },
                    "n": { "type": "number", "optional": true }
                },
                "indexes": {
                    "byTag": { "kind": "value", "fields": ["tag"] },
                    "byGroupTag": { "kind": "value", "fields": ["group", "tag"] },
                    "byN": { "kind": "value", "fields": ["n"] }
                }
            }
        }
    })
}

fn open_at(dir: &tempfile::TempDir, schema: Json) -> Database {
    let schema = Schema::from_json(&schema).unwrap();
    Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap()
}

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = open_at(&dir, schema_json());
    (dir, db)
}

fn item(id: &str, fields: Json) -> Json {
    json!({ "op": "create", "type": "Item", "id": format!("Item-{id}"), "fields": fields })
}

fn set(id: &str, fields: Json) -> Json {
    json!({ "op": "set", "id": format!("Item-{id}"), "fields": fields })
}

/// An index's order as it is stored: its marker, and what each key under the marker's build holds after the build,
/// the leading parts first, then whether the sort key was cut, then the string or the place
fn stored(db: &Database, index: &str) -> (Option<Marker>, Vec<Vec<Part>>) {
    db.snapshot(|s| {
        let marker = order::marker(s, "Item", index)?;
        let mut held = Vec::new();
        if let Some(marker) = &marker {
            for (key, value) in s.scan(&keys::order_range("Item", index, marker.build, &[]))? {
                assert!(value.is_empty(), "a key of an order holds nothing");
                let mut parts = decode(&key)?.split_off(4);
                // The sort key says nothing a test reads but that it is there
                let at = parts.iter().position(|part| matches!(part, Part::Bytes(_))).expect("a sort key");
                parts.remove(at);
                held.push(parts);
            }
        }
        Ok((marker, held))
    })
    .unwrap()
}

/// The strings an index's order holds, in its order, under no leading part
fn strings(db: &Database, index: &str) -> Vec<String> {
    let (_, held) = stored(db, index);
    held.into_iter()
        .map(|parts| match parts.as_slice() {
            [Part::Bool(false), Part::String(string)] => string.clone(),
            other => panic!("not a whole key of a string: {other:?}"),
        })
        .collect()
}

fn clean(db: &Database) {
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn an_order_has_a_key_for_each_string_an_entry_holds_a_place_under_in_the_collation_s_order() {
    let (_dir, db) = open();
    db.wait_for_index_builds().unwrap();
    let (marker, held) = stored(&db, "byTag");
    let marker = marker.expect("an index built empty has its order");
    assert!(marker.whole && marker.stamp == order::stamp("root").unwrap() && held.is_empty());
    write(
        &db,
        vec![item("1", json!({ "tag": "b" })), item("2", json!({ "tag": "a" })), item("3", json!({ "tag": "b" }))],
    )
    .unwrap();
    assert_eq!(strings(&db, "byTag"), ["a", "b"]);
    clean(&db);
    // A capital after its small letter and before the next letter: the collation's order, not the bytes'
    write(
        &db,
        vec![item("4", json!({ "tag": "B" })), item("5", json!({ "tag": "é" })), item("6", json!({ "tag": 7 }))],
    )
    .unwrap();
    assert_eq!(strings(&db, "byTag"), ["a", "b", "B", "é"]);
    // A string stays while one entity holds it, and goes with the last
    write(&db, vec![set("1", json!({ "tag": "c" }))]).unwrap();
    assert_eq!(strings(&db, "byTag"), ["a", "b", "B", "c", "é"]);
    write(&db, vec![set("3", json!({ "tag": "c" }))]).unwrap();
    assert_eq!(strings(&db, "byTag"), ["a", "B", "c", "é"]);
    write(&db, vec![json!({ "op": "destroy", "id": "Item-2" }), set("4", json!({ "tag": null }))]).unwrap();
    assert_eq!(strings(&db, "byTag"), ["c", "é"]);
    clean(&db);
    // The marker is the one the builder wrote: no write begins a build
    assert_eq!(stored(&db, "byTag").0, Some(marker));
    // An index of numbers has an order of no key
    assert!(stored(&db, "byN").1.is_empty());
}

/// Runs `f` as a unit of this thread's by the store alone, so the database's builder hears nothing of it and the
/// stamp is the one this thread forges
fn unit<T>(db: &Database, f: impl FnOnce(&mut Txn<'_, '_>) -> crate::Result<T>) -> T {
    let schema = db.schema().unwrap();
    let (mut f, mut out) = (Some(f), None);
    db.store()
        .unwrap()
        .write(
            |txn, store| {
                let mut state = UnitState::new(false);
                let f = f.take().expect("a unit runs once");
                out = Some(f(&mut Txn { txn, db: store, state: &mut state, schema: &schema, now: 0.0 })?);
                Ok(())
            },
            false,
        )
        .unwrap();
    out.expect("the unit ran")
}

/// Builds an index's order by units of this thread's, each taking `limit` of its work: how many units it took
fn build(db: &Database, index: &str, limit: usize) -> usize {
    let (mut step, mut units) = (Step::Begin, 0);
    while step != Step::Done {
        step = unit(db, |t| order::build_unit(t, "Item", index, step.clone(), true, limit)).0;
        units += 1;
        assert!(units < 10_000, "a build that doesn't end");
    }
    units
}

/// Every key the store holds under these prefixes for an index of `Item`
fn keys_of(db: &Database, index: &str, prefixes: &[Prefix]) -> usize {
    db.snapshot(|s| {
        let mut held = 0;
        for prefix in prefixes {
            let mut under = prefix.key();
            under.string("Item").string(index);
            let under = under.to_vec();
            held += s.scan(&under)?.len() + usize::from(s.contains(&under)?);
        }
        Ok(held)
    })
    .unwrap()
}

const ORDER: [Prefix; 4] = [Prefix::IndexOrder, Prefix::IndexOrderBuilt, Prefix::IndexLong, Prefix::IndexLongPlaces];

#[test]
fn a_string_s_key_stays_through_an_entry_that_passes_into_blocks_and_back_and_goes_with_its_last_place() {
    let (_dir, db) = open();
    db.wait_for_index_builds().unwrap();
    // More entities than an entry's own key holds, so its places pass into blocks, and out again as they leave
    let many = 600;
    write(&db, (0..many).map(|i| item(&format!("{i:04}"), json!({ "tag": "held" }))).collect()).unwrap();
    assert_eq!(strings(&db, "byTag"), ["held"]);
    clean(&db);
    for i in 0..many - 1 {
        write(&db, vec![set(&format!("{i:04}"), json!({ "tag": "other" }))]).unwrap();
        if i % 97 == 0 || i > many - 4 {
            assert_eq!(strings(&db, "byTag"), ["held", "other"], "{i}");
        }
    }
    clean(&db);
    write(&db, vec![set(&format!("{:04}", many - 1), json!({ "tag": "other" }))]).unwrap();
    assert_eq!(strings(&db, "byTag"), ["other"]);
    clean(&db);
}

#[test]
fn a_string_with_no_key_of_its_own_is_filed_by_the_place_of_each_entity_holding_it() {
    let (_dir, db) = open();
    db.wait_for_index_builds().unwrap();
    // One past what an index has an entry for, and one of fewer bytes whose zero bytes make its part of a key as long
    let long = "a".repeat(300);
    let zeros = format!("{}{}", "b".repeat(200), "\0".repeat(60));
    assert!(!order::placed(&long) && !order::placed(&zeros) && order::placed("b"));
    write(
        &db,
        vec![
            item("1", json!({ "tag": long })),
            item("2", json!({ "tag": zeros })),
            item("3", json!({ "tag": zeros })),
            item("4", json!({ "tag": "b" })),
        ],
    )
    .unwrap();
    let places = |db: &Database| -> Vec<Vec<Part>> { stored(db, "byTag").1 };
    // The long one's sort key is cut, the others' whole; each entity holding a string with no key has one by its
    // place, in the order of their strings and then their places, and the short string has its own
    assert_eq!(
        places(&db),
        [
            vec![Part::Bool(true), Part::Number(1.0)],
            vec![Part::Bool(false), Part::String("b".into())],
            vec![Part::Bool(false), Part::Number(2.0)],
            vec![Part::Bool(false), Part::Number(3.0)],
        ]
    );
    // The long one is among the index's long strings, and the one with an entry is not
    assert_eq!(keys_of(&db, "byTag", &[Prefix::IndexLong]), 1);
    clean(&db);
    // A rebuild files the same, under a build of its own
    let before = stored(&db, "byTag");
    db.rebuild(Some(("Item", "byTag"))).unwrap();
    let after = stored(&db, "byTag");
    assert_eq!(after.1, before.1);
    assert!(after.0.as_ref().unwrap().whole && after.0.unwrap().build > before.0.unwrap().build);
    clean(&db);
    // An entity's key goes when its string does, and comes for the one it takes
    write(
        &db,
        vec![
            set("1", json!({ "tag": "b" })),
            set("2", json!({ "tag": long })),
            json!({ "op": "destroy", "id": "Item-3" }),
        ],
    )
    .unwrap();
    assert_eq!(
        places(&db),
        [vec![Part::Bool(true), Part::Number(2.0)], vec![Part::Bool(false), Part::String("b".into())]]
    );
    clean(&db);
}

#[test]
fn a_compound_index_keeps_an_order_under_each_tuple_of_its_leading_values() {
    let (_dir, db) = open();
    db.wait_for_index_builds().unwrap();
    write(
        &db,
        vec![
            item("1", json!({ "group": "x", "tag": "b" })),
            item("2", json!({ "group": "x", "tag": "a" })),
            item("3", json!({ "group": "y", "tag": "b" })),
            item("4", json!({ "tag": "c" })),
        ],
    )
    .unwrap();
    let under = |db: &Database| -> Vec<(String, String)> {
        let (_, held) = stored(db, "byGroupTag");
        held.into_iter()
            .map(|parts| match parts.as_slice() {
                [Part::String(group), Part::Bool(false), Part::String(tag)] => (group.clone(), tag.clone()),
                other => panic!("{other:?}"),
            })
            .collect()
    };
    let pairs = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs.iter().map(|(group, tag)| ((*group).to_owned(), (*tag).to_owned())).collect()
    };
    // An entity with no value before the last is under no tuple, and files no key
    assert_eq!(under(&db), pairs(&[("x", "a"), ("x", "b"), ("y", "b")]));
    // A change to a field before the last moves the string's key to the other tuple
    write(&db, vec![set("1", json!({ "group": "y" }))]).unwrap();
    assert_eq!(under(&db), pairs(&[("x", "a"), ("y", "b")]));
    write(&db, vec![set("3", json!({ "group": "x" }))]).unwrap();
    assert_eq!(under(&db), pairs(&[("x", "a"), ("x", "b"), ("y", "b")]));
    clean(&db);
    db.rebuild(None).unwrap();
    assert_eq!(under(&db), pairs(&[("x", "a"), ("x", "b"), ("y", "b")]));
    clean(&db);
}

#[test]
fn check_reports_a_key_of_an_order_removed_one_added_and_one_moved() {
    let (_dir, db) = open();
    db.wait_for_index_builds().unwrap();
    write(&db, vec![item("1", json!({ "tag": "a" })), item("2", json!({ "tag": "b" }))]).unwrap();
    clean(&db);
    let build = stored(&db, "byTag").0.unwrap().build;
    let root = order::running("root").unwrap();
    let key = |string: &str| {
        order::string_key(&root.collator, "Item", "byTag", build, &[Part::String(string.into())]).unwrap()
    };
    let tamper = |put: Option<Vec<u8>>, delete: Option<Vec<u8>>| {
        db.store()
            .unwrap()
            .write(
                |txn, store| {
                    if let Some(key) = &put {
                        store.put(txn, key, &[])?;
                    }
                    if let Some(key) = &delete {
                        store.delete(txn, key)?;
                    }
                    Ok(())
                },
                false,
            )
            .unwrap();
    };
    let kinds = |db: &Database| -> Vec<String> {
        let mut kinds: Vec<String> = db.check().unwrap().differences.into_iter().map(|d| d.kind).collect();
        kinds.sort();
        kinds
    };
    // One removed
    tamper(None, Some(key("a")));
    assert_eq!(kinds(&db), ["missing"]);
    tamper(Some(key("a")), None);
    clean(&db);
    // One added, for a string no entity holds
    tamper(Some(key("c")), None);
    assert_eq!(kinds(&db), ["extra"]);
    tamper(None, Some(key("c")));
    // One moved: the string's key where another collator would have put it
    let wrong = order::SortKey { bytes: vec![0x7f], cut: false };
    let moved = keys::order_string("Item", "byTag", build, &[], &wrong, "a");
    tamper(Some(moved), Some(key("a")));
    assert_eq!(kinds(&db), ["extra", "missing"]);
    // A key under a build that isn't the marker's is no order's, and is not compared
    db.rebuild(Some(("Item", "byTag"))).unwrap();
    clean(&db);
    tamper(Some(key("zz")), None);
    clean(&db);
}

#[test]
fn an_index_declared_over_entities_has_no_order_until_it_is_built_and_one_that_can_t_be_built_has_none() {
    let dir = tempfile::tempdir().unwrap();
    let mut schema = schema_json();
    let indexes = schema["types"]["Item"]["indexes"].take();
    schema["types"]["Item"]["indexes"] = json!({});
    let db = open_at(&dir, schema.clone());
    write(
        &db,
        vec![
            item("1", json!({ "tag": "b" })),
            item("2", json!({ "tag": "a" })),
            item("3", json!({ "tag": "a", "n": 1 })),
        ],
    )
    .unwrap();
    // Declared over entities, its builder fills it, and then its order's does; a write beside them files into both
    schema["types"]["Item"]["indexes"] = indexes;
    db.set_schema(Schema::from_json(&schema).unwrap()).unwrap();
    write(&db, vec![item("4", json!({ "tag": "c" }))]).unwrap();
    db.wait_for_index_builds().unwrap();
    assert_eq!(strings(&db, "byTag"), ["a", "b", "c"]);
    assert!(stored(&db, "byTag").0.unwrap().whole);
    clean(&db);
    // A unique index two entities hold one value of can't be built: it has neither a marker nor a key, whatever is
    // written to its type, and a rebuild of everything leaves it so
    schema["types"]["Item"]["indexes"]["oneTag"] = json!({ "kind": "value", "fields": ["tag"], "unique": true });
    db.set_schema(Schema::from_json(&schema).unwrap()).unwrap();
    assert!(db.wait_for_index_builds().is_err());
    assert_eq!(keys_of(&db, "oneTag", &ORDER), 0);
    write(&db, vec![item("5", json!({ "tag": "d" }))]).unwrap();
    assert_eq!(keys_of(&db, "oneTag", &ORDER), 0);
    db.rebuild(None).unwrap();
    assert_eq!(keys_of(&db, "oneTag", &ORDER), 0);
    assert_eq!(strings(&db, "byTag"), ["a", "b", "c", "d"]);
    // The rows two entities hold one value in are the unique rule's to report, and no derived key differs
    let report = db.check().unwrap();
    assert!(report.differences.is_empty() && report.violations.len() == 1, "{report:?}");
}

#[test]
fn a_dropped_index_and_a_reset_leave_no_key_of_an_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_at(&dir, schema_json());
    db.wait_for_index_builds().unwrap();
    write(&db, vec![item("1", json!({ "tag": "a" })), item("2", json!({ "tag": "a".repeat(300) }))]).unwrap();
    assert!(keys_of(&db, "byTag", &ORDER) >= 4);
    let mut schema = schema_json();
    schema["types"]["Item"]["indexes"].as_object_mut().unwrap().remove("byTag");
    db.set_schema(Schema::from_json(&schema).unwrap()).unwrap();
    assert_eq!(keys_of(&db, "byTag", &ORDER), 0);
    // Declared again it is built from the rows, its order with it
    db.set_schema(Schema::from_json(&schema_json()).unwrap()).unwrap();
    db.wait_for_index_builds().unwrap();
    assert_eq!(strings_and_places(&db, "byTag"), 2);
    clean(&db);
    db.reset().unwrap();
    db.wait_for_index_builds().unwrap();
    // Nothing but the marker of an order of nothing, which entities made after it are filed under
    assert_eq!(keys_of(&db, "byTag", &ORDER), 1);
    write(&db, vec![item("1", json!({ "tag": "z" }))]).unwrap();
    assert_eq!(strings(&db, "byTag"), ["z"]);
    clean(&db);
}

/// How many keys an index's order holds under its marker's build
fn strings_and_places(db: &Database, index: &str) -> usize {
    stored(db, index).1.len()
}

#[test]
fn a_write_of_another_stamp_ends_an_order_s_build_files_nothing_and_leaves_it_owed() {
    let (_dir, db) = open();
    db.wait_for_index_builds().unwrap();
    write(&db, vec![item("1", json!({ "tag": "a" })), item("2", json!({ "tag": "b" }))]).unwrap();
    let ours = stored(&db, "byTag").0.unwrap();
    let readable = |db: &Database| db.snapshot(|s| order::readable(s, "Item", "byTag")).unwrap();
    assert_eq!(readable(&db), Some(ours.build));
    // Another elmdb's write of a string another entity holds, which this layout files nothing for: it ends the
    // build all the same, since what its own layout would have filed is not this one's to know
    order::forge(1);
    let owed = unit(&db, |t| {
        entities::put_field(t, "Item-1", "tag", &"b".into())?;
        Ok(t.state.orders_owed.clone())
    });
    assert_eq!(owed.into_iter().collect::<Vec<_>>(), [("Item".to_owned(), "byTag".to_owned())]);
    assert_eq!(db.snapshot(|s| order::marker(s, "Item", "byTag")).unwrap(), None);
    assert_eq!(readable(&db), None);
    // With no marker it files nothing, a new string's key neither
    unit(&db, |t| entities::put_field(t, "Item-2", "tag", &"c".into()));
    assert_eq!(db.snapshot(|s| order::marker(s, "Item", "byTag")).unwrap(), None);
    // The other's builder builds the order under its own stamp, which this elmdb doesn't read, and whose keys a
    // check of this elmdb's doesn't compare
    build(&db, "byTag", 1000);
    let theirs = stored(&db, "byTag").0.unwrap();
    assert!(theirs.whole && theirs.stamp != ours.stamp && theirs.build > ours.build);
    assert_eq!(strings(&db, "byTag"), ["b", "c"]);
    order::forge(0);
    assert_eq!(readable(&db), None);
    clean(&db);
    // This elmdb's write ends that build in its turn, and its builder builds its own: the keys of the other's
    // build are deleted, and the order is this one's to read again
    unit(&db, |t| entities::put_field(t, "Item-1", "tag", &"d".into()));
    assert_eq!(db.snapshot(|s| order::marker(s, "Item", "byTag")).unwrap(), None);
    let units = build(&db, "byTag", 1);
    assert!(units > 3, "{units}");
    let again = stored(&db, "byTag").0.unwrap();
    assert!(again.whole && again.stamp == ours.stamp && again.build > theirs.build);
    assert_eq!(strings(&db, "byTag"), ["c", "d"]);
    assert_eq!(db.snapshot(|s| Ok(s.scan(&keys::order_of("Item", "byTag"))?.len())).unwrap(), 2);
    clean(&db);
}

#[test]
fn a_build_goes_on_with_one_it_finds_unfinished_beside_the_writes_that_keep_it() {
    let (_dir, db) = open();
    db.wait_for_index_builds().unwrap();
    write(&db, (0..40).map(|i| item(&format!("{i:02}"), json!({ "tag": format!("s{i:02}") }))).collect()).unwrap();
    // The order's marker deleted, as another's write leaves it: a build begun and stopped part way
    unit(&db, |t| {
        t.del(&keys::order_marker("Item", "byTag"))?;
        t.clear_prefix(&keys::order_of("Item", "byTag"))
    });
    let mut step = Step::Begin;
    for _ in 0..3 {
        step = unit(&db, |t| order::build_unit(t, "Item", "byTag", step.clone(), true, 5)).0;
    }
    assert!(matches!(step, Step::Entries { .. }), "{step:?}");
    let begun = stored(&db, "byTag").0.unwrap();
    assert!(!begun.whole);
    assert_eq!(db.snapshot(|s| order::readable(s, "Item", "byTag")).unwrap(), None);
    // Writes of this stamp file under it, behind where the build is and ahead of it
    write(
        &db,
        vec![
            item("90", json!({ "tag": "s00a" })),
            item("91", json!({ "tag": "s39a" })),
            set("01", json!({ "tag": "s00" })),
        ],
    )
    .unwrap();
    // A builder that finds it goes on from the index's first key, puts what isn't there, and marks it whole
    build(&db, "byTag", 7);
    let whole = stored(&db, "byTag").0.unwrap();
    assert!(whole.whole && whole.build == begun.build);
    assert_eq!(strings(&db, "byTag").len(), 41);
    clean(&db);
}
