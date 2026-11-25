//! Failpoint interruption tests for migrations (`rename_type`, `rename_field`, `rename_relation`): a failure at
//! `COMMIT` (or `SCRATCH` during `rename_type`'s rebuild) aborts the unit cleanly, leaving the sequence, primary data,
//! every derived index (including hannoy vector graphs and rank indexes), and checkpoints intact.

use serde_json::{Value as Json, json};

use super::write;
use crate::engine::answer::{self, Answer, Hit};
use crate::engine::plan::{ReadOp, Reads};
use crate::failpoints::{fail_once_here, is_armed_here, points};
use crate::schema::{SCHEMA_LANGUAGE, Schema};
use crate::{Database, ErrorCode, OpenOptions, Position};

fn read_answer(db: &Database, op: Json) -> Answer {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    let bytes = db.read(&reads).unwrap();
    let mut answers = answer::conforms(&bytes).unwrap().0;
    assert_eq!(answers.len(), 1);
    answers.pop().unwrap()
}

fn read_json(db: &Database, op: Json) -> Json {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    let bytes = db.read(&reads).unwrap();
    let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    answers[0].take()
}

fn assert_hits_close(label: &str, actual: &[Hit], expected: &[Hit], eps: f64) {
    let actual_ids: Vec<&str> = actual.iter().map(|h| h.id.as_str()).collect();
    let expected_ids: Vec<&str> = expected.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(actual_ids, expected_ids, "{label}: hit ids differ");
    for (a, e) in actual.iter().zip(expected.iter()) {
        assert!((a.score - e.score).abs() <= eps, "{label}: score for {} differs ({} vs {})", a.id, a.score, e.score);
    }
}

fn rename_type_schema() -> Schema {
    let item_def = |prefix: &str| {
        json!({
            "shortCode": { "prefix": prefix },
            "changeOrder": true,
            "fields": {
                "status": { "type": "string", "optional": true },
                "slug": { "type": "string", "optional": true },
                "group": { "type": "string", "optional": true },
                "priority": { "type": "number", "optional": true },
                "title": { "type": "string", "optional": true },
                "body": { "type": "string", "optional": true }
            },
            "indexes": {
                "byStatus": { "kind": "value", "fields": ["status"] },
                "bySlug": { "kind": "value", "fields": ["slug"], "unique": true },
                "byGroupPriority": { "kind": "value", "fields": ["group", "priority"] },
                "byTitle": { "kind": "value", "fields": ["title"] },
                "text": { "kind": "text", "fields": { "title": { "weight": 2 }, "body": {} } },
                "grams": { "kind": "trigram", "fields": ["title", "body"] },
                "meaning": {
                    "kind": "vector",
                    "from": ["title", "body"],
                    "dims": 4,
                    "metric": "cosine",
                    "model": "m"
                }
            }
        })
    };
    Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "collation": "en",
        "relations": {
            "weighs": { "cardinality": "many-to-many", "weighted": true }
        },
        "types": {
            "OldItem": item_def("OLD"),
            "NewItem": item_def("NEW"),
            "Holder": {
                "fields": {
                    "target": { "type": "ref", "to": ["OldItem", "NewItem"], "optional": true }
                }
            }
        },
        "ranks": {
            "itemRank": { "types": ["OldItem", "NewItem"], "relations": ["weighs"] }
        }
    }))
    .unwrap()
}

struct RenameTypeSnapshot {
    old_rows: Json,
    new_count: Json,
    holder_row: Json,
    relations: Json,
    by_status: Json,
    by_slug: Json,
    compound: Json,
    collation: Json,
    text: Json,
    trigram_containing: Json,
    trigram_matching: Json,
    vector_near: Vec<Hit>,
    vector_like: Vec<Hit>,
    vector_hybrid: Vec<Hit>,
    rank_hits: Vec<Hit>,
    changed_old: crate::Changed,
    checkpoint: Option<Vec<u8>>,
}

fn capture_rename_type(db: &Database, entity_type: &str, like_id: &str) -> RenameTypeSnapshot {
    let old_rows = read_json(
        db,
        json!({
            "read": "query",
            "plan": { "start": { "from": "types", "types": [entity_type] }, "terminal": { "answer": "rows" } }
        }),
    );
    let other_type = if entity_type == "OldItem" { "NewItem" } else { "OldItem" };
    let new_count = read_json(
        db,
        json!({
            "read": "query",
            "plan": { "start": { "from": "types", "types": [other_type] }, "terminal": { "answer": "count" } }
        }),
    );
    let holder_row = read_json(db, json!({ "read": "row", "id": "Holder-1" }));
    let relations = read_json(db, json!({ "read": "findRelations", "kind": "weighs" }));
    let by_status = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": [entity_type] },
                "steps": [{ "step": "where", "field": "status", "value": "open" }],
                "terminal": { "answer": "ids" }
            }
        }),
    );
    let by_slug = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": [entity_type] },
                "steps": [{ "step": "where", "field": "slug", "value": "slug-2" }],
                "terminal": { "answer": "ids" }
            }
        }),
    );
    let compound = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": [entity_type] },
                "steps": [
                    { "step": "where", "field": "group", "value": "eng" },
                    { "step": "range", "field": "priority", "atLeast": 1, "atMost": 3 },
                    { "step": "orderBy", "field": "priority", "descending": false }
                ],
                "terminal": { "answer": "ids" }
            }
        }),
    );
    let collation = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": [entity_type] },
                "steps": [{ "step": "orderBy", "field": "title", "descending": false }],
                "terminal": { "answer": "ids" }
            }
        }),
    );
    let text = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": [entity_type] },
                "steps": [{ "step": "search", "query": "telemetry" }],
                "terminal": { "answer": "hits" }
            }
        }),
    );
    let trigram_containing = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": [entity_type] },
                "steps": [{ "step": "containing", "text": "ERR-9001" }],
                "terminal": { "answer": "ids" }
            }
        }),
    );
    let trigram_matching = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": [entity_type] },
                "steps": [{ "step": "matching", "pattern": r"ERR-\d{4}" }],
                "terminal": { "answer": "ids" }
            }
        }),
    );
    let Answer::Hits { hits: vector_near, .. } = read_answer(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": [entity_type] },
                "steps": [{ "step": "near", "vector": [0.98, 0.19, 0.0, 0.0], "k": 4 }],
                "terminal": { "answer": "hits" }
            }
        }),
    ) else {
        panic!("near did not answer hits");
    };
    let Answer::Hits { hits: vector_like, .. } = read_answer(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": [entity_type] },
                "steps": [{ "step": "like", "id": like_id, "k": 3 }],
                "terminal": { "answer": "hits" }
            }
        }),
    ) else {
        panic!("like did not answer hits");
    };
    let Answer::Hits { hits: vector_hybrid, .. } = read_answer(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": [entity_type] },
                "steps": [{ "step": "hybrid", "query": "telemetry", "vector": [0.98, 0.19, 0.0, 0.0], "k": 4 }],
                "terminal": { "answer": "hits" }
            }
        }),
    ) else {
        panic!("hybrid did not answer hits");
    };
    let Answer::Hits { hits: rank_hits, .. } = read_answer(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["OldItem", "NewItem"] },
                "steps": [{ "step": "ranked", "index": "itemRank" }],
                "terminal": { "answer": "hits" }
            }
        }),
    ) else {
        panic!("ranked did not answer hits");
    };
    let changed_old = db.changed_since(&[entity_type.into()], &Position { sequence: 0, after: None }, 100).unwrap();
    let checkpoint = db.checkpoint("sync").unwrap();

    RenameTypeSnapshot {
        old_rows,
        new_count,
        holder_row,
        relations,
        by_status,
        by_slug,
        compound,
        collation,
        text,
        trigram_containing,
        trigram_matching,
        vector_near,
        vector_like,
        vector_hybrid,
        rank_hits,
        changed_old,
        checkpoint,
    }
}

fn assert_rename_type_unchanged(label: &str, actual: &RenameTypeSnapshot, expected: &RenameTypeSnapshot) {
    assert_eq!(actual.old_rows, expected.old_rows, "{label}: old_rows");
    assert_eq!(actual.new_count, expected.new_count, "{label}: new_count");
    assert_eq!(actual.holder_row, expected.holder_row, "{label}: holder_row");
    assert_eq!(actual.relations, expected.relations, "{label}: relations");
    assert_eq!(actual.by_status, expected.by_status, "{label}: by_status");
    assert_eq!(actual.by_slug, expected.by_slug, "{label}: by_slug");
    assert_eq!(actual.compound, expected.compound, "{label}: compound");
    assert_eq!(actual.collation, expected.collation, "{label}: collation");
    assert_eq!(actual.text, expected.text, "{label}: text");
    assert_eq!(actual.trigram_containing, expected.trigram_containing, "{label}: trigram_containing");
    assert_eq!(actual.trigram_matching, expected.trigram_matching, "{label}: trigram_matching");
    assert_hits_close(&format!("{label}: vector_near"), &actual.vector_near, &expected.vector_near, 1e-6);
    assert_hits_close(&format!("{label}: vector_like"), &actual.vector_like, &expected.vector_like, 1e-6);
    assert_hits_close(&format!("{label}: vector_hybrid"), &actual.vector_hybrid, &expected.vector_hybrid, 1e-6);
    assert_hits_close(&format!("{label}: rank_hits"), &actual.rank_hits, &expected.rank_hits, 1e-6);
    assert_eq!(actual.changed_old, expected.changed_old, "{label}: changed_old");
    assert_eq!(actual.checkpoint, expected.checkpoint, "{label}: checkpoint");
}

#[test]
fn rename_type_interrupted_at_commit_or_scratch_rolls_back_all_indexes_and_hannoy_graph() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions { schema: Some(rename_type_schema()), syncs: false, ..Default::default() },
    )
    .unwrap();

    write(
        &db,
        vec![
            json!({
                "op": "create", "type": "OldItem", "id": "OldItem-1",
                "fields": { "status": "open", "slug": "slug-1", "group": "eng", "priority": 2, "title": "apple", "body": "telemetry ERR-9001 alert" }
            }),
            json!({
                "op": "create", "type": "OldItem", "id": "OldItem-2",
                "fields": { "status": "open", "slug": "slug-2", "group": "eng", "priority": 1, "title": "Banana", "body": "telemetry pipeline ERR-9002" }
            }),
            json!({
                "op": "create", "type": "OldItem", "id": "OldItem-3",
                "fields": { "status": "closed", "slug": "slug-3", "group": "eng", "priority": 3, "title": "banana", "body": "quiet hub node" }
            }),
            json!({
                "op": "create", "type": "OldItem", "id": "OldItem-4",
                "fields": { "status": "open", "slug": "slug-4", "group": "ops", "priority": 4, "title": "café", "body": "edge storage note" }
            }),
            json!({ "op": "create", "type": "Holder", "id": "Holder-1", "fields": { "target": "OldItem-1" } }),
            json!({ "op": "link", "source": "OldItem-1", "kind": "weighs", "target": "OldItem-3", "weight": 4.0, "info": { "n": 1 } }),
            json!({ "op": "link", "source": "OldItem-2", "kind": "weighs", "target": "OldItem-3", "weight": 2.5, "info": { "n": 2 } }),
            json!({ "op": "link", "source": "OldItem-4", "kind": "weighs", "target": "OldItem-2", "weight": 1.5 }),
        ],
    )
    .unwrap();

    db.wait_for_index_builds().unwrap();
    let Answer::Pending { chunks, .. } =
        read_answer(&db, json!({ "read": "pendingEmbeddings", "type": "OldItem", "limit": 50 }))
    else {
        panic!("expected pending chunks");
    };
    assert_eq!(chunks.len(), 4);
    let vec_for = |id: &str| match id {
        "OldItem-1" => [1.0f32, 0.0, 0.0, 0.0],
        "OldItem-2" => [0.8, 0.6, 0.0, 0.0],
        "OldItem-3" => [0.0, 1.0, 0.0, 0.0],
        "OldItem-4" => [0.0, 0.0, 1.0, 0.0],
        other => panic!("unexpected {other}"),
    };
    write(
        &db,
        chunks
            .into_iter()
            .map(|c| {
                json!({
                    "op": "embed", "id": c.id, "index": "meaning", "chunk": c.chunk,
                    "hash": c.hash, "model": "m", "vector": vec_for(&c.id)
                })
            })
            .collect(),
    )
    .unwrap();

    {
        let mut unit = db.begin().unwrap();
        unit.set_checkpoint("sync", b"cp-before-rename").unwrap();
        unit.commit(false).unwrap();
    }

    db.wait_for_index_builds().unwrap();
    assert!(db.check().unwrap().is_clean());
    let seq_before = db.sequence().unwrap();
    let expected = capture_rename_type(&db, "OldItem", "OldItem-1");
    assert_eq!(expected.new_count["n"], json!(0));
    assert_eq!(expected.vector_near[0].id, "OldItem-1");
    assert_eq!(expected.rank_hits[0].id, "OldItem-3");
    assert_eq!(expected.checkpoint.as_deref(), Some(&b"cp-before-rename"[..]));

    // 1. Fail at COMMIT
    fail_once_here(points::COMMIT);
    let err = db.rename_type("OldItem", "NewItem").unwrap_err();
    assert!(!is_armed_here(points::COMMIT), "COMMIT failpoint must have fired");
    assert_eq!(err.code, ErrorCode::Io, "{err}");
    assert_eq!(db.sequence().unwrap(), seq_before, "sequence unchanged after COMMIT failure");
    let after_commit = capture_rename_type(&db, "OldItem", "OldItem-1");
    assert_rename_type_unchanged("after COMMIT", &after_commit, &expected);
    let report = db.check().unwrap();
    assert!(report.is_clean(), "check after COMMIT: {report:?}");

    // 2. Fail at SCRATCH inside rename_type's rebuild
    fail_once_here(points::SCRATCH);
    let err = db.rename_type("OldItem", "NewItem").unwrap_err();
    assert!(!is_armed_here(points::SCRATCH), "SCRATCH failpoint must have fired");
    assert_eq!(err.code, ErrorCode::Io, "{err}");
    assert_eq!(db.sequence().unwrap(), seq_before, "sequence unchanged after SCRATCH failure");
    let after_scratch = capture_rename_type(&db, "OldItem", "OldItem-1");
    assert_rename_type_unchanged("after SCRATCH", &after_scratch, &expected);
    let report = db.check().unwrap();
    assert!(report.is_clean(), "check after SCRATCH: {report:?}");

    // 3. Reopen from disk and verify everything survived both aborted migrations intact, then complete the rename.
    db.close().unwrap();
    let db = Database::open(dir.path(), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    db.wait_for_index_builds().unwrap();
    assert_eq!(db.sequence().unwrap(), seq_before);
    let after_reopen = capture_rename_type(&db, "OldItem", "OldItem-1");
    assert_rename_type_unchanged("after reopen", &after_reopen, &expected);
    assert!(db.check().unwrap().is_clean());

    db.rename_type("OldItem", "NewItem").unwrap();
    db.wait_for_index_builds().unwrap();
    let report = db.check().unwrap();
    assert!(report.is_clean(), "check after successful rename_type: {report:?}");
    let migrated = capture_rename_type(&db, "NewItem", "NewItem-1");
    assert_eq!(migrated.new_count["n"], json!(0), "OldItem must now be empty");
    assert_eq!(migrated.holder_row["row"]["target"], json!("NewItem-1"));
    assert_eq!(migrated.vector_near[0].id, "NewItem-1");
    assert_eq!(migrated.rank_hits[0].id, "NewItem-3");
    assert_eq!(migrated.checkpoint.as_deref(), Some(&b"cp-before-rename"[..]));
}

fn rename_field_schema() -> Schema {
    Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": {
            "Item": {
                "fields": {
                    "group": { "type": "string", "optional": true },
                    "oldStatus": { "type": "string", "optional": true },
                    "newStatus": { "type": "string", "optional": true },
                    "oldSlug": { "type": "string", "optional": true },
                    "newSlug": { "type": "string", "optional": true },
                    "oldPriority": { "type": "number", "optional": true },
                    "newPriority": { "type": "number", "optional": true },
                    "oldText": { "type": "string", "optional": true },
                    "newText": { "type": "string", "optional": true }
                },
                "indexes": {
                    "byOldStatus": { "kind": "value", "fields": ["oldStatus"] },
                    "byNewStatus": { "kind": "value", "fields": ["newStatus"] },
                    "byOldSlug": { "kind": "value", "fields": ["oldSlug"], "unique": true },
                    "byNewSlug": { "kind": "value", "fields": ["newSlug"], "unique": true },
                    "byOldComp": { "kind": "value", "fields": ["group", "oldPriority"] },
                    "byNewComp": { "kind": "value", "fields": ["group", "newPriority"] },
                    "textOld": { "kind": "text", "fields": { "oldText": {} } },
                    "textNew": { "kind": "text", "fields": { "newText": {} } },
                    "gramsOld": { "kind": "trigram", "fields": ["oldText"] },
                    "gramsNew": { "kind": "trigram", "fields": ["newText"] },
                    "vecOld": { "kind": "vector", "from": ["oldText"], "dims": 4, "metric": "cosine", "model": "m" },
                    "vecNew": { "kind": "vector", "from": ["newText"], "dims": 4, "metric": "cosine", "model": "m" }
                }
            }
        }
    }))
    .unwrap()
}

#[test]
fn rename_field_interrupted_at_commit_rolls_back_all_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions { schema: Some(rename_field_schema()), syncs: false, ..Default::default() },
    )
    .unwrap();
    let long_slug = format!("long-field-slug-{}", "z".repeat(300));

    write(
        &db,
        vec![
            json!({
                "op": "create", "type": "Item", "id": "Item-1",
                "fields": { "group": "eng", "oldStatus": "open", "oldSlug": "short-1", "oldPriority": 2, "oldText": "telemetry ERR-1111 alpha" }
            }),
            json!({
                "op": "create", "type": "Item", "id": "Item-2",
                "fields": { "group": "eng", "oldStatus": "open", "oldSlug": long_slug, "oldPriority": 1, "oldText": "telemetry ERR-2222 beta" }
            }),
            json!({
                "op": "create", "type": "Item", "id": "Item-3",
                "fields": { "group": "ops", "oldStatus": "closed", "oldSlug": "short-3", "oldPriority": 3, "oldText": "storage compaction gamma" }
            }),
        ],
    )
    .unwrap();

    db.wait_for_index_builds().unwrap();
    let Answer::Pending { chunks, .. } =
        read_answer(&db, json!({ "read": "pendingEmbeddings", "type": "Item", "index": "vecOld", "limit": 50 }))
    else {
        panic!("expected pending chunks");
    };
    assert_eq!(chunks.len(), 3);
    let vec_for = |id: &str| match id {
        "Item-1" => [1.0f32, 0.0, 0.0, 0.0],
        "Item-2" => [0.7, 0.714_142_8, 0.0, 0.0],
        "Item-3" => [0.0, 0.0, 1.0, 0.0],
        other => panic!("unexpected {other}"),
    };
    write(
        &db,
        chunks
            .into_iter()
            .map(|c| {
                json!({
                    "op": "embed", "id": c.id, "index": "vecOld", "chunk": c.chunk,
                    "hash": c.hash, "model": "m", "vector": vec_for(&c.id)
                })
            })
            .collect(),
    )
    .unwrap();

    db.wait_for_index_builds().unwrap();
    assert!(db.check().unwrap().is_clean());
    let seq_before = db.sequence().unwrap();

    let snapshot = |db: &Database| {
        let rows = read_json(
            db,
            json!({ "read": "query", "plan": { "start": { "from": "types", "types": ["Item"] }, "terminal": { "answer": "rows" } } }),
        );
        let status = read_json(
            db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Item"] },
                    "steps": [{ "step": "where", "field": "oldStatus", "value": "open" }],
                    "terminal": { "answer": "ids" }
                }
            }),
        );
        let slug_long = read_json(
            db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Item"] },
                    "steps": [{ "step": "where", "field": "oldSlug", "value": long_slug }],
                    "terminal": { "answer": "ids" }
                }
            }),
        );
        let comp = read_json(
            db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Item"] },
                    "steps": [
                        { "step": "where", "field": "group", "value": "eng" },
                        { "step": "orderBy", "field": "oldPriority", "descending": false }
                    ],
                    "terminal": { "answer": "ids" }
                }
            }),
        );
        let text = read_json(
            db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Item"] },
                    "steps": [{ "step": "search", "index": "textOld", "query": "telemetry" }],
                    "terminal": { "answer": "ids" }
                }
            }),
        );
        let grams = read_json(
            db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Item"] },
                    "steps": [{ "step": "containing", "index": "gramsOld", "text": "ERR-1111" }],
                    "terminal": { "answer": "ids" }
                }
            }),
        );
        let near = read_json(
            db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Item"] },
                    "steps": [{ "step": "near", "index": "vecOld", "vector": [1.0, 0.0, 0.0, 0.0], "k": 3 }],
                    "terminal": { "answer": "ids" }
                }
            }),
        );
        (rows, status, slug_long, comp, text, grams, near)
    };
    let expected = snapshot(&db);
    assert_eq!(expected.1["ids"], json!(["Item-1", "Item-2"]));
    assert_eq!(expected.2["ids"], json!(["Item-2"]));
    assert_eq!(expected.3["ids"], json!(["Item-2", "Item-1"]));
    assert_eq!(expected.5["ids"], json!(["Item-1"]));
    assert_eq!(expected.6["ids"], json!(["Item-1", "Item-2", "Item-3"]));

    // Interrupt every indexed field kind with COMMIT failpoint and assert exact rollback.
    for (from, to) in
        [("oldStatus", "newStatus"), ("oldSlug", "newSlug"), ("oldPriority", "newPriority"), ("oldText", "newText")]
    {
        fail_once_here(points::COMMIT);
        let err = db.rename_field("Item", from, to).unwrap_err();
        assert!(!is_armed_here(points::COMMIT), "COMMIT failpoint must have fired for {from}->{to}");
        assert_eq!(err.code, ErrorCode::Io, "{from}->{to}: {err}");
        assert_eq!(db.sequence().unwrap(), seq_before, "{from}->{to}: sequence must not advance");
        assert_eq!(snapshot(&db), expected, "{from}->{to}: state must be completely unchanged");
        let report = db.check().unwrap();
        assert!(report.is_clean(), "{from}->{to}: {report:?}");
    }

    // Without failpoints, all four renames succeed and update their target indexes cleanly.
    for (from, to) in
        [("oldStatus", "newStatus"), ("oldSlug", "newSlug"), ("oldPriority", "newPriority"), ("oldText", "newText")]
    {
        db.rename_field("Item", from, to).unwrap();
    }
    db.wait_for_index_builds().unwrap();
    let report = db.check().unwrap();
    assert!(report.is_clean(), "after rename_field retries: {report:?}");
    assert_eq!(
        read_json(
            &db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Item"] },
                    "steps": [{ "step": "where", "field": "newSlug", "value": long_slug }],
                    "terminal": { "answer": "ids" }
                }
            })
        )["ids"],
        json!(["Item-2"])
    );
    assert_eq!(
        read_json(
            &db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Item"] },
                    "steps": [{ "step": "containing", "index": "gramsNew", "text": "ERR-1111" }],
                    "terminal": { "answer": "ids" }
                }
            })
        )["ids"],
        json!(["Item-1"])
    );
}

#[test]
fn rename_relation_interrupted_at_commit_rolls_back_weighted_edges_and_rank_index() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "relations": {
            "old_rel": { "cardinality": "many-to-many", "weighted": true },
            "new_rel": { "cardinality": "many-to-many", "weighted": true }
        },
        "types": { "Item": { "fields": {} } },
        "ranks": {
            "oldRank": { "types": ["Item"], "relations": ["old_rel"] },
            "bothRank": { "types": ["Item"], "relations": ["old_rel", "new_rel"] }
        }
    }))
    .unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap();

    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Item", "id": "Item-1" }),
            json!({ "op": "create", "type": "Item", "id": "Item-2" }),
            json!({ "op": "create", "type": "Item", "id": "Item-3" }),
            json!({ "op": "create", "type": "Item", "id": "Item-4" }),
            json!({ "op": "link", "source": "Item-1", "kind": "old_rel", "target": "Item-3", "weight": 4.0, "info": { "tag": "a" } }),
            json!({ "op": "link", "source": "Item-2", "kind": "old_rel", "target": "Item-3", "weight": 3.0, "info": { "tag": "b" } }),
            json!({ "op": "link", "source": "Item-4", "kind": "old_rel", "target": "Item-2", "weight": 2.0, "info": { "tag": "c" } }),
            json!({ "op": "link", "source": "Item-4", "kind": "old_rel", "target": "Item-1", "weight": 0.5 }),
        ],
    )
    .unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(db.check().unwrap().is_clean());

    let seq_before = db.sequence().unwrap();
    let old_rels = read_json(&db, json!({ "read": "findRelations", "kind": "old_rel" }));
    let new_rels = read_json(&db, json!({ "read": "findRelations", "kind": "new_rel" }));
    assert_eq!(old_rels["relations"].as_array().unwrap().len(), 4);
    assert_eq!(new_rels["relations"].as_array().unwrap().len(), 0);
    let rank_hits = |index: &str| -> Vec<Hit> {
        let Answer::Hits { hits, .. } = read_answer(
            &db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Item"] },
                    "steps": [{ "step": "ranked", "index": index }],
                    "terminal": { "answer": "hits" }
                }
            }),
        ) else {
            panic!("expected hits");
        };
        hits
    };
    let old_rank_before = rank_hits("oldRank");
    let both_rank_before = rank_hits("bothRank");
    assert_eq!(old_rank_before[0].id, "Item-3");
    assert_eq!(both_rank_before[0].id, "Item-3");

    fail_once_here(points::COMMIT);
    let err = db.rename_relation("old_rel", "new_rel").unwrap_err();
    assert!(!is_armed_here(points::COMMIT), "COMMIT failpoint must have fired");
    assert_eq!(err.code, ErrorCode::Io, "{err}");
    assert_eq!(db.sequence().unwrap(), seq_before, "sequence unchanged after failed rename_relation");
    assert_eq!(read_json(&db, json!({ "read": "findRelations", "kind": "old_rel" })), old_rels);
    assert_eq!(read_json(&db, json!({ "read": "findRelations", "kind": "new_rel" })), new_rels);
    assert_hits_close("oldRank after COMMIT fail", &rank_hits("oldRank"), &old_rank_before, 1e-9);
    assert_hits_close("bothRank after COMMIT fail", &rank_hits("bothRank"), &both_rank_before, 1e-9);
    let report = db.check().unwrap();
    assert!(report.is_clean(), "check after failed rename_relation: {report:?}");

    // Retry without failpoint: edges move to new_rel with weights/info preserved, and bothRank scores remain intact.
    db.rename_relation("old_rel", "new_rel").unwrap();
    db.wait_for_index_builds().unwrap();
    let report = db.check().unwrap();
    assert!(report.is_clean(), "check after successful rename_relation: {report:?}");
    assert_eq!(read_json(&db, json!({ "read": "findRelations", "kind": "old_rel" }))["relations"], json!([]));
    assert_eq!(
        read_json(&db, json!({ "read": "findRelations", "kind": "new_rel" }))["relations"].as_array().unwrap().len(),
        4
    );
    assert_hits_close("bothRank after rename_relation", &rank_hits("bothRank"), &both_rank_before, 1e-6);
}
