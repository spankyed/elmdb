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
fn an_embedding_of_another_model_or_size_or_index_is_refused() {
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
    let missing = write(
        &db,
        vec![
            json!({ "op": "embed", "id": "Doc-none", "chunk": 0, "hash": hash, "model": "m1", "vector": [1.0, 0.0, 0.0] }),
        ],
    );
    assert_eq!(missing.unwrap_err().code, ErrorCode::NoSuchEntity);
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
fn an_index_declared_over_existing_data_queues_its_chunks_and_a_dropped_one_takes_its_embeddings() {
    let dir = tempfile::tempdir().unwrap();
    let options =
        |indexes: Json| OpenOptions { schema: Some(schema_with(indexes)), syncs: false, ..Default::default() };
    let db = Database::open(dir.path(), options(json!({}))).unwrap();
    corpus(&db);
    db.set_schema(schema_with(indexes())).unwrap();
    db.wait_for_index_builds();
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
