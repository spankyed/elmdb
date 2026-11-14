//! The order a value index keeps of its strings (`docs/roadmap/string-order.md`): a key for each string an entry is
//! for, there while the entry holds a place, one for each entity holding a string with no key of its own, and the
//! marker that says which elmdb filed them and whether they are all there.

use serde_json::{Value as Json, json};

use crate::engine::keys;
use crate::engine::order::{self, Marker};
use crate::engine::unit::Read;
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
