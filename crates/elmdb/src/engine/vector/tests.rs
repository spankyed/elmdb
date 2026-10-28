//! Vector indexes: chunks and their pending embeddings kept by every write, an embedding stored only for the text it
//! was made from, nearest neighbours over hannoy's graphs within a query's other steps, more like this, hybrid ranking
//! against a fixed corpus, and the graphs changed only as a unit commits, checked and rebuilt like derived data.

use serde_json::{Value as Json, json};

use crate::engine::batch::OpResult;
use crate::engine::plan::{ReadOp, Reads};
use crate::error::ErrorCode;
use crate::schema::Schema;
use crate::{Database, OpenOptions};

const DIMS: usize = 3;

fn schema_with(indexes: Json) -> Schema {
    Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "relations": { "in": { "cardinality": "many-to-many" } },
        "types": {
            "Doc": {
                "fields": { "title": { "type": "string", "optional": true }, "body": { "type": "any", "optional": true } },
                "indexes": indexes
            },
            "Folder": { "fields": { "name": { "type": "string", "optional": true } } }
        }
    }))
    .unwrap()
}

fn indexes() -> Json {
    json!({
        "meaning": { "kind": "vector", "from": ["title", "body"], "chunk": "paragraph", "dims": DIMS, "model": "m1" },
        "text": { "kind": "text", "fields": { "title": { "weight": 2 }, "body": {} } }
    })
}

fn open(dir: &tempfile::TempDir) -> Database {
    Database::open(dir.path(), OpenOptions { schema: Some(schema_with(indexes())), syncs: false, ..Default::default() })
        .unwrap()
}

fn write(db: &Database, ops: Vec<Json>) -> crate::Result<Vec<OpResult>> {
    crate::engine::tests::write(db, ops)
}

fn decode(bytes: &[u8]) -> Json {
    serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()
}

fn read(db: &Database, op: Json) -> crate::Result<Json> {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    db.read(&reads).map(|b| decode(&b)[0].clone())
}

fn pending(db: &Database) -> Json {
    read(db, json!({ "read": "pendingEmbeddings", "type": "Doc", "index": "meaning" })).unwrap()
}

/// Embeds every pending chunk with `vector_of(text)`, as a caller's worker does
fn embed_pending(db: &Database, vector_of: impl Fn(&str) -> [f32; DIMS]) -> Vec<OpResult> {
    let chunks = pending(db)["chunks"].as_array().unwrap().clone();
    let ops = chunks
        .iter()
        .map(|c| {
            json!({ "op": "embed", "id": c["id"], "index": "meaning", "chunk": c["chunk"], "hash": c["hash"], "model": "m1",
                    "vector": vector_of(c["text"].as_str().unwrap()) })
        })
        .collect();
    write(db, ops).unwrap()
}

fn ids(db: &Database, start: Json, steps: Json) -> Json {
    read(db, json!({ "read": "query", "plan": { "start": start, "steps": steps, "terminal": { "answer": "ids" } } }))
        .unwrap()["ids"]
        .clone()
}

fn docs() -> Json {
    json!({ "from": "types", "types": ["Doc"] })
}

/// Three directions, one per topic: a chunk's vector is its topic's
fn topic(text: &str) -> [f32; DIMS] {
    if text.contains("cat") {
        [1.0, 0.0, 0.0]
    } else if text.contains("car") {
        [0.0, 1.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    }
}

fn corpus(db: &Database) {
    write(db, vec![
        json!({ "op": "create", "type": "Doc", "id": "Doc-cats", "fields": { "title": "cats", "body": "a cat sleeps" } }),
        json!({ "op": "create", "type": "Doc", "id": "Doc-cars", "fields": { "title": "cars", "body": "a car drives" } }),
        json!({ "op": "create", "type": "Doc", "id": "Doc-sea", "fields": { "title": "sea", "body": "waves" } }),
    ])
    .unwrap();
}

#[test]
fn chunks_are_whole_paragraphs_or_markdown_sections() {
    use super::{VectorIndex, chunks_of};
    use crate::schema::{Chunk, Metric};
    let index = |chunk| VectorIndex {
        name: "x".into(),
        from: vec!["a".into()],
        chunk,
        dims: 3,
        metric: Metric::Cosine,
        model: "m".into(),
    };
    let text = Some(rmpv::Value::from("# One\nfirst\n\nsecond\n# Two\nthird"));
    assert_eq!(chunks_of(&index(Chunk::Whole), std::slice::from_ref(&text)), ["# One\nfirst\n\nsecond\n# Two\nthird"]);
    assert_eq!(
        chunks_of(&index(Chunk::Paragraph), std::slice::from_ref(&text)),
        ["# One\nfirst", "second\n# Two\nthird"]
    );
    assert_eq!(chunks_of(&index(Chunk::Heading), &[text]), ["# One\nfirst\n\nsecond", "# Two\nthird"]);
    assert!(chunks_of(&index(Chunk::Whole), &[Some(rmpv::Value::from(3))]).is_empty());
}

#[test]
fn a_chunk_is_pending_until_embedded_and_pending_again_when_its_text_changes() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    let waiting = pending(&db);
    assert_eq!(
        (waiting["model"].clone(), waiting["dims"].clone(), waiting["count"].clone()),
        (json!("m1"), json!(3), json!(6))
    );
    assert!(embed_pending(&db, topic).iter().all(|r| *r == OpResult::Done));
    assert_eq!(pending(&db)["count"], 0);
    assert_eq!(ids(&db, docs(), json!([{ "step": "near", "vector": [1.0, 0.1, 0.0], "k": 1 }])), json!(["Doc-cats"]));

    // The body changes: its chunk is pending again, and an embedding of the old text is stale
    let old = read(&db, json!({ "read": "pendingEmbeddings", "type": "Doc" })).unwrap();
    assert_eq!(old["count"], 0);
    write(&db, vec![json!({ "op": "set", "id": "Doc-cats", "fields": { "body": "a cat purrs" } })]).unwrap();
    let now = pending(&db);
    assert_eq!(
        now["chunks"],
        json!([{ "id": "Doc-cats", "chunk": 1, "hash": super::hash_of("a cat purrs"), "text": "a cat purrs" }])
    );
    let stale = write(&db, vec![json!({ "op": "embed", "id": "Doc-cats", "chunk": 1, "hash": super::hash_of("a cat sleeps"), "model": "m1", "vector": [1.0, 0.0, 0.0] })])
        .unwrap();
    assert_eq!(stale, [OpResult::Stale]);
    assert_eq!(pending(&db)["count"], 1);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
}

#[test]
fn an_embedding_of_another_model_or_size_or_index_is_refused_and_one_of_no_entity_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    let hash = super::hash_of("cats");
    let embed = |model: &str, vector: Json, index: &str| {
        write(&db, vec![json!({ "op": "embed", "id": "Doc-cats", "index": index, "chunk": 0, "hash": hash, "model": model, "vector": vector })])
            .unwrap_err()
            .code
    };
    assert_eq!(embed("m2", json!([1.0, 0.0, 0.0]), "meaning"), ErrorCode::ValueInvalid);
    assert_eq!(embed("m1", json!([1.0, 0.0]), "meaning"), ErrorCode::ValueInvalid);
    assert_eq!(embed("m1", json!([1.0, 0.0, 0.0]), "nope"), ErrorCode::UnknownIndex);
    // No entity has the id: its chunks went with it, as a destroy takes them, so the embedding is of text no longer held
    let missing = write(
        &db,
        vec![
            json!({ "op": "embed", "id": "Doc-none", "chunk": 0, "hash": hash, "model": "m1", "vector": [1.0, 0.0, 0.0] }),
        ],
    );
    assert!(matches!(missing.unwrap()[..], [OpResult::Stale]));
}

#[test]
fn near_ranks_by_distance_within_the_steps_before_it_and_leaves_the_trash_out() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    embed_pending(&db, topic);
    let near = |steps: Json| ids(&db, docs(), steps);
    assert_eq!(near(json!([{ "step": "near", "vector": [0.9, 0.5, 0.0], "k": 2 }])), json!(["Doc-cats", "Doc-cars"]));
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Folder", "id": "Folder-1", "fields": {} }),
            json!({ "op": "link", "source": "Folder-1", "kind": "in", "target": "Doc-cars" }),
            json!({ "op": "link", "source": "Folder-1", "kind": "in", "target": "Doc-sea" }),
        ],
    )
    .unwrap();
    let inside = ids(
        &db,
        json!({ "from": "ids", "ids": ["Folder-1"] }),
        json!([{ "step": "out", "kinds": ["in"] }, { "step": "near", "vector": [1.0, 0.0, 0.0], "k": 1 }]),
    );
    assert_eq!(inside, json!(["Doc-cars"]));
    write(&db, vec![json!({ "op": "trash", "id": "Doc-cats" })]).unwrap();
    assert_eq!(near(json!([{ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 1 }])), json!(["Doc-cars"]));
    let refused = read(
        &db,
        json!({ "read": "query", "plan": { "start": docs(), "steps": [{ "step": "near", "vector": [1.0] }], "terminal": { "answer": "ids" } } }),
    );
    assert_eq!(refused.unwrap_err().code, ErrorCode::ValueInvalid);
}

#[test]
fn like_searches_near_an_entitys_own_embedding_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    write(&db, vec![json!({ "op": "create", "type": "Doc", "id": "Doc-kitten", "fields": { "title": "kitten", "body": "a cat naps" } })]).unwrap();
    embed_pending(&db, topic);
    assert_eq!(ids(&db, docs(), json!([{ "step": "like", "id": "Doc-cats", "k": 1 }])), json!(["Doc-kitten"]));
}

#[test]
fn hybrid_fuses_the_text_and_vector_rankings_on_a_fixed_corpus() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![
        // Matches the words and the meaning: first in both lists
        json!({ "op": "create", "type": "Doc", "id": "Doc-both", "fields": { "title": "engine", "body": "a car engine" } }),
        // Matches the words alone
        json!({ "op": "create", "type": "Doc", "id": "Doc-words", "fields": { "title": "engine", "body": "a search engine" } }),
        // Matches the meaning alone
        json!({ "op": "create", "type": "Doc", "id": "Doc-meaning", "fields": { "title": "motor", "body": "a car motor" } }),
        json!({ "op": "create", "type": "Doc", "id": "Doc-neither", "fields": { "title": "sea", "body": "waves" } }),
    ])
    .unwrap();
    embed_pending(&db, topic);
    let answer = read(
        &db,
        json!({ "read": "query", "plan": {
        "start": docs(),
        "steps": [{ "step": "hybrid", "query": "engine", "vector": [0.0, 1.0, 0.0], "textIndex": "text", "k": 3 }],
        "terminal": { "answer": "hits", "highlights": true }
    } }),
    )
    .unwrap();
    let hits = answer["hits"].as_array().unwrap();
    let order: Vec<&str> = hits.iter().map(|h| h["id"].as_str().unwrap()).collect();
    assert_eq!(order[0], "Doc-both");
    assert_eq!(order.len(), 3);
    assert!(!order.contains(&"Doc-neither"));
    assert_eq!(hits[0]["terms"], json!(["engine"]));
    assert!(hits[0]["highlights"]["title"].is_array());
}

#[test]
fn a_unit_changes_the_graphs_as_it_commits_and_a_rolled_back_one_not_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    let mut unit = db.begin().unwrap();
    let chunks = pending(&db)["chunks"].as_array().unwrap().clone();
    let embed_all = |unit: &mut crate::Unit| {
        for c in &chunks {
            let op = json!({ "op": "embed", "id": c["id"], "chunk": c["chunk"], "hash": c["hash"], "model": "m1", "vector": topic(c["text"].as_str().unwrap()) });
            unit.write(&crate::engine::tests::batch(vec![op])).unwrap();
        }
    };
    let depth = unit.savepoint().unwrap();
    embed_all(&mut unit);
    unit.rollback_to(depth).unwrap();
    unit.commit(false).unwrap();
    assert_eq!(pending(&db)["count"], 6);
    assert_eq!(ids(&db, docs(), json!([{ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 1 }])), json!([]));
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());

    let mut unit = db.begin().unwrap();
    embed_all(&mut unit);
    unit.abort();
    assert_eq!(pending(&db)["count"], 6);

    let mut unit = db.begin().unwrap();
    embed_all(&mut unit);
    unit.commit(false).unwrap();
    assert_eq!(ids(&db, docs(), json!([{ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 1 }])), json!(["Doc-cats"]));
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
}

#[test]
fn a_rebuild_keeps_the_stored_embeddings_and_builds_the_graphs_again() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    embed_pending(&db, topic);
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    assert_eq!(pending(&db)["count"], 0);
    assert_eq!(ids(&db, docs(), json!([{ "step": "near", "vector": [0.0, 0.0, 1.0], "k": 1 }])), json!(["Doc-sea"]));
    // A destroyed entity takes its embeddings and its items with it
    write(&db, vec![json!({ "op": "destroy", "id": "Doc-sea" })]).unwrap();
    let found = ids(&db, docs(), json!([{ "step": "near", "vector": [0.0, 0.0, 1.0], "k": 3 }]));
    assert!(!found.as_array().unwrap().contains(&json!("Doc-sea")), "{found}");
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
}

#[test]
fn a_dump_carries_the_embeddings_so_its_load_finds_by_them_with_nothing_pending() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    embed_pending(&db, topic);
    let work = tempfile::tempdir().unwrap();
    let path = work.path().join("db.dump");
    let report = crate::dump::dump(&db, &path).unwrap();
    assert_eq!(report.embeddings, 6, "a title and a body for each of three: {report:?}");
    let target = work.path().join("loaded");
    crate::dump::load(&path, &target).unwrap();
    let loaded = Database::open(&target, OpenOptions { syncs: false, ..Default::default() }).unwrap();
    assert_eq!(pending(&loaded)["count"], 0);
    assert_eq!(
        ids(&loaded, docs(), json!([{ "step": "near", "vector": [0.0, 0.0, 1.0], "k": 1 }])),
        json!(["Doc-sea"])
    );
    assert!(loaded.check().unwrap().is_clean(), "{:?}", loaded.check().unwrap());
}

#[test]
fn an_index_declared_over_existing_data_queues_its_chunks_and_a_dropped_one_takes_its_embeddings() {
    let dir = tempfile::tempdir().unwrap();
    let options =
        |indexes: Json| OpenOptions { schema: Some(schema_with(indexes)), syncs: false, ..Default::default() };
    let db = Database::open(dir.path(), options(json!({}))).unwrap();
    corpus(&db);
    db.set_schema(schema_with(indexes())).unwrap();
    db.wait_for_index_builds().unwrap();
    assert_eq!(pending(&db)["count"], 6);
    embed_pending(&db, topic);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    db.set_schema(schema_with(json!({}))).unwrap();
    let left = db
        .snapshot(|s| {
            use crate::engine::unit::Read as _;
            let mut n = 0;
            for prefix in [
                crate::format::Prefix::Vector,
                crate::format::Prefix::VectorChunk,
                crate::format::Prefix::VectorItem,
                crate::format::Prefix::VectorGraph,
            ] {
                s.visit(&prefix.key().to_vec(), |_, _| {
                    n += 1;
                    true
                })?;
            }
            Ok(n)
        })
        .unwrap();
    assert_eq!(left, 0);
}

#[test]
fn a_reset_empties_the_graphs_with_everything_else() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    embed_pending(&db, topic);
    db.reset().unwrap();
    corpus(&db);
    assert_eq!(ids(&db, docs(), json!([{ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 1 }])), json!([]));
    embed_pending(&db, topic);
    assert_eq!(ids(&db, docs(), json!([{ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 1 }])), json!(["Doc-cats"]));
}

#[test]
fn near_answers_k_live_entities_when_trashed_ones_sit_closer() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let mut ops: Vec<Json> = (0..30)
        .map(|i| json!({ "op": "create", "type": "Doc", "id": format!("Doc-t{i}"), "fields": { "title": "cat" } }))
        .collect();
    ops.push(json!({ "op": "create", "type": "Doc", "id": "Doc-far1", "fields": { "title": "sea" } }));
    ops.push(json!({ "op": "create", "type": "Doc", "id": "Doc-far2", "fields": { "title": "car" } }));
    write(&db, ops).unwrap();
    embed_pending(&db, topic);
    write(&db, (0..30).map(|i| json!({ "op": "trash", "id": format!("Doc-t{i}") })).collect()).unwrap();
    // The 30 nearest are trashed: the graph is asked again for more until two live ones are found
    let found = ids(&db, docs(), json!([{ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 2 }]));
    assert_eq!(found.as_array().unwrap().len(), 2, "{found}");
}

#[test]
fn near_asks_again_when_one_trashed_entity_s_chunks_fill_what_the_graph_answered() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    // One entity of 60 paragraphs, every one nearer the query than either live entity: the first answers the graph
    // gives are all its chunks, which collapse to one entity, and the graph still holds more
    let body = vec!["a cat"; 60].join("\n\n");
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Doc", "id": "Doc-many", "fields": { "body": body } }),
            json!({ "op": "create", "type": "Doc", "id": "Doc-far1", "fields": { "title": "sea" } }),
            json!({ "op": "create", "type": "Doc", "id": "Doc-far2", "fields": { "title": "car" } }),
        ],
    )
    .unwrap();
    embed_pending(&db, topic);
    write(&db, vec![json!({ "op": "trash", "id": "Doc-many" })]).unwrap();
    let found = ids(&db, docs(), json!([{ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 2 }]));
    assert_eq!(found.as_array().unwrap().len(), 2, "{found}");
}

#[test]
fn an_embedding_of_an_entity_destroyed_since_is_stale_and_the_rest_of_its_batch_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    let chunks = pending(&db)["chunks"].as_array().unwrap().clone();
    write(&db, vec![json!({ "op": "destroy", "id": "Doc-cats" })]).unwrap();
    let ops = chunks
        .iter()
        .map(|c| json!({ "op": "embed", "id": c["id"], "index": "meaning", "chunk": c["chunk"], "hash": c["hash"], "model": "m1", "vector": topic(c["text"].as_str().unwrap()) }))
        .collect();
    let results = write(&db, ops).unwrap();
    assert!(results.iter().any(|r| matches!(r, OpResult::Stale)));
    assert!(results.iter().any(|r| matches!(r, OpResult::Done)));
    assert_eq!(pending(&db)["count"], 0);
}

#[test]
fn a_cosine_index_refuses_a_zero_vector_and_a_query_holding_a_number_that_isn_t_finite() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    let c = pending(&db)["chunks"][0].clone();
    let zero = write(&db, vec![json!({ "op": "embed", "id": c["id"], "index": "meaning", "chunk": c["chunk"], "hash": c["hash"], "model": "m1", "vector": [0.0, 0.0, 0.0] })]).unwrap_err();
    assert_eq!(zero.code, ErrorCode::ValueInvalid, "{}", zero.message);
    embed_pending(&db, topic);
    let plan = |vector: Json| json!({ "read": "query", "plan": { "start": docs(), "steps": [{ "step": "near", "vector": vector, "k": 1 }], "terminal": { "answer": "ids" } } });
    // msgpack carries a NaN, which JSON can't: the reads are built as the binding decodes them
    let nan =
        Reads { reads: vec![serde_json::from_value::<ReadOp>(plan(json!([0.0, 0.0, 0.0]))).unwrap()], track: None };
    let mut nan = nan;
    if let ReadOp::Query { plan, .. } = &mut nan.reads[0]
        && let Some(crate::engine::plan::Step::Near { vector, .. }) = plan.steps.first_mut()
    {
        vector[0] = f32::NAN;
    }
    assert_eq!(db.read(&nan).unwrap_err().code, ErrorCode::ValueInvalid);
}

#[test]
fn re_embedding_a_chunk_names_a_tracked_near() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    let cats = pending(&db)["chunks"].as_array().unwrap().iter().find(|c| c["id"] == "Doc-cats").unwrap().clone();
    embed_pending(&db, topic);
    let near = json!({ "read": "query", "plan": { "start": docs(), "steps": [{ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 1 }], "terminal": { "answer": "ids" } } });
    db.read(&Reads { reads: vec![serde_json::from_value::<ReadOp>(near).unwrap()], track: Some(5) }).unwrap();
    // The same chunk under the same hash, with another vector: the graph item stays, the embedding changes
    let again = json!({ "op": "embed", "id": "Doc-cats", "index": "meaning", "chunk": cats["chunk"], "hash": cats["hash"], "model": "m1", "vector": [0.0, 1.0, 0.0] });
    let (_, changes) = db.write(&crate::engine::tests::batch(vec![again]), false).unwrap();
    assert_eq!(changes.touched, [5]);
}

#[test]
fn single_vector_rebuild_spills_identifiers_and_preserves_embeddings_and_neighbors() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    embed_pending(&db, topic);
    let expected = ids(&db, docs(), json!([{ "step":"near","vector":[1.0,0.0,0.0],"k":3 }]));
    super::super::derive::sorted::with_budget(1, || db.rebuild(Some(("Doc", "meaning")))).unwrap();
    assert_eq!(ids(&db, docs(), json!([{ "step":"near","vector":[1.0,0.0,0.0],"k":3 }])), expected);
    assert!(db.check().unwrap().is_clean());
    crate::failpoints::fail_once_here(crate::failpoints::points::SCRATCH);
    assert_eq!(db.rebuild(Some(("Doc", "meaning"))).unwrap_err().code, ErrorCode::Io);
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn vector_flush_drains_multiple_journal_batches_and_index_groups() {
    use crate::engine::unit::Read;
    let dir = tempfile::tempdir().unwrap();
    let mut defs = indexes();
    defs["other"] = json!({"kind":"vector","from":["title"],"dims":DIMS,"model":"m1","metric":"l2"});
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema_with(defs)), syncs: false, ..Default::default() })
            .unwrap();
    let count = 1005;
    write(
        &db,
        (0..count)
            .map(|i| {
                json!({"op":"create","type":"Doc","id":format!("Doc-{i:04}"),
        "fields":{"title":"one chunk"}})
            })
            .collect(),
    )
    .unwrap();
    let mut ops = Vec::new();
    for index in ["meaning", "other"] {
        let pending = read(&db, json!({"read":"pendingEmbeddings","type":"Doc","index":index,"limit":2000})).unwrap();
        let chunks = pending["chunks"].as_array().unwrap();
        assert_eq!(chunks.len(), count);
        for chunk in chunks {
            ops.push(json!({"op":"embed","id":chunk["id"],"index":index,"chunk":chunk["chunk"],
            "hash":chunk["hash"],"model":"m1","vector":[1.0,0.5,0.25]}));
        }
    }
    write(&db, ops).unwrap();
    assert!(
        db.snapshot(|r| r.scan(&crate::format::Prefix::VectorJournal.key().to_vec())).unwrap().is_empty(),
        "commit must drain every journal batch and index group"
    );
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_near_hit_scores_its_nearness_and_a_hybrid_one_its_fused_ranks() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    embed_pending(&db, topic);
    let hits = |step: Json| -> Vec<(String, f64)> {
        let answer = read(
            &db,
            json!({ "read": "query", "plan": { "start": docs(), "steps": [step], "terminal": { "answer": "hits" } } }),
        )
        .unwrap();
        answer["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| (h["id"].as_str().unwrap().into(), h["score"].as_f64().unwrap()))
            .collect()
    };
    // `1 / (1 + distance)`: 1 at no distance, falling below it with distance
    let near = hits(json!({ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 3 }));
    assert_eq!(near[0], ("Doc-cats".into(), 1.0));
    // Nothing to highlight, a vector search matching no terms
    let highlighted = read(
        &db,
        json!({ "read": "query", "plan": { "start": docs(), "steps": [{ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 1 }],
            "terminal": { "answer": "hits", "highlights": true } } }),
    )
    .unwrap();
    assert_eq!(highlighted["hits"][0]["highlights"], json!({}));
    assert!(near[1..].iter().all(|(_, s)| *s > 0.0 && *s < 1.0), "{near:?}");
    // First in both lists: 1 / 61, twice
    let fused = hits(json!({ "step": "hybrid", "query": "cats", "vector": [1.0, 0.0, 0.0], "k": 1 }));
    assert_eq!(fused.len(), 1);
    assert_eq!(fused[0].0, "Doc-cats");
    assert!((fused[0].1 - 2.0 / 61.0).abs() < 1e-12, "{fused:?}");
}

#[test]
fn among_a_set_a_type_without_the_index_holds_no_match_and_one_named_or_started_from_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    write(&db, vec![json!({ "op": "create", "type": "Folder", "id": "Folder-1", "fields": {} })]).unwrap();
    embed_pending(&db, topic);
    let mixed = json!({ "from": "ids", "ids": ["Folder-1", "Doc-cars"] });
    let hits = |start: Json, step: Json| {
        read(
            &db,
            json!({ "read": "query", "plan": { "start": start, "steps": [step], "terminal": { "answer": "hits" } } }),
        )
    };
    let near = json!({ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 2 });
    assert_eq!(hits(mixed.clone(), near.clone()).unwrap()["hits"][0]["id"], "Doc-cars");
    // After a step, among what it kept, each entity once
    let within = read(
        &db,
        json!({ "read": "query", "plan": { "start": docs(), "steps": [{ "step": "within", "ids": ["Doc-cars"] }, near], "terminal": { "answer": "ids" } } }),
    )
    .unwrap();
    assert_eq!(within["ids"], json!(["Doc-cars"]));
    let pair = json!({ "from": "ids", "ids": ["Doc-cats", "Doc-cars"] });
    assert_eq!(
        hits(pair, json!({ "step": "near", "vector": [1.0, 0.0, 0.0], "k": 5 })).unwrap()["hits"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let hybrid = json!({ "step": "hybrid", "query": "car", "vector": [1.0, 0.0, 0.0], "k": 2 });
    let found = hits(mixed.clone(), hybrid.clone()).unwrap();
    assert_eq!(found["hits"].as_array().unwrap().len(), 1);
    assert_eq!(found["hits"][0]["terms"], json!(["car"]), "the text search ran among the set's docs");
    let folders = json!({ "from": "types", "types": ["Folder"] });
    assert_eq!(hits(folders.clone(), near.clone()).unwrap_err().code, ErrorCode::UnknownIndex);
    assert_eq!(hits(folders, hybrid.clone()).unwrap_err().code, ErrorCode::UnknownIndex);
    let mut named = near;
    named["index"] = json!("meaning");
    assert_eq!(hits(mixed.clone(), named).unwrap_err().code, ErrorCode::UnknownIndex);
    let mut named = hybrid;
    named["textIndex"] = json!("text");
    assert_eq!(hits(mixed, named).unwrap_err().code, ErrorCode::UnknownIndex);
}

#[test]
fn hybrid_fuses_the_vector_ranking_four_times_k_deep() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    // Eighty-six docs nearer the vector than `Doc-e0`, `Doc-e1` 47th of them; only the two hold the word, `Doc-e0` the
    // better match
    let mut ops: Vec<Json> = (0..86)
        .map(|i| json!({ "op": "create", "type": "Doc", "id": format!("Doc-f{i}"), "fields": { "body": format!("f{i}") } }))
        .collect();
    ops.push(json!({ "op": "create", "type": "Doc", "id": "Doc-e0", "fields": { "body": "zebra e0" } }));
    ops.push(json!({ "op": "create", "type": "Doc", "id": "Doc-e1", "fields": { "body": "zebra with stripes e1" } }));
    write(&db, ops).unwrap();
    embed_pending(&db, |text| {
        let angle = match text {
            t if t.contains("e0") => 1.5,
            t if t.contains("e1") => 0.465,
            t => t.trim_start_matches('f').parse::<f32>().unwrap() * 0.01,
        };
        [angle.cos(), angle.sin(), 0.0]
    });
    let first = |k: u32| {
        let step = json!({ "step": "hybrid", "query": "zebra", "vector": [1.0, 0.0, 0.0], "k": k });
        read(
            &db,
            json!({ "read": "query", "plan": { "start": docs(), "steps": [step], "terminal": { "answer": "ids" } } }),
        )
        .unwrap()["ids"][0]
            .clone()
    };
    // At k 20 the vector list is 80 deep, so `Doc-e1`'s place in it adds to its score and lifts it over `Doc-e0`
    assert_eq!(first(20), "Doc-e1");
    // At k 5 it is 40 deep, without `Doc-e1`
    assert_eq!(first(5), "Doc-e0");
}

#[test]
fn each_of_two_vector_indexes_declared_over_existing_data_queues_its_own_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let options =
        |indexes: Json| OpenOptions { schema: Some(schema_with(indexes)), syncs: false, ..Default::default() };
    let db = Database::open(dir.path(), options(json!({}))).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Doc", "id": "Doc-1", "fields": { "title": "a title", "body": "a body" } }),
        ],
    )
    .unwrap();
    db.set_schema(schema_with(json!({
        "ofTitle": { "kind": "vector", "from": ["title"], "dims": DIMS, "model": "m1" },
        "ofBody": { "kind": "vector", "from": ["body"], "dims": DIMS, "model": "m1" }
    })))
    .unwrap();
    db.wait_for_index_builds().unwrap();
    for (index, text) in [("ofTitle", "a title"), ("ofBody", "a body")] {
        let waiting = read(&db, json!({ "read": "pendingEmbeddings", "type": "Doc", "index": index })).unwrap();
        let texts: Vec<&str> =
            waiting["chunks"].as_array().unwrap().iter().map(|c| c["text"].as_str().unwrap()).collect();
        assert_eq!(texts, [text], "{index}: {waiting}");
    }
}
