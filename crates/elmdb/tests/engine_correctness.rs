//! End-to-end regression tests for engine codec, indexes, queries, vector search, ranks, and admin operations.

use std::collections::BTreeMap;

use elmdb::engine::batch::{Batch, Op};
use elmdb::engine::maintenance::Load;
use elmdb::engine::plan::{Plan, ReadOp, Reads, Scope, Start, Step, Terminal};
use elmdb::schema::{SCHEMA_LANGUAGE, Schema};
use elmdb::{Database, ErrorCode, OpenOptions};
use rmpv::Value;
use serde_json::{Value as Json, json};

fn open(dir: &tempfile::TempDir, schema: Schema) -> Database {
    Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap()
}

fn write_json(db: &Database, ops: Vec<Json>) {
    let batch =
        Batch { ops: ops.into_iter().map(|v| serde_json::from_value(v).unwrap()).collect(), now: Some(1_000.0) };
    db.write(&batch, false).unwrap();
}

fn create_op(entity_type: &str, id: &str, fields: BTreeMap<String, Value>) -> Op {
    Op::Create {
        entity_type: entity_type.into(),
        id: Some(id.into()),
        short_code: None,
        created_at: None,
        updated_at: None,
        fields,
    }
}

fn read_json(db: &Database, op: Json) -> elmdb::Result<Json> {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    db.read(&reads).map(|bytes| {
        let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
        answers[0].take()
    })
}

fn read_plan(db: &Database, plan: Plan) -> elmdb::Result<Json> {
    let reads = Reads { reads: vec![ReadOp::Query { plan, explain: false, visits: false }], track: None };
    db.read(&reads).map(|bytes| {
        let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
        answers[0].take()
    })
}

#[test]
fn rank_top_with_infinite_or_huge_limit_does_not_oom_or_panic() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": { "Doc": { "fields": {} } },
        "relations": { "refs": { "cardinality": "many-to-many" } },
        "ranks": { "docRank": { "types": ["Doc"], "relations": ["refs"] } }
    }))
    .unwrap();
    let db = open(&dir, schema);
    write_json(
        &db,
        vec![
            json!({ "op": "create", "type": "Doc", "id": "Doc-1" }),
            json!({ "op": "create", "type": "Doc", "id": "Doc-2" }),
            json!({ "op": "link", "source": "Doc-1", "kind": "refs", "target": "Doc-2" }),
        ],
    );
    db.wait_for_index_builds().unwrap();
    for limit in [f64::INFINITY, 1e18] {
        let plan = Plan {
            scope: Scope::Live,
            start: Start::Types { types: vec!["Doc".into()] },
            steps: vec![
                Step::Ranked { index: "docRank".into(), while_building: Default::default() },
                Step::Limit { n: limit },
            ],
            terminal: Terminal::Ids,
        };
        let answer = read_plan(&db, plan).unwrap();
        assert_eq!(answer["ids"], json!(["Doc-2", "Doc-1"]), "limit={limit}");
    }
}

#[test]
fn distinct_and_group_by_preserve_order_normalize_negative_zero_and_scale_linearly() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": {
            "Item": {
                "fields": { "tag": { "type": "any", "optional": true, "nullable": true } }
            }
        }
    }))
    .unwrap();
    let db = open(&dir, schema);
    let ops = vec![
        create_op("Item", "Item-neg0", BTreeMap::from([("tag".into(), Value::F64(-0.0))])),
        create_op("Item", "Item-pos0", BTreeMap::from([("tag".into(), Value::F64(0.0))])),
        create_op("Item", "Item-int0", BTreeMap::from([("tag".into(), Value::from(0))])),
        create_op("Item", "Item-missing", BTreeMap::new()),
        create_op("Item", "Item-null", BTreeMap::from([("tag".into(), Value::Nil)])),
        create_op("Item", "Item-arr1", BTreeMap::from([("tag".into(), Value::Array(vec![1.into()]))])),
        create_op("Item", "Item-arr2", BTreeMap::from([("tag".into(), Value::Array(vec![1.into()]))])),
        create_op("Item", "Item-s1", BTreeMap::from([("tag".into(), Value::from("alpha"))])),
        create_op("Item", "Item-s2", BTreeMap::from([("tag".into(), Value::from("alpha"))])),
    ];
    db.write(&Batch { ops, now: Some(1_000.0) }, false).unwrap();

    let distinct = read_json(
        &db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Item"] },
                "steps": [{ "step": "distinct", "field": "tag" }],
                "terminal": { "answer": "ids" }
            }
        }),
    )
    .unwrap();
    assert_eq!(distinct["ids"], json!(["Item-neg0", "Item-missing", "Item-arr1", "Item-arr2", "Item-s1"]));

    let grouped = read_json(
        &db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Item"] },
                "steps": [],
                "terminal": { "answer": "groupBy", "field": "tag" }
            }
        }),
    )
    .unwrap();
    let groups: Vec<Vec<String>> = grouped["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry[1].as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned()).collect())
        .collect();
    assert_eq!(
        groups,
        vec![
            vec!["Item-neg0", "Item-pos0", "Item-int0"],
            vec!["Item-missing", "Item-null"],
            vec!["Item-arr1"],
            vec!["Item-arr2"],
            vec!["Item-s1", "Item-s2"],
        ]
    );
}

#[test]
fn hybrid_executes_prefix_steps_once_without_doubling_visits() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": {
            "Doc": {
                "fields": {
                    "text": { "type": "string" },
                    "keep": { "type": "boolean" }
                },
                "indexes": {
                    "text": { "kind": "text", "fields": { "text": { "weight": 1 } } },
                    "meaning": { "kind": "vector", "from": ["text"], "dims": 2, "metric": "cosine", "model": "m" }
                }
            }
        }
    }))
    .unwrap();
    let db = open(&dir, schema);
    write_json(
        &db,
        vec![
            json!({ "op": "create", "type": "Doc", "id": "Doc-1", "fields": { "text": "rust database", "keep": true } }),
            json!({ "op": "create", "type": "Doc", "id": "Doc-2", "fields": { "text": "rust engine", "keep": false } }),
            json!({ "op": "create", "type": "Doc", "id": "Doc-3", "fields": { "text": "other topic", "keep": true } }),
        ],
    );
    let pending = read_json(&db, json!({ "read": "pendingEmbeddings", "type": "Doc" })).unwrap();
    let embeds: Vec<Json> = pending["chunks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            json!({
                "op": "embed",
                "id": c["id"],
                "index": "meaning",
                "chunk": c["chunk"],
                "hash": c["hash"],
                "model": "m",
                "vector": [1.0, 0.0]
            })
        })
        .collect();
    write_json(&db, embeds);

    // A non-indexed leading filter (`keep == true`) visits every entity of `Doc` (3 entities) once when collecting the
    // prefix. Previously `hybrid` called `searched(before)` twice, doubling `kept` / `visited` on the prefix stages.
    let explained = read_json(
        &db,
        json!({
            "read": "query",
            "explain": true,
            "plan": {
                "start": { "from": "types", "types": ["Doc"] },
                "steps": [
                    { "step": "where", "field": "keep", "value": true },
                    { "step": "hybrid", "query": "rust", "vector": [1.0, 0.0], "k": 5 }
                ],
                "terminal": { "answer": "hits" }
            }
        }),
    )
    .unwrap();
    let stages = explained["stages"].as_array().unwrap();
    assert_eq!(stages[1]["visited"], json!(3), "where stage walked types once: {stages:?}");
    assert_eq!(stages[1]["kept"], json!(2), "where stage ran once: {stages:?}");
}

#[test]
fn bulk_loaded_out_of_range_weights_return_errors_in_communities_rank_and_rank_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let base_schema = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": { "Node": { "fields": {} } },
        "relations": { "weighs": { "cardinality": "many-to-many", "weighted": true } }
    }))
    .unwrap();
    let db = open(&dir, base_schema);
    write_json(
        &db,
        vec![
            json!({ "op": "create", "type": "Node", "id": "Node-a" }),
            json!({ "op": "create", "type": "Node", "id": "Node-b" }),
            json!({ "op": "create", "type": "Node", "id": "Node-c" }),
        ],
    );
    // Bulk-load negative weight and overflowing weights (> MAX_WEIGHT)
    db.bulk_load(&[
        Load::Relation {
            id: "Relation-neg".into(),
            kind: "weighs".into(),
            source: "Node-a".into(),
            target: "Node-b".into(),
            info: None,
            weight: Some(-1.0),
        },
        Load::Relation {
            id: "Relation-huge1".into(),
            kind: "weighs".into(),
            source: "Node-b".into(),
            target: "Node-a".into(),
            info: None,
            weight: Some(1e308),
        },
        Load::Relation {
            id: "Relation-huge2".into(),
            kind: "weighs".into(),
            source: "Node-b".into(),
            target: "Node-c".into(),
            info: None,
            weight: Some(1e308),
        },
    ])
    .unwrap();

    // `communities` with `weights: true` refuses cleanly instead of panicking in leiden-rs
    let err = read_json(
        &db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Node"] },
                "terminal": { "answer": "communities", "kinds": ["weighs"], "weights": true }
            }
        }),
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::ValueInvalid);

    // `communities` with `weights: false` ignores the weights and succeeds
    assert!(
        read_json(
            &db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Node"] },
                    "terminal": { "answer": "communities", "kinds": ["weighs"], "weights": false }
                }
            }),
        )
        .is_ok()
    );

    // `rank` with `weights: true` refuses cleanly
    let err = read_json(
        &db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Node"] },
                "steps": [{ "step": "rank", "kinds": ["weighs"], "weights": true }],
                "terminal": { "answer": "ids" }
            }
        }),
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::ValueInvalid);

    // Adding a weighted rank index over those edges fails cleanly instead of panicking with NaN/Inf
    let ranked_schema = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": { "Node": { "fields": {} } },
        "relations": { "weighs": { "cardinality": "many-to-many", "weighted": true } },
        "ranks": {
            "nodeRank": { "types": ["Node"], "relations": ["weighs"], "direction": "out", "weights": true }
        }
    }))
    .unwrap();
    db.set_schema(ranked_schema).unwrap();
    assert_eq!(db.wait_for_index_builds().unwrap_err().code, ErrorCode::IndexBuildFailed);
    assert_eq!(db.rebuild(None).unwrap_err().code, ErrorCode::ValueInvalid);

    // Also test bulk_load into an already-built rank index: refused cleanly instead of panicking
    let dir2 = tempfile::tempdir().unwrap();
    let ranked_schema2 = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": { "Node": { "fields": {} } },
        "relations": { "weighs": { "cardinality": "many-to-many", "weighted": true } },
        "ranks": {
            "nodeRank": { "types": ["Node"], "relations": ["weighs"], "direction": "out", "weights": true }
        }
    }))
    .unwrap();
    let db2 = open(&dir2, ranked_schema2);
    write_json(
        &db2,
        vec![
            json!({ "op": "create", "type": "Node", "id": "Node-a" }),
            json!({ "op": "create", "type": "Node", "id": "Node-b" }),
        ],
    );
    db2.wait_for_index_builds().unwrap();
    let err = db2
        .bulk_load(&[Load::Relation {
            id: "Relation-bad".into(),
            kind: "weighs".into(),
            source: "Node-a".into(),
            target: "Node-b".into(),
            info: None,
            weight: Some(1e308),
        }])
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ValueInvalid);
}

#[test]
fn unique_index_build_and_rebuild_catch_unindexable_duplicates() {
    let long = "x".repeat(300);
    let ts = Value::Ext(-1, 1_700_000_000u32.to_be_bytes().to_vec());
    let map_a = Value::Map(vec![("a".into(), 1.into()), ("b".into(), 2.into()), ("a".into(), 3.into())]);
    let map_b = Value::Map(vec![("a".into(), 3.0.into()), ("b".into(), 2.0.into()), ("a".into(), 1.0.into())]);

    for (label, v1, v2) in [
        ("long string", Value::from(long.as_str()), Value::from(long.as_str())),
        ("map with reordered/repeated keys", map_a, map_b),
        ("timestamp", ts.clone(), ts),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let plain = Schema::from_json(&json!({
            "$schema": SCHEMA_LANGUAGE,
            "types": {
                "Item": {
                    "fields": { "k": { "type": "any", "optional": true } }
                }
            }
        }))
        .unwrap();
        let unique = Schema::from_json(&json!({
            "$schema": SCHEMA_LANGUAGE,
            "types": {
                "Item": {
                    "fields": { "k": { "type": "any", "optional": true } },
                    "indexes": { "k": { "kind": "value", "fields": ["k"], "unique": true } }
                }
            }
        }))
        .unwrap();
        let db = open(&dir, plain);
        db.write(
            &Batch {
                ops: vec![
                    create_op("Item", "Item-1", BTreeMap::from([("k".into(), v1)])),
                    create_op("Item", "Item-2", BTreeMap::from([("k".into(), v2)])),
                ],
                now: Some(1_000.0),
            },
            false,
        )
        .unwrap();

        // 1. Background build (`build_chunk_after`) refuses the unindexable duplicate
        db.set_schema(unique).unwrap();
        assert_eq!(db.wait_for_index_builds().unwrap_err().code, ErrorCode::IndexBuildFailed, "{label}");

        // 2. Single-index rebuild refuses with `UNIQUE`
        assert_eq!(db.rebuild(Some(("Item", "k"))).unwrap_err().code, ErrorCode::Unique, "{label}");

        // 3. Full rebuild leaves the unique index unbuilt
        db.rebuild(None).unwrap();
        let status = db.index_status().unwrap();
        let k_status = status.iter().find(|s| s.entity_type.as_deref() == Some("Item") && s.index == "k").unwrap();
        assert!(!k_status.built, "{label}: {k_status:?}");
    }

    // Compound unique index where a leading field is an unindexable long string
    let dir = tempfile::tempdir().unwrap();
    let plain = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": {
            "Pair": {
                "fields": {
                    "a": { "type": "string" },
                    "b": { "type": "number" }
                }
            }
        }
    }))
    .unwrap();
    let unique = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": {
            "Pair": {
                "fields": {
                    "a": { "type": "string" },
                    "b": { "type": "number" }
                },
                "indexes": { "ab": { "kind": "value", "fields": ["a", "b"], "unique": true } }
            }
        }
    }))
    .unwrap();
    let db = open(&dir, plain);
    write_json(
        &db,
        vec![
            json!({ "op": "create", "type": "Pair", "id": "Pair-1", "fields": { "a": long, "b": 1 } }),
            json!({ "op": "create", "type": "Pair", "id": "Pair-2", "fields": { "a": long, "b": 1 } }),
        ],
    );
    db.set_schema(unique).unwrap();
    assert_eq!(db.wait_for_index_builds().unwrap_err().code, ErrorCode::IndexBuildFailed);
    assert_eq!(db.rebuild(Some(("Pair", "ab"))).unwrap_err().code, ErrorCode::Unique);
}

#[test]
fn rename_field_handles_self_rename_and_validates_target_field_def() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": {
            "Note": {
                "fields": {
                    "raw": { "type": "any", "optional": true, "nullable": true },
                    "count": { "type": "number", "optional": true }
                }
            }
        }
    }))
    .unwrap();
    let db = open(&dir, schema);
    write_json(
        &db,
        vec![json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "raw": "not-a-number" } })],
    );

    // Self-rename is a clean no-op
    db.rename_field("Note", "raw", "raw").unwrap();
    assert_eq!(
        read_json(&db, json!({ "read": "field", "id": "Note-1", "field": "raw" })).unwrap()["value"],
        json!("not-a-number")
    );

    // Renaming a string into a number field is refused with `FIELD_TYPE` and moves nothing
    assert_eq!(db.rename_field("Note", "raw", "count").unwrap_err().code, ErrorCode::FieldType);
    assert_eq!(
        read_json(&db, json!({ "read": "field", "id": "Note-1", "field": "raw" })).unwrap()["value"],
        json!("not-a-number")
    );
    assert_eq!(
        read_json(&db, json!({ "read": "field", "id": "Note-1", "field": "count" })).unwrap()["present"],
        json!(false)
    );

    // Renaming null into a non-nullable field is also refused
    write_json(&db, vec![json!({ "op": "set", "id": "Note-1", "fields": { "raw": null } })]);
    assert_eq!(db.rename_field("Note", "raw", "count").unwrap_err().code, ErrorCode::FieldType);
}

#[test]
fn rename_type_handles_self_rename_and_removes_orphaned_embeddings() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": {
            "Old": {
                "fields": { "text": { "type": "string" } },
                "indexes": {
                    "shared": { "kind": "vector", "from": ["text"], "dims": 2, "metric": "cosine", "model": "m" },
                    "onlyOld": { "kind": "vector", "from": ["text"], "dims": 2, "metric": "cosine", "model": "m" }
                }
            },
            "New": {
                "fields": { "text": { "type": "string" } },
                "indexes": {
                    "shared": { "kind": "vector", "from": ["text"], "dims": 2, "metric": "cosine", "model": "m" }
                }
            }
        }
    }))
    .unwrap();
    let db = open(&dir, schema);
    write_json(&db, vec![json!({ "op": "create", "type": "Old", "id": "Old-1", "fields": { "text": "hello" } })]);

    for index in ["shared", "onlyOld"] {
        let pending = read_json(&db, json!({ "read": "pendingEmbeddings", "type": "Old", "index": index })).unwrap();
        let c = &pending["chunks"][0];
        write_json(
            &db,
            vec![json!({
                "op": "embed",
                "id": "Old-1",
                "index": index,
                "chunk": c["chunk"],
                "hash": c["hash"],
                "model": "m",
                "vector": [1.0, 0.0]
            })],
        );
    }

    // Self-rename is a clean no-op
    db.rename_type("Old", "Old").unwrap();
    assert!(read_json(&db, json!({ "read": "row", "id": "Old-1" })).unwrap()["row"].is_object());

    // Renaming Old -> New moves `shared` embedding and removes orphaned `onlyOld` embedding
    db.rename_type("Old", "New").unwrap();
    assert!(db.check().unwrap().is_clean());
    // `shared` embedding was preserved on `New-1` (no pending chunks), and re-declaring `onlyOld` on `Old` or
    // dumping shows no orphaned `onlyOld` embeddings remain
    let pending_new = read_json(&db, json!({ "read": "pendingEmbeddings", "type": "New", "index": "shared" })).unwrap();
    assert!(pending_new["chunks"].as_array().unwrap().is_empty(), "{pending_new}");
    let dump_dir = tempfile::tempdir().unwrap();
    let dump_file = dump_dir.path().join("after.dump");
    elmdb::dump::dump(&db, &dump_file).unwrap();
    let dump_bytes = std::fs::read(&dump_file).unwrap();
    assert!(
        !dump_bytes.windows("onlyOld".len()).any(|w| w == b"onlyOld")
            || dump_bytes.windows("onlyOld".len()).filter(|w| *w == b"onlyOld").count() == 1,
        "onlyOld must appear only in the schema header, not as a stored embedding load"
    );
}

#[test]
fn vector_edge_cases_stale_like_restore_validation_and_zero_or_huge_k() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": {
            "Doc": {
                "fields": { "text": { "type": "string" } },
                "indexes": {
                    "text": { "kind": "text", "fields": { "text": { "weight": 1 } } },
                    "meaning": { "kind": "vector", "from": ["text"], "dims": 2, "metric": "cosine", "model": "m" }
                }
            }
        }
    }))
    .unwrap();
    let db = open(&dir, schema);
    write_json(
        &db,
        vec![
            json!({ "op": "create", "type": "Doc", "id": "Doc-1", "fields": { "text": "hello world" } }),
            json!({ "op": "create", "type": "Doc", "id": "Doc-2", "fields": { "text": "goodbye rust" } }),
        ],
    );
    let pending = read_json(&db, json!({ "read": "pendingEmbeddings", "type": "Doc" })).unwrap();
    for c in pending["chunks"].as_array().unwrap() {
        let vec = if c["id"] == "Doc-1" { [1.0, 0.0] } else { [0.9, 0.1] };
        write_json(
            &db,
            vec![json!({
                "op": "embed",
                "id": c["id"],
                "index": "meaning",
                "chunk": c["chunk"],
                "hash": c["hash"],
                "model": "m",
                "vector": vec
            })],
        );
    }

    // 1. `restore` rejects all-zero Cosine vectors and non-finite vectors (both declared and undeclared indexes)
    for bad in [
        Load::Embedding {
            id: "Doc-1".into(),
            index: "meaning".into(),
            chunk: 0,
            model: "m".into(),
            hash: "h".into(),
            vector: vec![0.0, 0.0],
        },
        Load::Embedding {
            id: "Doc-1".into(),
            index: "meaning".into(),
            chunk: 0,
            model: "m".into(),
            hash: "h".into(),
            vector: vec![f32::NAN, 1.0],
        },
        Load::Embedding {
            id: "Doc-1".into(),
            index: "removedIdx".into(),
            chunk: 0,
            model: "m".into(),
            hash: "h".into(),
            vector: vec![f32::INFINITY, 1.0],
        },
    ] {
        assert_eq!(db.bulk_load(&[bad]).unwrap_err().code, ErrorCode::ValueInvalid);
    }
    // A finite vector for an undeclared index is still accepted by `restore`
    db.bulk_load(&[Load::Embedding {
        id: "Doc-1".into(),
        index: "removedIdx".into(),
        chunk: 0,
        model: "m".into(),
        hash: "h".into(),
        vector: vec![1.0, 0.0],
    }])
    .unwrap();

    // 2. `k == 0` short-circuits cleanly (while still refusing invalid query vectors) and `k == u32::MAX` succeeds
    for k in [0u32, u32::MAX] {
        let near = read_json(
            &db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Doc"] },
                    "steps": [{ "step": "near", "vector": [1.0, 0.0], "k": k }],
                    "terminal": { "answer": "ids" }
                }
            }),
        )
        .unwrap();
        let expected = if k == 0 { json!([]) } else { json!(["Doc-1", "Doc-2"]) };
        assert_eq!(near["ids"], expected, "near k={k}");

        let hybrid = read_json(
            &db,
            json!({
                "read": "query",
                "plan": {
                    "start": { "from": "types", "types": ["Doc"] },
                    "steps": [{ "step": "hybrid", "query": "hello", "vector": [1.0, 0.0], "k": k }],
                    "terminal": { "answer": "ids" }
                }
            }),
        )
        .unwrap();
        assert_eq!(hybrid["ids"].as_array().unwrap().len(), if k == 0 { 0 } else { 2 }, "hybrid k={k}");
    }
    let bad_zero_k = read_json(
        &db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Doc"] },
                "steps": [{ "step": "near", "vector": [0.0, 0.0], "k": 0 }],
                "terminal": { "answer": "ids" }
            }
        }),
    )
    .unwrap_err();
    assert_eq!(bad_zero_k.code, ErrorCode::ValueInvalid);

    // 3. `Step::Like` ignores a stale stored embedding whose hash doesn't match `d/qc`
    db.bulk_load(&[Load::Embedding {
        id: "Doc-1".into(),
        index: "meaning".into(),
        chunk: 0,
        model: "m".into(),
        hash: "stale-hash".into(),
        vector: vec![1.0, 0.0],
    }])
    .unwrap();
    let like = read_json(
        &db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Doc"] },
                "steps": [{ "step": "like", "id": "Doc-1", "k": 5 }],
                "terminal": { "answer": "ids" }
            }
        }),
    )
    .unwrap();
    assert_eq!(like["ids"], json!([]), "stale stored embedding must not be used for Step::Like");
}
