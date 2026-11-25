//! End-to-end equivalence across `backup()`, `dump()` + `load()`, and `replace_with()` over one database that exercises
//! all 9 index/order kinds together (single-field value, unique including long strings > 256 bytes, compound value,
//! locale string collation `d/xo`, stemming full-text search, trigram, vector + hannoy graph, rank over weighted
//! relations, and `changeOrder`) plus checkpoints, numbering sequences (`shortCode` / `label` / `seq`), compressed
//! values, weighted relations with `info`, and trashed entities.

use elmdb::engine::answer::{self, Answer, Hit};
use elmdb::engine::batch::{Batch, OpResult};
use elmdb::engine::plan::{ReadOp, Reads};
use elmdb::schema::{SCHEMA_LANGUAGE, Schema};
use elmdb::{Database, OpenOptions, Position};
use serde_json::{Value as Json, json};

fn all_indexes_schema() -> Schema {
    Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "collation": "en",
        "roles": { "starred": {} },
        "relations": {
            "weighs": { "cardinality": "many-to-many", "weighted": true },
            "parent_of": { "cardinality": "one-to-many", "acyclic": true }
        },
        "types": {
            "Note": {
                "shortCode": { "prefix": "NTE" },
                "changeOrder": true,
                "fields": {
                    "status": { "type": "string", "optional": true },
                    "slug": { "type": "string", "optional": true },
                    "group": { "type": "string", "optional": true },
                    "priority": { "type": "number", "optional": true },
                    "title": { "type": "string", "optional": true },
                    "body": { "type": "string", "optional": true },
                    "label": { "type": "string", "optional": true },
                    "tags": { "type": "list", "of": { "type": "string" }, "optional": true },
                    "payload": { "type": "string", "optional": true }
                },
                "indexes": {
                    "byStatus": { "kind": "value", "fields": ["status"] },
                    "bySlug": { "kind": "value", "fields": ["slug"], "unique": true },
                    "byGroupPriority": { "kind": "value", "fields": ["group", "priority"] },
                    "byTitle": { "kind": "value", "fields": ["title"] },
                    "text": {
                        "kind": "text",
                        "fields": { "title": { "weight": 2 }, "body": {} },
                        "stem": "en"
                    },
                    "grams": { "kind": "trigram", "fields": ["title", "body", "tags"] },
                    "meaning": {
                        "kind": "vector",
                        "from": ["title", "body"],
                        "dims": 4,
                        "metric": "cosine",
                        "model": "test-embed-v1"
                    }
                }
            }
        },
        "ranks": {
            "noteRank": { "types": ["Note"], "relations": ["weighs"] }
        }
    }))
    .unwrap()
}

fn batch(ops: Vec<Json>, now: f64) -> Batch {
    Batch { ops: ops.into_iter().map(|v| serde_json::from_value(v).unwrap()).collect(), now: Some(now) }
}

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

fn long_slug() -> String {
    // Exceeds MAX_INDEXED_STRING (256 bytes) so the unique index files it under `d/xl`.
    format!("long-unique-slug-{}", "q".repeat(300))
}

fn long_title() -> String {
    // Exceeds MAX_INDEXED_STRING (256 bytes) so collation `d/xo` exercises place-keyed long strings too.
    format!("Zeta-long-collated-title-{}", "m".repeat(300))
}

fn compressed_payload() -> String {
    // Highly compressible payload well over COMPRESS_FROM (256 bytes) so `p/v` stores a `0xc1` zstd frame.
    "compressed telemetry record segment alpha-beta-gamma; ".repeat(40)
}

/// Captured query answers covering every primary feature and every one of the 9 index/order kinds.
struct ReferenceAnswers {
    live_rows: Json,
    trashed_rows: Json,
    all_count: Json,
    trashed_row_direct: Json,
    compressed_field: Json,
    all_relations: Json,
    weighted_from_1: Json,
    edges_both: Json,
    value_status_open: Json,
    value_status_explained: Json,
    unique_short: Json,
    unique_long: Json,
    compound_range: Json,
    compound_explained: Json,
    collation_asc: Json,
    collation_desc: Json,
    collation_explained: Json,
    text_search_hits: Json,
    text_suggest: Json,
    trigram_containing_exact: Json,
    trigram_containing_ci: Json,
    trigram_matching: Json,
    vector_near: Vec<Hit>,
    vector_like: Vec<Hit>,
    vector_hybrid: Vec<Hit>,
    pending_embeddings: Json,
    rank_hits: Vec<Hit>,
}

fn capture_answers(db: &Database) -> ReferenceAnswers {
    let live_rows = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "scope": "live",
                "start": { "from": "types", "types": ["Note"] },
                "terminal": { "answer": "rows" }
            }
        }),
    );
    let trashed_rows = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "scope": "trashed",
                "start": { "from": "types", "types": ["Note"] },
                "terminal": { "answer": "rows" }
            }
        }),
    );
    let all_count = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "scope": "withTrashed",
                "start": { "from": "types", "types": ["Note"] },
                "terminal": { "answer": "count" }
            }
        }),
    );
    let trashed_row_direct = read_json(db, json!({ "read": "row", "id": "Note-7" }));
    let compressed_field = read_json(db, json!({ "read": "field", "id": "Note-1", "field": "payload" }));
    let all_relations = read_json(db, json!({ "read": "findRelations" }));
    let weighted_from_1 = read_json(db, json!({ "read": "findRelations", "kind": "weighs", "source": "Note-1" }));
    let edges_both = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Note"] },
                "terminal": { "answer": "edges", "kinds": ["weighs", "parent_of"], "direction": "both" }
            }
        }),
    );

    // 1. Single-field value index
    let status_plan = json!({
        "start": { "from": "types", "types": ["Note"] },
        "steps": [{ "step": "where", "field": "status", "value": "open" }],
        "terminal": { "answer": "ids" }
    });
    let value_status_open = read_json(db, json!({ "read": "query", "plan": status_plan }));
    let value_status_explained = read_json(db, json!({ "read": "query", "explain": true, "plan": status_plan }));

    // 2. Unique value index (short + long > 256 bytes)
    let unique_short = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Note"] },
                "steps": [{ "step": "where", "field": "slug", "value": "slug-apple" }],
                "terminal": { "answer": "ids" }
            }
        }),
    );
    let unique_long = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Note"] },
                "steps": [{ "step": "where", "field": "slug", "value": long_slug() }],
                "terminal": { "answer": "ids" }
            }
        }),
    );

    // 3. Compound value index (equality + range + order)
    let compound_plan = json!({
        "start": { "from": "types", "types": ["Note"] },
        "steps": [
            { "step": "where", "field": "group", "value": "eng" },
            { "step": "range", "field": "priority", "atLeast": 2, "atMost": 4 },
            { "step": "orderBy", "field": "priority", "descending": false }
        ],
        "terminal": { "answer": "ids" }
    });
    let compound_range = read_json(db, json!({ "read": "query", "plan": compound_plan }));
    let compound_explained = read_json(db, json!({ "read": "query", "explain": true, "plan": compound_plan }));

    // 4. String collation order (`d/xo`, locale `"en"`)
    let collation_asc_plan = json!({
        "start": { "from": "types", "types": ["Note"] },
        "steps": [{ "step": "orderBy", "field": "title", "descending": false }],
        "terminal": { "answer": "ids" }
    });
    let collation_asc = read_json(db, json!({ "read": "query", "plan": collation_asc_plan }));
    let collation_explained = read_json(db, json!({ "read": "query", "explain": true, "plan": collation_asc_plan }));
    let collation_desc = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Note"] },
                "steps": [{ "step": "orderBy", "field": "title", "descending": true }],
                "terminal": { "answer": "ids" }
            }
        }),
    );

    // 5. Full-text search index (with English stemming + highlights + suggest)
    let text_search_hits = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Note"] },
                "steps": [{ "step": "search", "query": "collecting telemetry" }],
                "terminal": { "answer": "hits", "fields": ["title"], "highlights": true }
            }
        }),
    );
    let text_suggest = read_json(db, json!({ "read": "suggest", "type": "Note", "term": "telem" }));

    // 6. Trigram index (`containing` exact + case-insensitive, `matching` regex)
    let trigram_containing_exact = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Note"] },
                "steps": [{ "step": "containing", "text": "ERR-4042" }],
                "terminal": { "answer": "ids" }
            }
        }),
    );
    let trigram_containing_ci = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Note"] },
                "steps": [{ "step": "containing", "text": "err-404", "caseInsensitive": true }],
                "terminal": { "answer": "ids" }
            }
        }),
    );
    let trigram_matching = read_json(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Note"] },
                "steps": [{ "step": "matching", "pattern": r"ERR-\d{4}" }],
                "terminal": { "answer": "ids" }
            }
        }),
    );

    // 7. Vector index (`near`, `like`, `hybrid` over hannoy HNSW graph)
    let Answer::Hits { hits: vector_near, .. } = read_answer(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Note"] },
                "steps": [{ "step": "near", "vector": [0.95, 0.31, 0.0, 0.0], "k": 4 }],
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
                "start": { "from": "types", "types": ["Note"] },
                "steps": [{ "step": "like", "id": "Note-1", "k": 4 }],
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
                "start": { "from": "types", "types": ["Note"] },
                "steps": [{ "step": "hybrid", "query": "telemetry", "vector": [0.95, 0.31, 0.0, 0.0], "k": 4 }],
                "terminal": { "answer": "hits" }
            }
        }),
    ) else {
        panic!("hybrid did not answer hits");
    };
    let pending_embeddings = read_json(db, json!({ "read": "pendingEmbeddings", "type": "Note" }));

    // 8. Rank index (`ranked` over weighted relations)
    let Answer::Hits { hits: rank_hits, .. } = read_answer(
        db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Note"] },
                "steps": [{ "step": "ranked", "index": "noteRank" }],
                "terminal": { "answer": "hits" }
            }
        }),
    ) else {
        panic!("ranked did not answer hits");
    };

    ReferenceAnswers {
        live_rows,
        trashed_rows,
        all_count,
        trashed_row_direct,
        compressed_field,
        all_relations,
        weighted_from_1,
        edges_both,
        value_status_open,
        value_status_explained,
        unique_short,
        unique_long,
        compound_range,
        compound_explained,
        collation_asc,
        collation_desc,
        collation_explained,
        text_search_hits,
        text_suggest,
        trigram_containing_exact,
        trigram_containing_ci,
        trigram_matching,
        vector_near,
        vector_like,
        vector_hybrid,
        pending_embeddings,
        rank_hits,
    }
}

fn explain_indexes(explained: &Json) -> Vec<String> {
    explained["stages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|stage| stage.get("index").and_then(Json::as_str).map(ToOwned::to_owned))
        .collect()
}

fn assert_hits_close(label: &str, actual: &[Hit], expected: &[Hit], eps: f64) {
    let actual_ids: Vec<&str> = actual.iter().map(|h| h.id.as_str()).collect();
    let expected_ids: Vec<&str> = expected.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(actual_ids, expected_ids, "{label}: hit ordering differs");
    for (a, e) in actual.iter().zip(expected.iter()) {
        assert!(
            (a.score - e.score).abs() <= eps,
            "{label}: score for {} differs: got {}, expected {} (eps={eps})",
            a.id,
            a.score,
            e.score
        );
    }
}

fn assert_answers_equivalent(label: &str, actual: &ReferenceAnswers, expected: &ReferenceAnswers) {
    assert_eq!(actual.live_rows, expected.live_rows, "{label}: live_rows");
    assert_eq!(actual.trashed_rows, expected.trashed_rows, "{label}: trashed_rows");
    assert_eq!(actual.all_count, expected.all_count, "{label}: all_count");
    assert_eq!(actual.trashed_row_direct, expected.trashed_row_direct, "{label}: trashed_row_direct");
    assert_eq!(actual.compressed_field, expected.compressed_field, "{label}: compressed_field");
    assert_eq!(actual.all_relations, expected.all_relations, "{label}: all_relations");
    assert_eq!(actual.weighted_from_1, expected.weighted_from_1, "{label}: weighted_from_1");
    assert_eq!(actual.edges_both, expected.edges_both, "{label}: edges_both");
    assert_eq!(actual.value_status_open, expected.value_status_open, "{label}: value_status_open");
    assert_eq!(
        explain_indexes(&actual.value_status_explained),
        explain_indexes(&expected.value_status_explained),
        "{label}: value index plan"
    );
    assert_eq!(actual.unique_short, expected.unique_short, "{label}: unique_short");
    assert_eq!(actual.unique_long, expected.unique_long, "{label}: unique_long");
    assert_eq!(actual.compound_range, expected.compound_range, "{label}: compound_range");
    assert_eq!(
        explain_indexes(&actual.compound_explained),
        explain_indexes(&expected.compound_explained),
        "{label}: compound index plan"
    );
    assert_eq!(actual.collation_asc, expected.collation_asc, "{label}: collation_asc");
    assert_eq!(actual.collation_desc, expected.collation_desc, "{label}: collation_desc");
    assert_eq!(
        explain_indexes(&actual.collation_explained),
        explain_indexes(&expected.collation_explained),
        "{label}: collation index plan"
    );
    assert_eq!(actual.text_search_hits, expected.text_search_hits, "{label}: text_search_hits");
    assert_eq!(actual.text_suggest, expected.text_suggest, "{label}: text_suggest");
    assert_eq!(actual.trigram_containing_exact, expected.trigram_containing_exact, "{label}: trigram_containing_exact");
    assert_eq!(actual.trigram_containing_ci, expected.trigram_containing_ci, "{label}: trigram_containing_ci");
    assert_eq!(actual.trigram_matching, expected.trigram_matching, "{label}: trigram_matching");
    assert_hits_close(&format!("{label}: vector_near"), &actual.vector_near, &expected.vector_near, 1e-5);
    assert_hits_close(&format!("{label}: vector_like"), &actual.vector_like, &expected.vector_like, 1e-5);
    assert_hits_close(&format!("{label}: vector_hybrid"), &actual.vector_hybrid, &expected.vector_hybrid, 1e-5);
    assert_eq!(actual.pending_embeddings, expected.pending_embeddings, "{label}: pending_embeddings");
    assert_hits_close(&format!("{label}: rank_hits"), &actual.rank_hits, &expected.rank_hits, 1e-6);
}

/// Verifies numbering sequences (`shortCode:Note`, `label:Note:Draft`, `seq`) continue past the preserved numbers
/// without reusing any previously assigned number.
fn assert_numbering_continues(label: &str, db: &Database) {
    let (result, _) = db
        .write(
            &batch(
                vec![
                    json!({ "op": "create", "type": "Note", "id": "Note-next", "fields": { "title": "Next" } }),
                    json!({ "op": "takeLabel", "type": "Note", "base": "Draft" }),
                ],
                9_000.0,
            ),
            false,
        )
        .unwrap();
    match &result.results[0] {
        OpResult::Entity { id, short_code, created } => {
            assert_eq!(id, "Note-next", "{label}");
            assert!(*created, "{label}");
            assert_eq!(short_code.as_deref(), Some("NTE-8"), "{label}: shortCode must continue past NTE-7");
        }
        other => panic!("{label}: unexpected create result {other:?}"),
    }
    match &result.results[1] {
        OpResult::Label { label: next_label } => {
            assert_eq!(next_label, "Draft 3", "{label}: label sequence must continue past Draft 2");
        }
        other => panic!("{label}: unexpected takeLabel result {other:?}"),
    }
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{label} after numbering write: {report:?}");
}

#[test]
fn backup_dump_load_and_replace_with_preserve_all_nine_index_kinds_and_features() {
    let root = tempfile::tempdir().unwrap();
    let source_dir = root.path().join("source");
    let backup_dir = root.path().join("backup");
    let dump_file = root.path().join("snapshot.elmdb.dump");
    let loaded_dir = root.path().join("loaded");
    let target_dir = root.path().join("target");

    let schema = all_indexes_schema();
    let source_db =
        Database::open(&source_dir, OpenOptions { schema: Some(schema.clone()), syncs: false, ..Default::default() })
            .unwrap();

    // Commit 1: populate entities exercising value, unique (short + long), compound, collation (mixed case, accents,
    // long string), text (stemming), trigram, vector, compressed payload, shortCode, and label numbering.
    let (c1, _) = source_db
        .write(
            &batch(
                vec![
                    json!({
                        "op": "create",
                        "type": "Note",
                        "id": "Note-1",
                        "fields": {
                            "status": "open",
                            "slug": "slug-apple",
                            "group": "eng",
                            "priority": 3,
                            "title": "apple",
                            "body": "Telemetry runner reported ERR-4042 during ingestion",
                            "tags": ["ops", "ERR-4042"],
                            "payload": compressed_payload()
                        }
                    }),
                    json!({
                        "op": "create",
                        "type": "Note",
                        "id": "Note-2",
                        "fields": {
                            "status": "open",
                            "slug": long_slug(),
                            "group": "eng",
                            "priority": 2,
                            "title": "Banana",
                            "body": "Runners collected telemetry metrics after ERR-4049 recovery",
                            "tags": ["metrics"]
                        }
                    }),
                    json!({
                        "op": "create",
                        "type": "Note",
                        "id": "Note-3",
                        "fields": {
                            "status": "closed",
                            "slug": "slug-banana-lower",
                            "group": "eng",
                            "priority": 4,
                            "title": "banana",
                            "body": "Steady telemetry pipeline with zero alerts",
                            "tags": ["stable"]
                        }
                    }),
                    json!({
                        "op": "create",
                        "type": "Note",
                        "id": "Note-4",
                        "fields": {
                            "status": "open",
                            "slug": "slug-cafe",
                            "group": "eng",
                            "priority": 1,
                            "title": "café",
                            "body": "Database compaction review and notes",
                            "tags": ["storage"]
                        }
                    }),
                    json!({
                        "op": "create",
                        "type": "Note",
                        "id": "Note-5",
                        "fields": {
                            "status": "draft",
                            "slug": "slug-eclair",
                            "group": "design",
                            "priority": 3,
                            "title": "Éclair",
                            "body": "Visual review of ERR-5001 layout",
                            "tags": ["ui", "ERR-5001"]
                        }
                    }),
                    json!({
                        "op": "create",
                        "type": "Note",
                        "id": "Note-6",
                        "fields": {
                            "status": "open",
                            "slug": "slug-long-title",
                            "group": "eng",
                            "priority": 5,
                            "title": long_title(),
                            "body": "Long title entity for collation overflow coverage",
                            "tags": ["edge"]
                        }
                    }),
                    json!({
                        "op": "create",
                        "type": "Note",
                        "id": "Note-7",
                        "fields": {
                            "status": "open",
                            "slug": "slug-trashed",
                            "group": "eng",
                            "priority": 2,
                            "title": "cherry",
                            "body": "Trashed note carrying ERR-4042 and telemetry",
                            "tags": ["archived"]
                        }
                    }),
                    json!({ "op": "takeLabel", "type": "Note", "base": "Draft" }),
                    json!({ "op": "takeLabel", "type": "Note", "base": "Draft" }),
                ],
                1_000.0,
            ),
            false,
        )
        .unwrap();
    assert_eq!(c1.results[7], OpResult::Label { label: "Draft 1".into() });
    assert_eq!(c1.results[8], OpResult::Label { label: "Draft 2".into() });

    let mid_seq = source_db.sequence().unwrap();

    // Commit 2: update Note-1 (moves it in `changeOrder` past the initial commit), grant role, trash Note-7, and
    // create weighted relations with `info` so `noteRank` has a clear unambiguous score order (Note-3 hub > Note-2 > ...).
    source_db
        .write(
            &batch(
                vec![
                    json!({ "op": "set", "id": "Note-1", "fields": { "label": "Draft 1" } }),
                    json!({ "op": "grant", "id": "Note-1", "role": "starred" }),
                    json!({ "op": "trash", "id": "Note-7", "at": 2_500.0 }),
                    json!({
                        "op": "link",
                        "source": "Note-1",
                        "kind": "weighs",
                        "target": "Note-3",
                        "weight": 4.0,
                        "info": { "reason": "primary", "hop": 1 }
                    }),
                    json!({
                        "op": "link",
                        "source": "Note-2",
                        "kind": "weighs",
                        "target": "Note-3",
                        "weight": 3.0,
                        "info": { "reason": "corroborates" }
                    }),
                    json!({
                        "op": "link",
                        "source": "Note-4",
                        "kind": "weighs",
                        "target": "Note-2",
                        "weight": 2.0,
                        "info": { "reason": "upstream" }
                    }),
                    json!({
                        "op": "link",
                        "source": "Note-5",
                        "kind": "weighs",
                        "target": "Note-1",
                        "weight": 0.5,
                        "info": { "reason": "minor" }
                    }),
                    json!({
                        "op": "link",
                        "source": "Note-1",
                        "kind": "parent_of",
                        "target": "Note-2"
                    }),
                ],
                2_000.0,
            ),
            false,
        )
        .unwrap();

    // Commit 3: embed every pending vector chunk with well-separated unit 4D vectors so hannoy's HNSW graph is
    // populated and top-k nearest-neighbor ordering is unambiguous.
    source_db.wait_for_index_builds().unwrap();
    let Answer::Pending { chunks, .. } =
        read_answer(&source_db, json!({ "read": "pendingEmbeddings", "type": "Note", "limit": 100 }))
    else {
        panic!("pendingEmbeddings did not answer Pending");
    };
    assert_eq!(chunks.len(), 7, "all 7 notes (live + trashed) have pending chunks");
    let vec_for = |id: &str| -> [f32; 4] {
        match id {
            "Note-1" => [1.0, 0.0, 0.0, 0.0],
            "Note-2" => [0.8, 0.6, 0.0, 0.0],
            "Note-3" => [0.5, 0.866_025_4, 0.0, 0.0],
            "Note-4" => [0.0, 1.0, 0.0, 0.0],
            "Note-5" => [0.0, 0.0, 1.0, 0.0],
            "Note-6" => [0.0, 0.0, 0.0, 1.0],
            "Note-7" => [0.6, 0.0, 0.8, 0.0],
            other => panic!("unexpected id {other}"),
        }
    };
    let embed_ops: Vec<Json> = chunks
        .into_iter()
        .map(|c| {
            let v = vec_for(&c.id);
            json!({
                "op": "embed",
                "id": c.id,
                "index": "meaning",
                "chunk": c.chunk,
                "hash": c.hash,
                "model": "test-embed-v1",
                "vector": v
            })
        })
        .collect();
    source_db.write(&batch(embed_ops, 3_000.0), false).unwrap();

    // Commit 4: write checkpoints inside a unit.
    {
        let mut unit = source_db.begin().unwrap();
        unit.set_checkpoint("sync-cursor", b"cursor-42").unwrap();
        unit.set_checkpoint("other", b"state-9").unwrap();
        unit.commit(false).unwrap();
    }

    source_db.wait_for_index_builds().unwrap();
    let report = source_db.check().unwrap();
    assert!(report.is_clean(), "source check: {report:?}");

    let source_seq = source_db.sequence().unwrap();
    let ref_answers = capture_answers(&source_db);
    let ref_changed_all =
        source_db.changed_since(&["Note".into()], &Position { sequence: 0, after: None }, 100).unwrap();
    let ref_changed_mid =
        source_db.changed_since(&["Note".into()], &Position { sequence: mid_seq, after: None }, 100).unwrap();

    // Sanity-check that the reference answers actually exercise every index and feature non-trivially.
    assert_eq!(ref_answers.all_count["n"], json!(7));
    assert_eq!(ref_answers.trashed_rows["rows"].as_array().unwrap().len(), 1);
    assert_eq!(ref_answers.compressed_field["value"], json!(compressed_payload()));
    assert_eq!(ref_answers.value_status_open["ids"], json!(["Note-1", "Note-2", "Note-4", "Note-6"]));
    assert!(explain_indexes(&ref_answers.value_status_explained).iter().any(|i| i == "byStatus"));
    assert_eq!(ref_answers.unique_short["ids"], json!(["Note-1"]));
    assert_eq!(ref_answers.unique_long["ids"], json!(["Note-2"]));
    assert_eq!(ref_answers.compound_range["ids"], json!(["Note-2", "Note-1", "Note-3"]));
    assert!(explain_indexes(&ref_answers.compound_explained).iter().any(|i| i == "byGroupPriority"));
    // English collation: apple < banana < Banana < café < Éclair < Zeta-long-... (Note-7 "cherry" is trashed)
    assert_eq!(ref_answers.collation_asc["ids"], json!(["Note-1", "Note-3", "Note-2", "Note-4", "Note-5", "Note-6"]));
    assert!(explain_indexes(&ref_answers.collation_explained).iter().any(|i| i == "byTitle"));
    assert!(!ref_answers.text_search_hits["hits"].as_array().unwrap().is_empty());
    assert_eq!(ref_answers.trigram_containing_exact["ids"], json!(["Note-1"]));
    assert_eq!(ref_answers.trigram_containing_ci["ids"], json!(["Note-1", "Note-2"]));
    assert_eq!(ref_answers.trigram_matching["ids"], json!(["Note-1", "Note-2", "Note-5"]));
    assert_eq!(ref_answers.vector_near[0].id, "Note-1");
    assert_eq!(ref_answers.vector_like[0].id, "Note-2");
    assert!(!ref_answers.vector_hybrid.is_empty());
    assert_eq!(ref_answers.pending_embeddings["count"], json!(0));
    assert_eq!(ref_answers.rank_hits[0].id, "Note-3", "Note-3 has highest inbound weight");
    assert!(ref_changed_all.complete && ref_changed_all.done);
    // Commit 2 touched Note-1..Note-5 (via field/role/link writes) and trashed Note-7; Note-6 stayed at commit 1.
    assert_eq!(ref_changed_mid.ids, vec!["Note-1", "Note-2", "Note-3", "Note-4", "Note-5", "Note-7"]);
    assert!(ref_changed_mid.complete && ref_changed_mid.done);

    // -------------------------------------------------------------------------
    // 1. backup()
    // -------------------------------------------------------------------------
    source_db.backup(&backup_dir).unwrap();
    let backup_db = Database::open(&backup_dir, OpenOptions { syncs: false, ..Default::default() }).unwrap();
    backup_db.wait_for_index_builds().unwrap();
    let backup_report = backup_db.check().unwrap();
    assert!(backup_report.is_clean(), "backup check: {backup_report:?}");
    let backup_answers = capture_answers(&backup_db);
    assert_answers_equivalent("backup", &backup_answers, &ref_answers);
    assert_eq!(backup_db.sequence().unwrap(), source_seq, "backup preserves sequence");
    assert_eq!(backup_db.checkpoint("sync-cursor").unwrap().as_deref(), Some(&b"cursor-42"[..]));
    assert_eq!(backup_db.checkpoint("other").unwrap().as_deref(), Some(&b"state-9"[..]));
    assert_eq!(
        backup_db.changed_since(&["Note".into()], &Position { sequence: 0, after: None }, 100).unwrap(),
        ref_changed_all,
        "backup preserves full changeOrder"
    );
    assert_eq!(
        backup_db.changed_since(&["Note".into()], &Position { sequence: mid_seq, after: None }, 100).unwrap(),
        ref_changed_mid,
        "backup preserves incremental changeOrder"
    );
    assert_numbering_continues("backup", &backup_db);
    backup_db.close().unwrap();

    // -------------------------------------------------------------------------
    // 2. dump() + load()
    // -------------------------------------------------------------------------
    let dumped = elmdb::dump::dump(&source_db, &dump_file).unwrap();
    assert_eq!(dumped.entities, 7);
    assert_eq!(dumped.relations, 5);
    assert_eq!(dumped.embeddings, 7);
    let loaded = elmdb::dump::load(&dump_file, &loaded_dir).unwrap();
    assert_eq!(loaded, dumped);

    let loaded_db = Database::open(&loaded_dir, OpenOptions { syncs: false, ..Default::default() }).unwrap();
    loaded_db.wait_for_index_builds().unwrap();
    let loaded_report = loaded_db.check().unwrap();
    assert!(loaded_report.is_clean(), "dump+load check: {loaded_report:?}");
    let loaded_answers = capture_answers(&loaded_db);
    assert_answers_equivalent("dump+load", &loaded_answers, &ref_answers);
    assert!(
        loaded_db.sequence().unwrap() > source_seq,
        "dump+load sequence ({}) must be numbered past dumped sequence ({source_seq})",
        loaded_db.sequence().unwrap()
    );
    let loaded_since_source =
        loaded_db.changed_since(&["Note".into()], &Position { sequence: source_seq, after: None }, 100).unwrap();
    assert!(!loaded_since_source.complete, "prior changedSince at source_seq must report restart/incomplete");
    let loaded_since_mid =
        loaded_db.changed_since(&["Note".into()], &Position { sequence: mid_seq, after: None }, 100).unwrap();
    assert!(!loaded_since_mid.complete, "prior changedSince at mid_seq must report restart/incomplete");
    assert_eq!(loaded_db.checkpoint("sync-cursor").unwrap(), None, "dump+load does not carry checkpoints");
    assert_numbering_continues("dump+load", &loaded_db);
    loaded_db.close().unwrap();

    // Close source_db before replace_with() since replace_with() refuses a source open in the same process.
    source_db.close().unwrap();

    // -------------------------------------------------------------------------
    // 3. replace_with()
    // -------------------------------------------------------------------------
    let target_db =
        Database::open(&target_dir, OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap();
    target_db
        .write(
            &batch(
                vec![json!({
                    "op": "create",
                    "type": "Note",
                    "id": "Note-target-only",
                    "fields": { "status": "open", "slug": "target-slug", "title": "Target" }
                })],
                500.0,
            ),
            false,
        )
        .unwrap();
    {
        let mut unit = target_db.begin().unwrap();
        unit.set_checkpoint("target-checkpoint", b"before-replace").unwrap();
        unit.commit(false).unwrap();
    }
    let target_seq_before = target_db.sequence().unwrap();

    target_db.replace_with(&source_dir).unwrap();
    target_db.wait_for_index_builds().unwrap();
    let replaced_report = target_db.check().unwrap();
    assert!(replaced_report.is_clean(), "replace_with check: {replaced_report:?}");
    let replaced_answers = capture_answers(&target_db);
    assert_answers_equivalent("replace_with", &replaced_answers, &ref_answers);
    assert_eq!(
        read_json(&target_db, json!({ "read": "row", "id": "Note-target-only" }))["row"],
        json!(null),
        "old target rows must be gone"
    );
    assert!(target_db.sequence().unwrap() > target_seq_before, "replace_with must advance target sequence");
    assert_eq!(
        target_db.checkpoint("target-checkpoint").unwrap(),
        None,
        "replace_with must drop old target checkpoints"
    );
    assert_eq!(target_db.checkpoint("sync-cursor").unwrap(), None, "replace_with must not adopt source checkpoints");
    let replaced_since_target =
        target_db.changed_since(&["Note".into()], &Position { sequence: target_seq_before, after: None }, 100).unwrap();
    assert!(!replaced_since_target.complete, "changedSince from before replace_with must report incomplete");
    assert_numbering_continues("replace_with", &target_db);
    target_db.close().unwrap();
}
