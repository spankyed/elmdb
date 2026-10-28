//! Field values stored compressed from `COMPRESS_FROM` bytes (`docs/architecture/format.md`, Values): what is stored, and
//! that every reader — a row, a field, a filter, a value index, a text index, `check()`, `rebuild()` and a dump — sees
//! the value it was given.

use serde_json::{Value as Json, json};

use crate::engine::plan::{ReadOp, Reads};
use crate::engine::{keys, values};
use crate::schema::Schema;
use crate::{Database, OpenOptions};

fn schema() -> Schema {
    Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "types": { "Message": {
            "fields": {
                "text": { "type": "string", "optional": true },
                "tag": { "type": "string", "optional": true },
                "blob": { "type": "any", "optional": true }
            },
            "indexes": {
                "tag": { "kind": "value", "fields": ["tag"] },
                "text": { "kind": "text", "fields": { "text": {} } }
            }
        } }
    }))
    .unwrap()
}

/// Long text of a chat's kind, which compresses well
fn prose() -> String {
    "the database keeps each message whole, and a long message is mostly words it has seen before. ".repeat(40)
}

/// Text that doesn't compress
fn noise(len: usize) -> String {
    let mut state = 0x2545_f491_u32;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            char::from(b'!' + (state % 90) as u8)
        })
        .collect()
}

fn open(dir: &tempfile::TempDir) -> Database {
    Database::open(dir.path(), OpenOptions { schema: Some(schema()), syncs: false, ..Default::default() }).unwrap()
}

fn stored(db: &Database, id: &str, field: &str) -> Vec<u8> {
    db.snapshot(|r| Ok(crate::engine::unit::Read::get(r, &keys::value(id, field))?.unwrap())).unwrap()
}

fn read(db: &Database, op: Json) -> Json {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    let bytes = db.read(&reads).unwrap();
    let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    answers[0].take()
}

#[test]
fn a_large_value_is_stored_compressed_a_small_or_dense_one_as_its_msgpack() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    super::write(
        &db,
        vec![
            json!({ "op": "create", "type": "Message", "id": "Message-long", "fields": { "text": prose() } }),
            json!({ "op": "create", "type": "Message", "id": "Message-short", "fields": { "text": "a short one" } }),
        ],
    )
    .unwrap();
    let long = stored(&db, "Message-long", "text");
    let plain = values::encode(&prose().into());
    assert!(plain.len() >= values::COMPRESS_FROM);
    assert_eq!(long[0], values::COMPRESSED);
    assert!(long.len() * 4 < plain.len(), "{} bytes stored for {}", long.len(), plain.len());
    assert_eq!(
        stored(&db, "Message-short", "text"),
        values::encode(&"a short one".into()),
        "below the threshold, as given"
    );
    // Bytes that don't compress are kept as given, compressing them costing more than it saves
    let mut state = 0x2545_f491_u32;
    let random: Vec<u8> = (0..4_096)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let dense = rmpv::Value::Binary(random);
    assert_eq!(values::stored(&dense), values::encode(&dense));
}

#[test]
fn every_reader_sees_the_value_written_and_check_rebuild_and_a_dump_agree() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let text = prose();
    let tag = noise(200) + &"z".repeat(400);
    super::write(&db, vec![
        json!({ "op": "create", "type": "Message", "id": "Message-a", "fields": { "text": text, "tag": "short" } }),
        json!({ "op": "create", "type": "Message", "id": "Message-b", "fields": { "text": "another", "tag": tag } }),
    ])
    .unwrap();
    assert_eq!(stored(&db, "Message-b", "tag")[0], values::COMPRESSED);
    // A row, a field, a filter, a value index (a string past what one holds is walked), a text index
    let row = read(&db, json!({ "read": "row", "id": "Message-a" }));
    assert_eq!(row["row"]["text"], json!(text));
    let field = read(&db, json!({ "read": "field", "id": "Message-b", "field": "tag" }));
    assert_eq!(field["value"], json!(tag));
    let plan = |steps: Json| {
        json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Message"] }, "steps": steps, "terminal": { "answer": "ids" } } })
    };
    assert_eq!(
        read(&db, plan(json!([{ "step": "where", "field": "tag", "value": tag }])))["ids"],
        json!(["Message-b"])
    );
    assert_eq!(
        read(&db, plan(json!([{ "step": "search", "query": "seen before", "index": "text", "options": {} }])))["ids"],
        json!(["Message-a"])
    );
    // Derived data from compressed values is what the writes keep, and what a rebuild makes
    assert!(db.check().unwrap().is_clean());
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean());
    // A dump holds the value, not its stored form
    let mut loads = Vec::new();
    db.primary_snapshot(|_, r| {
        crate::engine::maintenance::primary_loads_of(r, |load| {
            loads.push(load);
            Ok(())
        })
    })
    .unwrap();
    let held = loads.iter().find_map(|load| match load {
        crate::engine::maintenance::Load::Entity { id, fields, .. } if id == "Message-a" => fields.get("text").cloned(),
        _ => None,
    });
    assert_eq!(held, Some(text.into()));
}
