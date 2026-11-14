//! The order a value index keeps of its strings (`docs/roadmap/string-order.md`): a key for each string an entry is
//! for, there while the entry holds a place, one for each entity holding a string with no key of its own, and the
//! marker that says which elmdb filed them and whether they are all there.

use serde_json::{Value as Json, json};

use crate::engine::order::{self, Marker, Step};
use crate::engine::plan::{ReadOp, Reads};
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
    // A builder's units, each taking one key or one place of the work, file the same: an entry of many places and
    // the long strings' one key are gone on with from the place a unit left off at
    unit(&db, |t| order::clear(t, "Item", "byTag"));
    assert!(build(&db, "byTag", 1) > 3);
    assert_eq!(stored(&db, "byTag").1, before.1);
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
fn a_build_left_before_it_deleted_another_build_s_keys_is_gone_on_with_at_the_next_open() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_at(&dir, schema_json());
    db.wait_for_index_builds().unwrap();
    write(&db, (0..30).map(|i| item(&format!("{i:02}"), json!({ "tag": format!("s{i:02}") }))).collect()).unwrap();
    // A build whole under a new number, the keys of the one before it still there: a builder stopped before its
    // last units
    let old = stored(&db, "byTag").0.unwrap();
    unit(&db, |t| t.del(&keys::order_marker("Item", "byTag")));
    let mut step = Step::Begin;
    while !matches!(step, Step::Sweep { .. }) {
        step = unit(&db, |t| order::build_unit(t, "Item", "byTag", step.clone(), true, 7)).0;
    }
    let all = |db: &Database| db.snapshot(|s| Ok(s.scan(&keys::order_of("Item", "byTag"))?.len())).unwrap();
    let new = stored(&db, "byTag").0.unwrap();
    assert!(new.whole && new.build > old.build);
    assert!(all(&db) > 30 && db.snapshot(|s| order::stale(s, "Item", "byTag", new.build)).unwrap());
    // A read trusts the new build, and a check compares no key of the old
    assert_eq!(read(&db, by_tag(1), None), (ids(&["00"]), 1));
    clean(&db);
    // Opened again, the builder is told of it, and deletes them
    drop(db);
    let db = open_at(&dir, schema_json());
    db.wait_for_index_builds().unwrap();
    assert_eq!(all(&db), 30);
    assert_eq!(stored(&db, "byTag").0, Some(new));
    // And an order that needs nothing costs no unit: the sequence stays where the last commit left it
    let sequence = db.sequence().unwrap();
    drop(db);
    let db = open_at(&dir, schema_json());
    db.wait_for_index_builds().unwrap();
    assert_eq!(db.sequence().unwrap(), sequence);
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
    // Which it says, of that order alone
    assert_eq!(
        db.check().unwrap().notes,
        ["The order of Item.byTag's strings was filed under another stamp, and was not compared"]
    );
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
    let ordered = |db: &Database| {
        let all = db.index_status().unwrap();
        all.into_iter().find(|status| status.index == "byTag").unwrap().ordered
    };
    assert_eq!(ordered(&db), Some(false));
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
    assert_eq!(db.check().unwrap().notes, ["The order of Item.byTag's strings is being built, and was not compared"]);
    // A builder that finds it goes on from the index's first key, puts what isn't there, and marks it whole
    build(&db, "byTag", 7);
    let whole = stored(&db, "byTag").0.unwrap();
    assert!(whole.whole && whole.build == begun.build);
    assert!(db.check().unwrap().notes.is_empty());
    assert_eq!(ordered(&db), Some(true));
    let own = db.index_status().unwrap().into_iter().find(|status| status.index == "shortCode").unwrap();
    assert_eq!(own.ordered, None);
    assert_eq!(strings(&db, "byTag").len(), 41);
    clean(&db);
}

/// The ids a plan of Items answers after these steps, and the entities it visited
fn read(db: &Database, steps: Json, track: Option<u32>) -> (Vec<String>, u64) {
    let plan =
        json!({ "start": { "from": "types", "types": ["Item"] }, "steps": steps, "terminal": { "answer": "ids" } });
    let op = serde_json::from_value::<ReadOp>(json!({ "read": "query", "visits": true, "plan": plan })).unwrap();
    let bytes = db.read(&Reads { reads: vec![op], track }).unwrap();
    let answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    let ids = answers[0]["answer"]["ids"].as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned());
    (ids.collect(), answers[0]["visited"].as_u64().unwrap())
}

fn by_tag(take: usize) -> Json {
    json!([{ "step": "orderBy", "field": "tag" }, { "step": "limit", "n": take }])
}

fn ids(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| format!("Item-{name}")).collect()
}

#[test]
fn an_order_is_read_from_keys_this_elmdb_filed_whole_and_from_the_rows_under_any_other_marker() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_at(&dir, schema_json());
    db.wait_for_index_builds().unwrap();
    // Swedish puts `å` after `z`, where the root collation has it beside `a`
    write(
        &db,
        vec![item("z", json!({ "tag": "z" })), item("ao", json!({ "tag": "å" })), item("a", json!({ "tag": "a" }))],
    )
    .unwrap();
    assert_eq!(read(&db, by_tag(2), None), (ids(&["a", "ao"]), 2));
    let root = stored(&db, "byTag").0.unwrap();
    // The collation changed: the keys are the root's, under the root's stamp, and no read of this schema's trusts
    // them. Until the builder has filed them again the rows are read, in the new collation's order
    let mut swedish = schema_json();
    swedish["collation"] = json!("sv");
    let changes = db.set_schema(Schema::from_json(&swedish).unwrap()).unwrap();
    assert!(changes.iter().any(|change| change.path == "collation" && change.effect == crate::schema::Effect::Builds));
    db.wait_for_index_builds().unwrap();
    assert_eq!(read(&db, by_tag(2), None), (ids(&["a", "z"]), 2));
    let built = stored(&db, "byTag").0.unwrap();
    assert!(built.whole && built.stamp != root.stamp && built.build > root.build);
    clean(&db);
    // The root's keys under a marker forged to this stamp are read, and give the root's order: what the stamp is for
    let forged = |keys_of: &str, stamp_of: &str| {
        let mut filed = schema_json();
        filed["collation"] = json!(keys_of);
        let filed = Schema::from_json(&filed).unwrap();
        let stamp = order::stamp(stamp_of).unwrap();
        db.store()
            .unwrap()
            .write(
                |txn, store| {
                    let mut state = UnitState::new(false);
                    let mut t = Txn { txn, db: store, state: &mut state, schema: &filed, now: 0.0 };
                    order::clear(&mut t, "Item", "byTag")?;
                    let (step, _) = order::build_unit(&mut t, "Item", "byTag", Step::Begin, true, 1000)?;
                    assert_eq!(step, Step::Done);
                    let marker = order::marker(&t, "Item", "byTag")?.expect("built");
                    t.put(&keys::order_marker("Item", "byTag"), &Marker { stamp, ..marker }.encode())
                },
                false,
            )
            .unwrap();
    };
    forged("root", "sv");
    assert_eq!(read(&db, by_tag(2), None), (ids(&["a", "ao"]), 2), "the root's order, read as if it were this one's");
    // Under their true stamp they are no read's: every entity's row is read, and the order is right
    forged("root", "root");
    assert_eq!(read(&db, by_tag(2), None), (ids(&["a", "z"]), 3));
    // A database opened to read only reads what is there, and never builds
    drop(db);
    let schema = Schema::from_json(&swedish).unwrap();
    let options = OpenOptions { schema: Some(schema), syncs: false, read_only: true, ..Default::default() };
    let reader = Database::open(dir.path(), options).unwrap();
    assert_eq!(read(&reader, by_tag(2), None), (ids(&["a", "z"]), 3));
    // An order of numbers is no collator's: its index is read whatever the marker of the strings' order beside it
    assert_eq!(read(&reader, json!([{ "step": "orderBy", "field": "n" }, { "step": "limit", "n": 1 }]), None).1, 1);
    drop(reader);
    // And one opened to write builds it, and then reads it
    let db = open_at(&dir, swedish);
    db.wait_for_index_builds().unwrap();
    assert_eq!(read(&db, by_tag(2), None), (ids(&["a", "z"]), 2));
    clean(&db);
}

#[test]
fn a_read_of_an_order_is_one_a_commit_writes_into_where_it_changes_what_the_read_read() {
    let (_dir, db) = open();
    db.wait_for_index_builds().unwrap();
    write(
        &db,
        vec![item("b", json!({ "tag": "b" })), item("c", json!({ "tag": "c" })), item("d", json!({ "tag": "d" }))],
    )
    .unwrap();
    let touched = |ops: Vec<Json>| db.write(&super::batch(ops), false).unwrap().1.touched;
    let track = || assert_eq!(read(&db, by_tag(2), Some(1)), (ids(&["b", "c"]), 2));
    track();
    // A string past what the read went as far as is nothing it read
    assert!(touched(vec![item("z", json!({ "tag": "z" }))]).is_empty());
    // One before the first it read is, and so is another entity taking a string it read
    assert_eq!(touched(vec![item("a", json!({ "tag": "a" }))]), [1]);
    db.untrack(1);
    assert_eq!(read(&db, by_tag(2), Some(1)), (ids(&["a", "b"]), 2));
    assert_eq!(touched(vec![item("b2", json!({ "tag": "b" }))]), [1]);
    // And so is another elmdb's write that ends the build the read was of, whatever string it files
    db.untrack(1);
    assert_eq!(read(&db, by_tag(2), Some(1)), (ids(&["a", "b"]), 2));
    order::forge(1);
    assert_eq!(touched(vec![item("y", json!({ "tag": "y" }))]), [1]);
    order::forge(0);
}

/// One unit of the model below: a write by one of two elmdbs, a unit of one's builder, a `rebuild()` of the index,
/// or the index dropped
#[derive(Debug, Clone, Copy, PartialEq)]
enum Act {
    /// The elmdb, the entity, and the string it is given, none taking its string away
    Write(usize, u8, Option<&'static str>),
    Build(usize),
    Rebuild,
    Drop,
}

const ACTS: [Act; 12] = [
    Act::Write(0, 1, Some("å")),
    Act::Write(0, 1, None),
    Act::Write(0, 2, Some("å")),
    Act::Write(0, 2, Some("z")),
    Act::Write(1, 1, Some("å")),
    Act::Write(1, 1, None),
    Act::Write(1, 2, Some("å")),
    Act::Write(1, 2, Some("z")),
    Act::Build(0),
    Act::Build(1),
    Act::Rebuild,
    Act::Drop,
];

/// Runs `acts` in turn as units of one transaction, which is then rolled back, holding after each that no elmdb
/// reads an order lacking a key or holding one too many: under a marker of its own stamp that is whole, the keys
/// under the marker's build are those of the strings the index has entries for, each where its collator puts it;
/// and that an index that isn't built has neither marker nor key. The two elmdbs differ in their collation, so in
/// their stamps and in where they file a string. Each unit commits as the next sequence, as a unit does
fn holds_through(db: &Database, schemas: &[Schema; 2], acts: &[Act]) {
    use crate::engine::derive;
    let sequence = crate::store::meta_key(crate::format::meta::SEQUENCE);
    let rolled_back = db.store().unwrap().write(
        |txn, store| {
            let mut steps = [Step::Begin, Step::Begin];
            for (at, act) in acts.iter().enumerate() {
                let mut state = UnitState::new(false);
                let schema = match act {
                    Act::Write(elmdb, ..) | Act::Build(elmdb) => &schemas[*elmdb],
                    Act::Rebuild | Act::Drop => &schemas[0],
                };
                let mut t = Txn { txn: &mut *txn, db: store, state: &mut state, schema, now: 0.0 };
                match act {
                    Act::Write(_, entity, string) => {
                        let value = string.map_or(rmpv::Value::Nil, rmpv::Value::from);
                        entities::put_field(&mut t, &format!("Item-{entity}"), "tag", &value)?;
                    }
                    Act::Build(elmdb) => {
                        let step = std::mem::replace(&mut steps[*elmdb], Step::Begin);
                        steps[*elmdb] = match order::build_unit(&mut t, "Item", "byTag", step, true, 1)?.0 {
                            Step::Done => Step::Begin,
                            step => step,
                        };
                    }
                    Act::Rebuild => derive::rebuild(&mut t, Some(("Item", "byTag")))?,
                    Act::Drop => {
                        derive::clear_value_index(&mut t, "Item", "byTag")?;
                        order::clear(&mut t, "Item", "byTag")?;
                        t.del(&keys::index_built("Item", "byTag"))?;
                    }
                }
                let committed = crate::engine::mirror::stored_sequence(&t)? + 1;
                t.put(&sequence, &committed.to_be_bytes())?;
                let built = derive::is_built(&t, "Item", "byTag")?;
                for (elmdb, schema) in schemas.iter().enumerate() {
                    let mut state = UnitState::new(false);
                    let t = Txn { txn: &mut *txn, db: store, state: &mut state, schema, now: 0.0 };
                    let said = || format!("elmdb {elmdb} after {:?}", &acts[..=at]);
                    if !built {
                        assert_eq!(order::marker(&t, "Item", "byTag")?, None, "{}", said());
                        assert!(t.scan(&keys::order_of("Item", "byTag"))?.is_empty(), "{}", said());
                        continue;
                    }
                    let Some(build) = order::readable(&t, "Item", "byTag")? else { continue };
                    let running = order::running(&schema.collation)?;
                    let mut wanted = Vec::new();
                    for (own, _) in t.scan(&keys::index_range("Item", "byTag", &[]))? {
                        let parts = decode(&own)?;
                        wanted.extend(order::string_key(&running.collator, "Item", "byTag", build, &parts[3..]));
                    }
                    wanted.sort();
                    let filed: Vec<Vec<u8>> = t
                        .scan(&keys::order_range("Item", "byTag", build, &[]))?
                        .into_iter()
                        .map(|(key, _)| key)
                        .collect();
                    assert_eq!(filed, wanted, "{}", said());
                }
            }
            Err::<(), _>(crate::Error::new(crate::ErrorCode::Io, "the model's units are rolled back"))
        },
        false,
    );
    assert_eq!(rolled_back.unwrap_err().message, "the model's units are rolled back");
}

#[test]
fn no_read_trusts_a_build_lacking_a_key_whatever_order_the_units_of_two_elmdbs_come_in() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_at(&dir, schema_json());
    db.wait_for_index_builds().unwrap();
    write(&db, vec![item("1", json!({})), item("2", json!({ "tag": "z" })), item("3", json!({ "tag": "a" }))]).unwrap();
    let mut swedish = schema_json();
    swedish["collation"] = json!("sv");
    let schemas = [Schema::from_json(&schema_json()).unwrap(), Schema::from_json(&swedish).unwrap()];
    // The two file a string in different places, or the model would hold whichever wrote
    let key = |collation: &str| {
        let running = order::running(collation).unwrap();
        order::string_key(&running.collator, "Item", "byTag", 1, &[Part::String("å".into())])
    };
    assert_ne!(key("root"), key("sv"));
    // Every order of four units
    let n = ACTS.len();
    for at in 0..n.pow(4) {
        let acts: Vec<Act> = (0..4).map(|place| ACTS[(at / n.pow(place)) % n]).collect();
        holds_through(&db, &schemas, &acts);
    }
    // And longer ones, each unit drawn by a generator a seed fixes: a builder's several units among the others'
    let mut drawn = 0x9e37_79b9_7f4a_7c15_u64;
    for _ in 0..3000 {
        let acts: Vec<Act> = (0..12)
            .map(|_| {
                drawn = drawn.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                ACTS[(drawn >> 33) as usize % n]
            })
            .collect();
        holds_through(&db, &schemas, &acts);
    }
    // Nothing of them was kept
    assert_eq!(strings(&db, "byTag"), ["a", "z"]);
    clean(&db);
}
