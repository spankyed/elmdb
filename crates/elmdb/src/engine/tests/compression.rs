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
    assert_eq!(values::stored(&dense).unwrap(), values::encode(&dense));
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

/// A stored value decompresses to its msgpack whether its frame records its size, records none (a streaming encoder's,
/// which a reader takes as a stream) or records more than one call decompresses; a frame cut short is `CORRUPTED`
#[test]
fn a_frame_with_or_without_its_size_or_past_one_call_reads_back_and_one_cut_short_is_corrupted() {
    use std::io::Write;
    let value = rmpv::Value::from(prose());
    let plain = values::encode(&value);
    let sized = values::stored(&value).unwrap();
    assert_eq!(sized[0], values::COMPRESSED);
    assert_eq!(values::unstored(&sized).unwrap().as_ref(), plain.as_slice());

    let mut encoder = zstd::stream::Encoder::new(vec![values::COMPRESSED], 3).unwrap();
    encoder.include_contentsize(false).unwrap();
    encoder.write_all(&plain).unwrap();
    let streamed = encoder.finish().unwrap();
    assert_eq!(zstd::zstd_safe::get_frame_content_size(&streamed[1..]).unwrap(), None);
    assert_eq!(values::unstored(&streamed).unwrap().as_ref(), plain.as_slice());

    let large = rmpv::Value::from("a".repeat(17 << 20));
    let stored = values::stored(&large).unwrap();
    assert!(zstd::zstd_safe::get_frame_content_size(&stored[1..]).unwrap().unwrap() > 16 << 20);
    assert_eq!(values::unstored(&stored).unwrap().as_ref(), values::encode(&large).as_slice());

    for frame in [&sized, &streamed] {
        let cut = &frame[..frame.len() - 4];
        assert_eq!(values::unstored(cut).unwrap_err().code, crate::ErrorCode::Corrupted);
    }
}

/// A damaged frame that claims far more than it holds is `CORRUPTED`, read as a stream rather than into a buffer of the
/// size it claims, which here no allocation could be
#[test]
fn a_frame_claiming_more_than_any_buffer_holds_is_corrupted_without_allocating_its_claim() {
    // A zstd frame: its magic, a header of an eight-byte content size in one segment, a claim of 2^63 bytes, then one
    // last raw block of four
    let mut frame = vec![values::COMPRESSED, 0x28, 0xb5, 0x2f, 0xfd, 0xe0];
    frame.extend_from_slice(&(1u64 << 63).to_le_bytes());
    frame.extend_from_slice(&[(4 << 3) | 1, 0, 0]);
    frame.extend_from_slice(b"data");
    assert_eq!(zstd::zstd_safe::get_frame_content_size(&frame[1..]).unwrap(), Some(1 << 63));
    assert_eq!(values::unstored(&frame).unwrap_err().code, crate::ErrorCode::Corrupted);
}

/// A write of a value past the most a field holds is refused, and leaves nothing of its batch
#[test]
fn a_write_of_a_value_past_the_most_a_field_holds_is_refused_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    // Five bytes longer as msgpack: one byte past the most
    let past = "x".repeat(values::MAX_VALUE_BYTES - 4);
    let refused = super::write(
        &db,
        vec![
            json!({ "op": "create", "type": "Message", "id": "Message-ok", "fields": { "text": "kept out" } }),
            json!({ "op": "create", "type": "Message", "id": "Message-big", "fields": { "blob": past } }),
        ],
    );
    assert_eq!(refused.unwrap_err().code, crate::ErrorCode::ValueTooLarge);
    let (ok_entity, big_entity, big_value) = db
        .snapshot(|r| {
            Ok((
                crate::engine::unit::Read::get(r, &keys::entity("Message-ok"))?,
                crate::engine::unit::Read::get(r, &keys::entity("Message-big"))?,
                crate::engine::unit::Read::get(r, &keys::value("Message-big", "blob"))?,
            ))
        })
        .unwrap();
    assert_eq!(ok_entity, None);
    assert_eq!(big_entity, None);
    assert_eq!(big_value, None);
}

/// Growing an existing field past `MAX_VALUE_BYTES` via `append` or `merge` is refused and leaves the field untouched
#[test]
fn growing_a_field_past_the_most_a_field_holds_via_append_or_merge_is_refused_and_leaves_it_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let near = "x".repeat(values::MAX_VALUE_BYTES - 20);
    super::write(
        &db,
        vec![
            json!({ "op": "create", "type": "Message", "id": "Message-list", "fields": { "blob": [near] } }),
            json!({ "op": "create", "type": "Message", "id": "Message-map", "fields": { "blob": { "a": near } } }),
        ],
    )
    .unwrap();
    let list_before = stored(&db, "Message-list", "blob");
    let map_before = stored(&db, "Message-map", "blob");

    let refused_append = super::write(
        &db,
        vec![json!({ "op": "append", "id": "Message-list", "field": "blob", "value": "0123456789abcdef0123" })],
    );
    assert_eq!(refused_append.unwrap_err().code, crate::ErrorCode::ValueTooLarge);
    assert_eq!(stored(&db, "Message-list", "blob"), list_before);

    let refused_merge = super::write(
        &db,
        vec![json!({ "op": "merge", "id": "Message-map", "field": "blob", "value": { "b": "0123456789abcdef0123" } })],
    );
    assert_eq!(refused_merge.unwrap_err().code, crate::ErrorCode::ValueTooLarge);
    assert_eq!(stored(&db, "Message-map", "blob"), map_before);
}

/// `bulk_load` refuses an entity whose field exceeds `MAX_VALUE_BYTES` and writes nothing of its unit
#[test]
fn a_bulk_load_of_a_value_past_the_most_a_field_holds_is_refused_and_writes_nothing() {
    use crate::engine::maintenance::Load;
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let ok = Load::Entity {
        id: "Message-load-ok".into(),
        fields: [("text".into(), rmpv::Value::from("kept out"))].into_iter().collect(),
        trashed: false,
    };
    let big = Load::Entity {
        id: "Message-load-big".into(),
        fields: [("blob".into(), rmpv::Value::from("x".repeat(values::MAX_VALUE_BYTES - 4)))].into_iter().collect(),
        trashed: false,
    };
    assert_eq!(db.bulk_load(&[ok, big]).unwrap_err().code, crate::ErrorCode::ValueTooLarge);
    let (ok_entity, big_entity) = db
        .snapshot(|r| {
            Ok((
                crate::engine::unit::Read::get(r, &keys::entity("Message-load-ok"))?,
                crate::engine::unit::Read::get(r, &keys::entity("Message-load-big"))?,
            ))
        })
        .unwrap();
    assert_eq!(ok_entity, None);
    assert_eq!(big_entity, None);
}
