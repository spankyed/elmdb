//! Text search: ranking against fixed corpora, the query's syntax, hits and their highlights, suggestions, fuzzy labels,
//! and the index kept by every write, built for data that predates it, and checked and rebuilt like any derived data.
//! What a text index matches after random writes is the model harness's (`engine/tests/model.rs`).

use serde_json::{Value as Json, json};

use crate::engine::plan::{ReadOp, Reads};
use crate::error::ErrorCode;
use crate::schema::Schema;
use crate::{Database, OpenOptions};

fn schema_with(message_indexes: Json) -> Schema {
    Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "relations": { "in": { "cardinality": "many-to-many" } },
        "types": {
            "Message": {
                "fields": {
                    "text": { "type": "string", "optional": true },
                    "sender": { "type": "string", "optional": true },
                    "at": { "type": "number", "optional": true },
                    "tags": { "type": "list", "of": { "type": "string" }, "optional": true }
                },
                "indexes": message_indexes
            },
            "Doc": {
                "fields": { "title": { "type": "string", "optional": true }, "body": { "type": "any", "optional": true } },
                "indexes": {
                    "text": { "kind": "text", "fields": { "title": { "weight": 3 }, "body": {} } },
                    "stemmed": { "kind": "text", "fields": { "body": {} }, "stem": "en" }
                }
            },
            "Thread": { "fields": { "title": { "type": "string", "optional": true } } },
            "Plain": { "fields": { "title": { "type": "string", "optional": true } } }
        }
    }))
    .unwrap()
}

fn message_text() -> Json {
    json!({ "text": { "kind": "text", "fields": { "text": {}, "tags": {} } } })
}

fn open(dir: &tempfile::TempDir) -> Database {
    let options = OpenOptions { schema: Some(schema_with(message_text())), syncs: false, ..Default::default() };
    Database::open(dir.path(), options).unwrap()
}

fn write(db: &Database, ops: Vec<Json>) {
    crate::engine::tests::write(db, ops).unwrap();
}

fn create(t: &str, id: &str, fields: Json) -> Json {
    json!({ "op": "create", "type": t, "id": id, "fields": fields })
}

fn decode(bytes: &[u8]) -> Json {
    serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()
}

fn read(db: &Database, op: Json) -> crate::Result<Json> {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    db.read(&reads).map(|b| decode(&b)[0].clone())
}

/// The ids a search of a type answers, most relevant first: of its index `text`, which Doc, having two, names
fn search(db: &Database, t: &str, query: &str, options: Json) -> Json {
    let index = if t == "Doc" { json!("text") } else { Json::Null };
    read(
        db,
        json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": [t] },
        "steps": [{ "step": "search", "query": query, "index": index, "options": options }],
        "terminal": { "answer": "ids" }
    } }),
    )
    .unwrap()["ids"]
        .clone()
}

/// A corpus of messages whose ranking each case below says the reason for
fn corpus(db: &Database) {
    write(
        db,
        vec![
            create("Message", "Message-1", json!({ "text": "rust database engine on lmdb" })),
            create("Message", "Message-2", json!({ "text": "the database is fast" })),
            create("Message", "Message-3", json!({ "text": "lmdb lmdb lmdb notes about storage" })),
            create("Message", "Message-4", json!({ "text": "cooking pasta tonight" })),
        ],
    );
}

#[test]
fn ranks_a_term_held_more_often_and_in_a_shorter_text_first() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    // Each holds "database" once; the shorter text ranks first
    assert_eq!(search(&db, "Message", "database", json!({})), json!(["Message-2", "Message-1"]));
    // Three times beats once
    assert_eq!(search(&db, "Message", "lmdb", json!({})), json!(["Message-3", "Message-1"]));
    // A rare term outweighs a common one: both match "lmdb"; only one also holds "rust"
    assert_eq!(search(&db, "Message", "rust lmdb", json!({ "any": true })), json!(["Message-1", "Message-3"]));
}

#[test]
fn a_fields_weight_counts_its_terms_that_many_times() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            create("Doc", "Doc-body", json!({ "title": "other", "body": "search search" })),
            create("Doc", "Doc-title", json!({ "title": "search", "body": "unrelated words here" })),
        ],
    );
    // Once in a field of weight 3 beats twice in a field of weight 1
    assert_eq!(search(&db, "Doc", "search", json!({})), json!(["Doc-title", "Doc-body"]));
    assert_eq!(search(&db, "Doc", "title:search", json!({})), json!(["Doc-title"]));
    assert_eq!(search(&db, "Doc", "search", json!({ "fields": ["body"] })), json!(["Doc-body"]));
}

#[test]
fn every_clause_matches_unless_any_and_negations_phrases_and_or_combine() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    assert_eq!(search(&db, "Message", "rust lmdb", json!({})), json!(["Message-1"]));
    assert_eq!(search(&db, "Message", "lmdb -rust", json!({})), json!(["Message-3"]));
    assert_eq!(search(&db, "Message", "\"database engine\"", json!({})), json!(["Message-1"]));
    assert_eq!(search(&db, "Message", "\"engine database\"", json!({})), json!([]));
    assert_eq!(search(&db, "Message", "pasta OR rust", json!({})), json!(["Message-4", "Message-1"]));
    assert_eq!(search(&db, "Message", "-rust", json!({})), json!([]));
    assert_eq!(search(&db, "Message", "", json!({})), json!([]));
}

#[test]
fn the_last_word_matches_as_a_prefix_and_a_long_word_one_edit_away() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    assert_eq!(search(&db, "Message", "datab", json!({ "prefix": true })), json!(["Message-2", "Message-1"]));
    assert_eq!(search(&db, "Message", "datab", json!({ "fuzzy": false })), json!([]));
    assert_eq!(search(&db, "Message", "databse", json!({})), json!(["Message-2", "Message-1"]));
    assert_eq!(search(&db, "Message", "databse", json!({ "fuzzy": false })), json!([]));
    // Four letters take no edit: "rost" isn't "rust"
    assert_eq!(search(&db, "Message", "rost", json!({})), json!([]));
    // The word itself outranks a term an edit away
    write(&db, vec![create("Message", "Message-5", json!({ "text": "databases" }))]);
    let hits = search(&db, "Message", "databases", json!({}));
    assert_eq!(hits[0], "Message-5");
    // A term one edit away outranks one two away
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            create("Message", "Message-6", json!({ "text": "databaxes" })),
            create("Message", "Message-7", json!({ "text": "databasez" })),
        ],
    );
    let hits = search(&db, "Message", "databasex", json!({ "prefix": false }));
    assert_eq!(hits, json!(["Message-7", "Message-6"]));
}

#[test]
fn identifiers_codes_and_accents_match_as_typed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            create("Message", "Message-1", json!({ "text": "openDatabaseStore fails on THR-12" })),
            create("Message", "Message-2", json!({ "text": "Café crème" })),
            create("Message", "Message-3", json!({ "tags": ["ops", "storage"] })),
        ],
    );
    assert_eq!(search(&db, "Message", "database", json!({})), json!(["Message-1"]));
    assert_eq!(search(&db, "Message", "openDatabaseStore", json!({})), json!(["Message-1"]));
    assert_eq!(search(&db, "Message", "thr-12", json!({})), json!(["Message-1"]));
    assert_eq!(search(&db, "Message", "cafe", json!({})), json!(["Message-2"]));
    assert_eq!(search(&db, "Message", "storage", json!({})), json!(["Message-3"]));
    assert_eq!(search(&db, "Doc", "running", json!({})), json!([]));
}

#[test]
fn a_term_too_long_for_a_key_is_held_by_no_index_and_keeps_its_neighbours_apart() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let long = "8".repeat(2100);
    write(&db, vec![create("Message", "Message-1", json!({ "text": format!("before {long} after") }))]);
    assert_eq!(search(&db, "Message", &long, json!({})), json!([]));
    assert_eq!(search(&db, "Message", "after", json!({})), json!(["Message-1"]));
    assert_eq!(search(&db, "Message", "\"before after\"", json!({})), json!([]));
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_stemmed_index_matches_a_words_other_forms() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![create("Doc", "Doc-1", json!({ "body": "the job runs nightly" }))]);
    let stemmed = |q: &str| {
        read(
            &db,
            json!({ "read": "query", "plan": {
            "start": { "from": "types", "types": ["Doc"] },
            "steps": [{ "step": "search", "query": q, "index": "stemmed" }],
            "terminal": { "answer": "ids" }
        } }),
        )
        .unwrap()["ids"]
            .clone()
    };
    assert_eq!(stemmed("running"), json!(["Doc-1"]));
    assert_eq!(search(&db, "Doc", "running", json!({ "fuzzy": false })), json!([]));
}

#[test]
fn a_stemmed_index_matches_a_prefix_as_typed_and_suggests_words_not_stems() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            create("Doc", "Doc-1", json!({ "body": "running the database nightly" })),
            create("Doc", "Doc-2", json!({ "body": "a quiet afternoon" })),
        ],
    );
    let typed = |q: &str| {
        read(
            &db,
            json!({ "read": "query", "plan": {
            "start": { "from": "types", "types": ["Doc"] },
            "steps": [{ "step": "search", "query": q, "index": "stemmed", "options": { "prefix": true, "fuzzy": false } }],
            "terminal": { "answer": "ids" }
        } }),
        )
        .unwrap()["ids"]
            .clone()
    };
    // "running" is held as "run", which "runn" and "runni" do not start; the word as written does
    for q in ["ru", "run", "runn", "runni", "running", "databa", "database"] {
        assert_eq!(typed(q), json!(["Doc-1"]), "{q}");
    }
    assert_eq!(typed("runx"), json!([]));
    // Only the last word is a prefix
    assert_eq!(typed("runni nightly"), json!([]));
    let answer =
        read(&db, json!({ "read": "suggest", "type": "Doc", "index": "stemmed", "term": "Datab", "limit": 3 }))
            .unwrap();
    assert_eq!(answer["completions"], json!(["database"]));
    let answer = read(&db, json!({ "read": "suggest", "type": "Doc", "index": "stemmed", "term": "runnign" })).unwrap();
    assert_eq!(answer["corrections"], json!(["running"]));
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    write(&db, vec![json!({ "op": "set", "id": "Doc-1", "fields": { "body": "walked home" } })]);
    assert_eq!(typed("runni"), json!([]));
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
}

#[test]
fn recency_halves_a_score_every_half_life() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            create("Message", "Message-old", json!({ "text": "deploy notes", "at": 1000 })),
            create("Message", "Message-new", json!({ "text": "deploy notes", "at": 90_000 })),
        ],
    );
    // Equal texts tie, in creation order
    assert_eq!(search(&db, "Message", "deploy", json!({})), json!(["Message-old", "Message-new"]));
    // Among a set, in the set's order
    let among = read(
        &db,
        json!({ "read": "query", "plan": {
            "start": { "from": "ids", "ids": ["Message-new", "Message-old"] },
            "steps": [{ "step": "search", "query": "deploy" }],
            "terminal": { "answer": "ids" }
        } }),
    )
    .unwrap();
    assert_eq!(among["ids"], json!(["Message-new", "Message-old"]));
    let recent = json!({ "recency": { "field": "at", "halfLife": 10_000, "now": 100_000 } });
    assert_eq!(search(&db, "Message", "deploy", recent), json!(["Message-new", "Message-old"]));
}

#[test]
fn a_search_combines_with_the_steps_around_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    write(
        &db,
        vec![
            create("Thread", "Thread-1", json!({ "title": "storage" })),
            json!({ "op": "link", "source": "Thread-1", "kind": "in", "target": "Message-1" }),
            json!({ "op": "link", "source": "Thread-1", "kind": "in", "target": "Message-3" }),
            json!({ "op": "trash", "id": "Message-3" }),
        ],
    );
    let plan = |start: Json, steps: Json| {
        read(
            &db,
            json!({ "read": "query", "plan": { "start": start, "steps": steps, "terminal": { "answer": "ids" } } }),
        )
        .unwrap()["ids"]
            .clone()
    };
    // Inside a thread: the search runs among the thread's messages; a trashed one is reached by none
    let thread = json!({ "from": "ids", "ids": ["Thread-1"] });
    let inside = json!([{ "step": "out", "kinds": ["in"] }, { "step": "search", "query": "lmdb" }]);
    assert_eq!(plan(thread, inside), json!(["Message-1"]));
    let second = json!({ "from": "ids", "ids": ["Message-2"] });
    assert_eq!(plan(second, json!([{ "step": "search", "query": "database" }])), json!(["Message-2"]));
    // A page and the next, by a cursor
    let all = json!({ "from": "types", "types": ["Message"] });
    // After a step, among what it kept, each entity once
    let within = json!([{ "step": "within", "ids": ["Message-2"] }, { "step": "search", "query": "database" }]);
    assert_eq!(plan(all.clone(), within), json!(["Message-2"]));
    let pair = json!({ "from": "ids", "ids": ["Message-1", "Message-2"] });
    assert_eq!(plan(pair, json!([{ "step": "search", "query": "database" }])), json!(["Message-2", "Message-1"]));
    let first = json!([{ "step": "search", "query": "database lmdb", "options": { "any": true } }, { "step": "limit", "n": 1 }]);
    assert_eq!(plan(all.clone(), first), json!(["Message-1"]));
    let next = json!([{ "step": "search", "query": "database lmdb", "options": { "any": true } }, { "step": "after", "id": "Message-1" }]);
    assert_eq!(plan(all.clone(), next), json!(["Message-2"]));
    // A type with no text index holds no match among several types; one named alone is refused
    let both = json!({ "from": "ids", "ids": ["Message-1", "Thread-1"] });
    assert_eq!(plan(both, json!([{ "step": "search", "query": "rust" }])), json!(["Message-1"]));
    let refused = read(
        &db,
        json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Plain"] },
        "steps": [{ "step": "search", "query": "x" }],
        "terminal": { "answer": "ids" }
    } }),
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::UnknownIndex);
    // Facets: counts per value over the matches
    let counts = read(
        &db,
        json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Message"] },
        "steps": [{ "step": "search", "query": "database" }],
        "terminal": { "answer": "groupBy", "field": "entityType", "counts": true }
    } }),
    )
    .unwrap();
    assert_eq!(counts["groups"], json!([["Message", 2]]));
}

#[test]
fn a_type_with_several_text_indexes_names_the_one_searched() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let unnamed = read(
        &db,
        json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Doc"] },
        "steps": [{ "step": "search", "query": "x" }],
        "terminal": { "answer": "ids" }
    } }),
    )
    .unwrap_err();
    assert_eq!(unnamed.code, ErrorCode::UnknownIndex);
    assert!(unnamed.message.contains("text indexes stemmed, text"), "{}", unnamed.message);
    let named = read(
        &db,
        json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Doc"] },
        "steps": [{ "step": "search", "query": "x", "index": "nope" }],
        "terminal": { "answer": "ids" }
    } }),
    )
    .unwrap_err();
    assert_eq!((named.code, named.message.contains("no text index \"nope\"")), (ErrorCode::UnknownIndex, true));
}

#[test]
fn hits_answer_scores_terms_highlights_in_utf16_units_and_fields() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            create(
                "Message",
                "Message-1",
                json!({ "text": "😀 database, Database!", "sender": "ada", "tags": ["x", "databases"] }),
            ),
            create("Message", "Message-2", json!({ "text": "nothing" })),
        ],
    );
    let answer = read(
        &db,
        json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Message"] },
        "steps": [{ "step": "search", "query": "database" }],
        "terminal": { "answer": "hits", "highlights": true, "fields": ["sender"] }
    } }),
    )
    .unwrap();
    let hits = answer["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    assert_eq!(hit["id"], "Message-1");
    assert!(hit["score"].as_f64().unwrap() > 0.0);
    assert_eq!(hit["terms"], json!(["database", "databases"]));
    // "😀 " is three UTF-16 units: the emoji's two and a space
    assert_eq!(hit["highlights"]["text"], json!([[0, 3, 11], [0, 13, 21]]));
    assert_eq!(hit["highlights"]["tags"], json!([[1, 0, 9]]));
    assert_eq!(hit["row"], json!({ "id": "Message-1", "sender": "ada" }));
    let refused = read(
        &db,
        json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Message"] },
        "terminal": { "answer": "hits" }
    } }),
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::BatchInvalid);
}

#[test]
fn suggestions_complete_a_word_most_held_first_and_correct_one_nearest_first() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            create("Message", "Message-1", json!({ "text": "storage store stored" })),
            create("Message", "Message-2", json!({ "text": "storage stork" })),
        ],
    );
    let answer = read(&db, json!({ "read": "suggest", "type": "Message", "term": "Sto", "limit": 3 })).unwrap();
    assert_eq!(answer["completions"], json!(["storage", "store", "stored"]));
    // One edit for a word of up to six letters: "stored" is two from "storr"; seven or more take two
    let answer = read(&db, json!({ "read": "suggest", "type": "Message", "term": "storr" })).unwrap();
    assert_eq!(answer["corrections"], json!(["store", "stork"]));
    let answer = read(&db, json!({ "read": "suggest", "type": "Message", "term": "storaeg" })).unwrap();
    assert_eq!(answer["corrections"], json!(["storage", "store", "stored"]));
    // A word held is no correction of itself
    let answer = read(&db, json!({ "read": "suggest", "type": "Message", "term": "store" })).unwrap();
    assert_eq!(answer["corrections"], json!(["stored", "stork"]));
    // Two letters take no edit
    write(&db, vec![create("Message", "Message-3", json!({ "text": "go" }))]);
    let answer = read(&db, json!({ "read": "suggest", "type": "Message", "term": "gx" })).unwrap();
    assert_eq!(answer["corrections"], json!([]));
    let answer = read(&db, json!({ "read": "suggest", "type": "Message", "term": "gox" })).unwrap();
    assert_eq!(answer["corrections"], json!(["go"]));
}

#[test]
fn labels_match_a_quick_opens_characters_in_order_across_types() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            create("Thread", "Thread-1", json!({ "title": "Release checklist" })),
            create("Doc", "Doc-1", json!({ "title": "Read carefully" })),
            create("Doc", "Doc-2", json!({ "title": "Unrelated" })),
            json!({ "op": "trash", "id": "Doc-1" }),
        ],
    );
    let found = |scope: &str| {
        read(&db, json!({ "read": "findLabels", "query": "rc", "fields": { "Thread": "title", "Doc": "title" }, "scope": scope }))
            .unwrap()["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(found("live"), ["Thread-1"]);
    assert_eq!(found("withTrashed").len(), 2);
    // Equal matches in creation order, across types
    write(
        &db,
        vec![create("Thread", "Thread-2", json!({ "title": "rc" })), create("Doc", "Doc-3", json!({ "title": "rc" }))],
    );
    assert_eq!(found("live")[..2], ["Thread-2", "Doc-3"]);
}

#[test]
fn hits_of_a_search_say_whether_their_rows_may_hold_proto() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![create("Doc", "Doc-1", json!({ "title": "proto", "body": { "__proto__": "x" } }))]);
    let plain = |fields: Json| {
        read(
            &db,
            json!({ "read": "query", "plan": {
            "start": { "from": "types", "types": ["Doc"] },
            "steps": [{ "step": "search", "query": "proto", "index": "text" }],
            "terminal": { "answer": "hits", "fields": fields }
        } }),
        )
        .unwrap()["plainObjects"]
            .clone()
    };
    assert_eq!(plain(json!(["title"])), true);
    assert_eq!(plain(json!(["body"])), false);
}

#[test]
fn every_write_keeps_the_index_and_check_finds_it_as_primary_data_derives_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    write(
        &db,
        vec![
            json!({ "op": "set", "id": "Message-1", "fields": { "text": "pasta with lmdb" } }),
            json!({ "op": "delete", "id": "Message-2", "field": "text" }),
            json!({ "op": "destroy", "id": "Message-4" }),
            json!({ "op": "append", "id": "Message-3", "field": "tags", "value": "pasta" }),
            json!({ "op": "trash", "id": "Message-3" }),
        ],
    );
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    assert_eq!(search(&db, "Message", "pasta", json!({})), json!(["Message-1"]));
    assert_eq!(search(&db, "Message", "database", json!({})), json!([]));
    // A unit that throws keeps none of its index's writes
    let mut unit = db.begin().unwrap();
    unit.write(&crate::engine::tests::batch(vec![
        json!({ "op": "set", "id": "Message-1", "fields": { "text": "gone" } }),
    ]))
    .unwrap();
    unit.abort();
    assert_eq!(search(&db, "Message", "pasta", json!({})), json!(["Message-1"]));
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean());
    assert_eq!(search(&db, "Message", "pasta", json!({})), json!(["Message-1"]));
}

#[test]
fn an_index_declared_over_existing_data_is_built_in_the_background_and_dropped_with_its_keys() {
    let dir = tempfile::tempdir().unwrap();
    let options =
        |indexes: Json| OpenOptions { schema: Some(schema_with(indexes)), syncs: false, ..Default::default() };
    let db = Database::open(dir.path(), options(json!({}))).unwrap();
    corpus(&db);
    db.set_schema(schema_with(message_text())).unwrap();
    db.wait_for_index_builds().unwrap();
    assert_eq!(search(&db, "Message", "lmdb", json!({})), json!(["Message-3", "Message-1"]));
    assert!(db.check().unwrap().is_clean());
    db.set_schema(schema_with(json!({}))).unwrap();
    let left = db
        .snapshot(|s| {
            use crate::engine::unit::Read as _;
            let mut n = 0;
            for prefix in [
                crate::format::Prefix::TextTerm,
                crate::format::Prefix::TextBlock,
                crate::format::Prefix::TextDocument,
                crate::format::Prefix::TextLengths,
                crate::format::Prefix::TextStats,
                crate::format::Prefix::TextWord,
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
fn a_read_of_a_text_index_still_being_built_is_refused_and_answered_once_it_is_built() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    // As a build leaves it part way: its entries there, its marker not yet
    db.store()
        .unwrap()
        .write(
            |txn, main| {
                main.delete(txn, &crate::engine::keys::index_built("Message", "text"))?;
                Ok(())
            },
            false,
        )
        .unwrap();
    let query = |start: Json| {
        json!({ "read": "query", "plan": {
            "start": start, "steps": [{ "step": "search", "query": "lmdb" }], "terminal": { "answer": "ids" }
        } })
    };
    let from_the_type = read(&db, query(json!({ "from": "types", "types": ["Message"] }))).unwrap_err();
    assert_eq!(from_the_type.code, ErrorCode::IndexBuilding, "{from_the_type}");
    assert!(from_the_type.message.contains("Message") && from_the_type.message.contains("text"), "{from_the_type}");
    let among_a_set = read(&db, query(json!({ "from": "ids", "ids": ["Message-1", "Message-3"] }))).unwrap_err();
    assert_eq!(among_a_set.code, ErrorCode::IndexBuilding, "{among_a_set}");
    let suggested = read(&db, json!({ "read": "suggest", "type": "Message", "term": "lm" })).unwrap_err();
    assert_eq!(suggested.code, ErrorCode::IndexBuilding, "{suggested}");
    // A set holding nothing of the type reads no index of it
    let elsewhere = read(&db, query(json!({ "from": "ids", "ids": [] }))).unwrap();
    assert_eq!(elsewhere["ids"], json!([]));

    db.rebuild(Some(("Message", "text"))).unwrap();
    assert_eq!(search(&db, "Message", "lmdb", json!({})), json!(["Message-3", "Message-1"]));
    assert_eq!(
        read(&db, json!({ "read": "suggest", "type": "Message", "term": "lm" })).unwrap()["completions"],
        json!(["lmdb"])
    );
}

#[test]
fn a_reader_refused_while_a_text_index_is_built_hears_of_the_commit_that_ends_the_build() {
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    db.wait_for_index_builds().unwrap();
    // As a build leaves it part way: its entries there, its marker not yet
    db.store()
        .unwrap()
        .write(
            |txn, main| {
                main.delete(txn, &crate::engine::keys::index_built("Message", "text"))?;
                Ok(())
            },
            false,
        )
        .unwrap();
    let searching = |track: Option<u32>| -> Reads {
        let plan = json!({ "start": { "from": "types", "types": ["Message"] },
            "steps": [{ "step": "search", "query": "lmdb" }], "terminal": { "answer": "ids" } });
        Reads { reads: vec![serde_json::from_value(json!({ "read": "query", "plan": plan })).unwrap()], track }
    };

    // One reader under a token, and one a watch
    assert_eq!(db.read(&searching(Some(7))).unwrap_err().code, ErrorCode::IndexBuilding);
    let answers = Arc::new(Mutex::new(Vec::<Result<Json, ErrorCode>>::new()));
    let into = Arc::clone(&answers);
    db.watch(
        searching(None),
        Box::new(move |answer| {
            into.lock().unwrap().push(answer.as_ref().map(|bytes| decode(bytes)[0]["ids"].clone()).map_err(|e| e.code));
        }),
    )
    .unwrap();
    assert_eq!(*answers.lock().unwrap(), vec![Err(ErrorCode::IndexBuilding)]);
    let heard = Arc::new(Mutex::new(Vec::<Vec<u32>>::new()));
    let hearing = Arc::clone(&heard);
    db.hear_builder_commits(Some(Arc::new(move |_, touched| hearing.lock().unwrap().push(touched)))).unwrap();

    // The builder ends the build: each read the marker to be refused, and the commit that writes it names them
    let builders = db.builders().unwrap().expect("a writable database starts builders");
    builders.queue(vec![crate::engine::Work::Index { entity_type: "Message".into(), index: "text".into() }]);
    db.wait_for_index_builds().unwrap();
    assert_eq!(*heard.lock().unwrap(), vec![vec![7]], "the build ended and told no reader it had refused");
    assert_eq!(
        *answers.lock().unwrap(),
        vec![Err(ErrorCode::IndexBuilding), Ok(json!(["Message-3", "Message-1"]))],
        "the watch was left on its refusal"
    );
    assert_eq!(decode(&db.read(&searching(Some(7))).unwrap())[0]["ids"], json!(["Message-3", "Message-1"]));
}

#[test]
fn a_highlight_names_its_element_by_its_place_in_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![create("Doc", "Doc-1", json!({ "body": [7, "zebra"] }))]);
    let answer = read(
        &db,
        json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Doc"] },
        "steps": [{ "step": "search", "query": "zebra", "index": "text", "options": {} }],
        "terminal": { "answer": "hits", "highlights": true }
    } }),
    )
    .unwrap();
    assert_eq!(answer["hits"][0]["highlights"]["body"], json!([[1, 0, 5]]), "{answer}");
}

#[test]
fn a_query_parses_into_its_clauses_as_typed() {
    use super::search::{RawClause, parse};
    let clause =
        |text: &str| RawClause { text: text.into(), quoted: false, negated: false, field: None, or_previous: false };
    assert_eq!(
        parse("a -b - title:c \"d e\" -\"f\" g OR h"),
        vec![
            clause("a"),
            RawClause { negated: true, ..clause("b") },
            clause("-"),
            RawClause { field: Some("title".into()), ..clause("c") },
            RawClause { quoted: true, ..clause("d e") },
            RawClause { negated: true, quoted: true, ..clause("f") },
            clause("g"),
            RawClause { or_previous: true, ..clause("h") },
        ]
    );
    // A colon followed by a space names no field
    assert_eq!(parse("title: c"), vec![clause("title:"), clause("c")]);
}

/// Each hit's id and score, most relevant first
fn scores(db: &Database, t: &str, query: &str, options: Json) -> Vec<(String, f64)> {
    let index = if t == "Doc" { json!("text") } else { Json::Null };
    let answer = read(
        db,
        json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": [t] },
        "steps": [{ "step": "search", "query": query, "index": index, "options": options }],
        "terminal": { "answer": "hits" }
    } }),
    )
    .unwrap();
    let hits = answer["hits"].as_array().unwrap();
    hits.iter().map(|h| (h["id"].as_str().unwrap().to_owned(), h["score"].as_f64().unwrap())).collect()
}

/// Okapi BM25 of a term `frequency` times in a document of `length` among `n` of `average` length, `df` holding it,
/// with k1 1.2 and b 0.75; a field's weight multiplies its terms and its length
fn bm25(frequency: f64, df: f64, n: f64, length: f64, average: f64) -> f64 {
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    idf * frequency * 2.2 / (frequency + 1.2 * (0.25 + 0.75 * length / average))
}

fn assert_scores(got: Vec<(String, f64)>, want: &[(&str, f64)]) {
    assert_eq!(got.len(), want.len(), "{got:?}");
    for ((id, score), (want_id, want_score)) in got.iter().zip(want) {
        assert_eq!(id, want_id, "{got:?}");
        assert!((score - want_score).abs() < 1e-9, "{id}: {score}, not {want_score}");
    }
}

#[test]
fn a_score_is_bm25_over_the_weighted_fields_searched() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            create("Doc", "Doc-a", json!({ "title": "alpha", "body": "alpha beta" })),
            create("Doc", "Doc-b", json!({ "title": "beta", "body": "gamma delta epsilon" })),
        ],
    );
    // Weighted lengths: Doc-a 3 + 2, Doc-b 3 + 3, an average of 5.5
    let exact = json!({ "fuzzy": false, "prefix": false });
    let alpha = bm25(4.0, 1.0, 2.0, 5.0, 5.5);
    assert_scores(scores(&db, "Doc", "alpha", exact.clone()), &[("Doc-a", alpha)]);
    assert_scores(
        scores(&db, "Doc", "beta", exact.clone()),
        &[("Doc-b", bm25(3.0, 2.0, 2.0, 6.0, 5.5)), ("Doc-a", bm25(1.0, 2.0, 2.0, 5.0, 5.5))],
    );
    // Searching the body alone, the body's lengths: 2 and 3
    let body = json!({ "fuzzy": false, "prefix": false, "fields": ["body"] });
    assert_scores(scores(&db, "Doc", "alpha", body), &[("Doc-a", bm25(1.0, 1.0, 2.0, 2.0, 2.5))]);
    // A clause that doesn't match adds nothing, though its terms are held: "beta alpha" isn't a phrase of Doc-a's
    let mut any = exact.clone();
    any["any"] = json!(true);
    assert_scores(scores(&db, "Doc", "\"beta alpha\" alpha", any), &[("Doc-a", alpha)]);
    // A phrase that does is scored as its words are, each wherever the entity holds it
    let phrase = alpha + bm25(1.0, 2.0, 2.0, 5.0, 5.5);
    assert_scores(scores(&db, "Doc", "\"alpha beta\"", exact), &[("Doc-a", phrase)]);
}

#[test]
fn recency_multiplies_a_score_by_a_half_for_each_half_life() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![create("Message", "Message-1", json!({ "text": "deploy", "at": 80_000 }))]);
    let [(_, plain)] = scores(&db, "Message", "deploy", json!({}))[..] else { panic!() };
    let aged = |half_life: f64| {
        let recency = json!({ "recency": { "field": "at", "halfLife": half_life, "now": 100_000 } });
        scores(&db, "Message", "deploy", recency)[0].1
    };
    assert!((aged(10_000.0) - plain / 4.0).abs() < 1e-12);
    assert!((aged(40_000.0) - plain / 2.0_f64.sqrt()).abs() < 1e-12);
    assert_eq!(aged(0.0), plain, "a half-life of 0 ages nothing");
}

#[test]
fn a_highlight_joins_the_terms_it_runs_across() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![create("Message", "Message-1", json!({ "text": "openDatabaseStore fails" }))]);
    let answer = read(
        &db,
        json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Message"] },
        "steps": [{ "step": "search", "query": "database store fails", "options": { "any": true } }],
        "terminal": { "answer": "hits", "highlights": true }
    } }),
    )
    .unwrap();
    // "Database" and "Store" touch, and mark as one
    assert_eq!(answer["hits"][0]["highlights"]["text"], json!([[0, 4, 17], [0, 18, 23]]));
}

#[test]
fn the_word_as_typed_outranks_a_rarer_word_an_edit_away_and_a_negation_takes_no_edit() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            create("Message", "Message-1", json!({ "text": "master plan" })),
            create("Message", "Message-2", json!({ "text": "master copy" })),
            create("Message", "Message-3", json!({ "text": "muster roll" })),
        ],
    );
    // "muster" is an edit from "master" and the rarer of the two. Each match is scored as a holder of the word asked
    // for, which all three are: one term of a text of two, the one an edit away at six tenths
    let held_by_all = bm25(1.0, 3.0, 3.0, 2.0, 2.0);
    assert_scores(
        scores(&db, "Message", "master", json!({})),
        &[("Message-1", held_by_all), ("Message-2", held_by_all), ("Message-3", 0.6 * held_by_all)],
    );
    // Matched in no other form, a word is as rare as its one term
    let exact = json!({ "fuzzy": false });
    let held_by_two = bm25(1.0, 2.0, 3.0, 2.0, 2.0);
    assert_scores(scores(&db, "Message", "master", exact), &[("Message-1", held_by_two), ("Message-2", held_by_two)]);
    // A word started is scored the same way: "ma" starts only "master", "m" starts "muster" too
    let starts = json!({ "prefix": true, "fuzzy": false });
    assert_scores(
        scores(&db, "Message", "m", starts),
        &[("Message-1", 0.8 * held_by_all), ("Message-2", 0.8 * held_by_all), ("Message-3", 0.8 * held_by_all)],
    );

    // A negation leaves out the word as typed, and nothing an edit from it
    assert_eq!(search(&db, "Message", "roll -master", json!({})), json!(["Message-3"]));
    assert_eq!(search(&db, "Message", "master -muster", json!({})), json!(["Message-1", "Message-2"]));
    assert_eq!(search(&db, "Message", "roll -muster", json!({})), json!([]));
}

#[test]
fn a_word_longer_than_any_term_by_more_than_two_takes_no_edits() {
    use super::MAX_TERM_BYTES;
    use super::search::edits;
    // A searched word: none through four characters, one through eight; a word being typed, through two and six
    assert_eq!([4, 5, 8, 9].map(|length| edits(length, 4, 8)), [0, 1, 1, 2]);
    assert_eq!([2, 3, 6, 7].map(|length| edits(length, 2, 6)), [0, 1, 1, 2]);
    // Two characters more than the longest term is two edits from it; three more is within two of none, so its
    // automaton, which grows with the word, is never made
    assert_eq!(edits(MAX_TERM_BYTES + 2, 4, 8), 2);
    assert_eq!(edits(MAX_TERM_BYTES + 3, 4, 8), 0);
    assert_eq!(edits(MAX_TERM_BYTES + 3, 2, 6), 0);

    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let longest = "b".repeat(MAX_TERM_BYTES);
    write(&db, vec![create("Message", "Message-1", json!({ "text": longest }))]);
    let two_more = format!("{longest}bb");
    assert_eq!(search(&db, "Message", &two_more, json!({})), json!(["Message-1"]), "two edits away, and found");
    assert_eq!(search(&db, "Message", &format!("{two_more}b"), json!({})), json!([]));
}

/// A plan over Message from its type: its answer, and how many entities it read
fn visited(db: &Database, scope: &str, steps: Json, terminal: Json) -> (Json, u64) {
    let plan = json!({
        "scope": scope, "start": { "from": "types", "types": ["Message"] }, "steps": steps, "terminal": terminal
    });
    let answer = read(db, json!({ "read": "query", "visits": true, "plan": plan })).unwrap();
    (answer["answer"].clone(), answer["visited"].as_u64().unwrap())
}

/// How many keys the database holds under a prefix
fn keys_under(db: &Database, prefix: crate::format::Prefix) -> usize {
    db.snapshot(|s| {
        use crate::engine::unit::Read as _;
        let mut n = 0;
        s.visit(&prefix.key().to_vec(), |_, _| {
            n += 1;
            true
        })?;
        Ok(n)
    })
    .unwrap()
}

#[test]
fn a_term_more_entities_hold_than_its_key_keeps_is_filed_in_blocks_and_derived_as_it_was_filed() {
    use crate::format::Prefix;
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    // 700 messages hold "common"; one of them holds "zebra" twice, in a text of four words where the others' have two
    let text = |i: usize| if i == 650 { format!("zebra zebra common w{i}") } else { format!("common w{i}") };
    let id = |i: usize| format!("Message-{i:03}");
    write(&db, (0..700).map(|i| create("Message", &id(i), json!({ "text": text(i) }))).collect());
    assert!(keys_under(&db, Prefix::TextBlock) >= 3, "700 places are more than two blocks hold");
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    let exact = json!({ "fuzzy": false });
    let holding = |word: &str| {
        let mut ids: Vec<String> = search(&db, "Message", word, exact.clone())
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(holding("common"), (0..700).map(id).collect::<Vec<_>>());
    // How many hold a term in blocks is read from its blocks: the most held of the words "co" starts
    write(&db, vec![create("Message", "Message-cold", json!({ "text": "cold" }))]);
    let answer = read(&db, json!({ "read": "suggest", "type": "Message", "term": "co" })).unwrap();
    assert_eq!(answer["completions"], json!(["common", "cold"]));
    write(&db, vec![json!({ "op": "destroy", "id": "Message-cold" })]);
    // A match's lengths are read from the block of places it is in, here not the first
    let average = (699.0 * 2.0 + 4.0) / 700.0;
    let twice = id(650);
    assert_scores(
        scores(&db, "Message", "zebra", exact.clone()),
        &[(twice.as_str(), bm25(2.0, 1.0, 700.0, 4.0, average))],
    );

    // Each of a word's matches reads its own lengths, whichever block of places the match before it was in
    let every: std::collections::HashMap<String, f64> =
        scores(&db, "Message", "common", exact.clone()).into_iter().collect();
    assert_eq!(every.len(), 700);
    for (i, length) in [(0, 2.0), (511, 2.0), (650, 4.0), (699, 2.0)] {
        let expected = bm25(1.0, 700.0, 700.0, length, average);
        assert!((every[&id(i)] - expected).abs() < 1e-9, "{}: {}, not {expected}", id(i), every[&id(i)]);
    }

    // Entities filed under other terms and destroyed, inside blocks and at their ends
    let mut ops: Vec<Json> = (0..700)
        .step_by(3)
        .map(|i| json!({ "op": "set", "id": id(i), "fields": { "text": format!("other w{i}") } }))
        .collect();
    ops.extend((1..700).step_by(7).map(|i| json!({ "op": "destroy", "id": id(i) })));
    write(&db, ops);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    let kept: Vec<usize> = (0..700).filter(|i| i % 3 != 0 && i % 7 != 1).collect();
    assert_eq!(kept.len(), 399);
    let ids_kept: Vec<String> = kept.iter().map(|i| id(*i)).collect();
    assert_eq!(holding("common"), ids_kept);
    // More matches than the trash is asked for one by one: it is read whole, and what it holds is left out
    let trashed = [id(kept[5]), id(kept[300])];
    write(&db, trashed.iter().map(|id| json!({ "op": "trash", "id": id })).collect());
    let live: Vec<String> = ids_kept.iter().filter(|held| !trashed.contains(held)).cloned().collect();
    assert_eq!(holding("common"), live);
    write(&db, trashed.iter().map(|id| json!({ "op": "restore", "id": id })).collect());
    assert_eq!(holding("common"), ids_kept);
    // A rebuild of the index, and of everything, derives the keys the writes left
    db.rebuild(Some(("Message", "text"))).unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    assert_eq!(holding("common"), ids_kept);
    assert!(keys_under(&db, Prefix::TextBlock) >= 2);

    // Few enough hold it for its own key again: no term is in blocks
    let fewer: Vec<Json> =
        kept[..199].iter().map(|i| json!({ "op": "delete", "id": id(*i), "field": "text" })).collect();
    write(&db, fewer);
    assert_eq!(keys_under(&db, Prefix::TextBlock), 0);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    assert_eq!(holding("common"), ids_kept[199..]);
}

#[test]
fn an_entity_far_from_the_next_of_its_type_keeps_its_lengths_in_few_bytes_and_is_scored_by_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    // Places are every type's in turn: 300 entities of another type are between the two messages
    write(&db, vec![create("Message", "Message-a", json!({ "text": "zebra crossing ahead" }))]);
    write(&db, (0..300).map(|i| create("Plain", &format!("Plain-{i}"), json!({ "title": "t" }))).collect());
    write(&db, vec![create("Message", "Message-b", json!({ "text": "zebra" }))]);
    let lengths = || {
        db.snapshot(|s| {
            use crate::engine::unit::Read as _;
            let blocks = s.scan(&crate::engine::keys::text_index_ranges("Message", "text")[3])?;
            Ok(blocks.iter().map(|(_, block)| block.len()).sum::<usize>())
        })
        .unwrap()
    };
    // In one block or two, each message's two lengths with a bit a place at most: every place's would be 600 bytes
    assert!(lengths() <= 2 * (1 + 64 + 2), "{}", lengths());
    let exact = json!({ "fuzzy": false });
    let expected = [("Message-b", bm25(1.0, 2.0, 2.0, 1.0, 2.0)), ("Message-a", bm25(1.0, 2.0, 2.0, 3.0, 2.0))];
    assert_scores(scores(&db, "Message", "zebra", exact.clone()), &expected);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    let before = lengths();
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    assert_eq!(lengths(), before);
    assert_scores(scores(&db, "Message", "zebra", exact), &expected);
}

#[test]
fn a_limit_after_a_search_keeps_its_best_matches_and_reads_only_the_entities_it_hands_on() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    // Each holds "lmdb" once, in a text some words longer: the shorter the text, the better the match. Three texts
    // are the word alone, which tie in the order they were made
    let id = |i: usize| format!("Message-{i:02}");
    let longer = |i: usize| if i >= 30 { 0 } else { (i * 7) % 30 };
    write(
        &db,
        (0..32)
            .map(|i| create("Message", &id(i), json!({ "text": format!("lmdb{}", " filler".repeat(longer(i))) })))
            .collect(),
    );
    let mut best: Vec<usize> = (0..32).collect();
    best.sort_by_key(|i| (longer(*i), *i));
    assert_eq!(best[..4], [0, 30, 31, 13]);
    let ids = |of: &[usize]| json!(of.iter().map(|i| id(*i)).collect::<Vec<_>>());
    let lmdb = json!({ "step": "search", "query": "lmdb", "options": { "fuzzy": false } });
    let answer = json!({ "answer": "ids" });

    let (all, read_for_all) = visited(&db, "live", json!([lmdb]), answer.clone());
    assert_eq!((all["ids"].clone(), read_for_all), (ids(&best), 32));
    for n in [1, 2, 5, 31, 32, 40] {
        let (first, entities) = visited(&db, "live", json!([lmdb, { "step": "limit", "n": n }]), answer.clone());
        let kept = &best[..n.min(32)];
        assert_eq!((first["ids"].clone(), entities), (ids(kept), kept.len() as u64), "the best {n}");
    }
    // A limit further on stops the walk as it stops any, without the search keeping only that many
    let late = json!([lmdb, { "step": "has", "field": "text" }, { "step": "limit", "n": 3 }]);
    let (first, entities) = visited(&db, "live", late, answer.clone());
    assert_eq!((first["ids"].clone(), entities), (ids(&best[..3]), 3));
    let (first, entities) = visited(&db, "live", json!([lmdb]), json!({ "answer": "first" }));
    assert_eq!((first["id"].clone(), entities), (json!(id(0)), 1));

    // The trash is left out before any match is kept, so a limit still hands on as many as it takes
    write(&db, vec![json!({ "op": "trash", "id": id(0) }), json!({ "op": "trash", "id": id(13) })]);
    let live: Vec<usize> = best.iter().copied().filter(|i| ![0, 13].contains(i)).collect();
    let three = json!([lmdb, { "step": "limit", "n": 3 }]);
    let (first, entities) = visited(&db, "live", three.clone(), answer.clone());
    assert_eq!((first["ids"].clone(), entities), (ids(&live[..3]), 3));
    let (first, entities) = visited(&db, "trashed", three.clone(), answer.clone());
    assert_eq!((first["ids"].clone(), entities), (ids(&[0, 13]), 2));
    let (first, entities) = visited(&db, "withTrashed", three, answer.clone());
    assert_eq!((first["ids"].clone(), entities), (ids(&best[..3]), 3));

    // A count reads the matches as the index holds them, and no entity
    let count = json!({ "answer": "count" });
    for (scope, matches) in [("live", 30), ("trashed", 2), ("withTrashed", 32)] {
        let (counted, entities) = visited(&db, scope, json!([lmdb]), count.clone());
        assert_eq!((counted["n"].clone(), entities), (json!(matches), 0), "{scope}");
    }
    // After a step past the search it is the walk's again, which reads what it counts
    let (counted, entities) = visited(&db, "live", json!([lmdb, { "step": "has", "field": "text" }]), count);
    assert_eq!((counted["n"].clone(), entities), (json!(30), 30));
}

#[test]
fn hits_after_a_limit_are_the_best_matches_with_their_scores_and_no_others() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    let hits = |steps: Json| {
        let plan = json!({
            "start": { "from": "types", "types": ["Message"] }, "steps": steps, "terminal": { "answer": "hits" }
        });
        let answer = read(&db, json!({ "read": "query", "plan": plan })).unwrap();
        let hits = answer["hits"].as_array().unwrap().clone();
        hits.iter()
            .map(|h| (h["id"].as_str().unwrap().to_owned(), h["score"].as_f64().unwrap(), h["terms"].clone()))
            .collect::<Vec<_>>()
    };
    let either = json!({ "step": "search", "query": "database lmdb", "options": { "any": true, "fuzzy": false } });
    let all = hits(json!([either]));
    assert_eq!(all.iter().map(|(id, _, _)| id.as_str()).collect::<Vec<_>>(), ["Message-1", "Message-3", "Message-2"]);
    assert!(all[0].1 > all[1].1 && all[1].1 > all[2].1, "{all:?}");
    assert_eq!(all[0].2, json!(["database", "lmdb"]));
    assert_eq!(hits(json!([either, { "step": "limit", "n": 2 }])), all[..2]);
    // A hit is of an entity the search handed on: a match the limit left out, reached again by a step after it, is
    // answered as an entity the search never scored
    write(&db, vec![json!({ "op": "link", "source": "Message-1", "kind": "in", "target": "Message-3" })]);
    let reached = hits(json!([either, { "step": "limit", "n": 1 }, { "step": "out", "kinds": ["in"] }]));
    assert_eq!(reached, [("Message-3".to_owned(), 0.0, json!([]))]);
}

#[test]
fn a_hit_names_the_terms_and_fields_its_clauses_matched_it_in() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![create("Doc", "Doc-a", json!({ "title": "alpha gamma", "body": "alpha beta" }))]);
    let marked = |query: &str, options: Json| {
        let plan = json!({
            "start": { "from": "types", "types": ["Doc"] },
            "steps": [{ "step": "search", "query": query, "index": "text", "options": options }],
            "terminal": { "answer": "hits", "highlights": true }
        });
        let answer = read(&db, json!({ "read": "query", "plan": plan })).unwrap();
        (answer["hits"][0]["terms"].clone(), answer["hits"][0]["highlights"].clone())
    };
    let exact = json!({ "fuzzy": false });
    // Held in both fields, and marked in both
    assert_eq!(
        marked("alpha", exact.clone()),
        (json!(["alpha"]), json!({ "title": [[0, 0, 5]], "body": [[0, 0, 5]] }))
    );
    let both = json!({ "fuzzy": false, "fields": ["title", "body"] });
    assert_eq!(marked("alpha", both), (json!(["alpha"]), json!({ "title": [[0, 0, 5]], "body": [[0, 0, 5]] })));
    // A clause held to one field marks it there alone, and a search of some fields marks none of the others
    assert_eq!(marked("title:alpha", exact.clone()), (json!(["alpha"]), json!({ "title": [[0, 0, 5]] })));
    let body = json!({ "fuzzy": false, "fields": ["body"] });
    assert_eq!(marked("alpha", body), (json!(["alpha"]), json!({ "body": [[0, 0, 5]] })));
    // Each clause's terms, where each matched: "beta" only in the body, and "gamma" asked for only in the title
    assert_eq!(
        marked("beta title:gamma", exact.clone()),
        (json!(["beta", "gamma"]), json!({ "title": [[0, 6, 11]], "body": [[0, 6, 10]] }))
    );
    // A term of a clause the entity doesn't match is none of its hit's: "alpha gamma" is no phrase of its body
    let mut any = exact;
    any["any"] = json!(true);
    assert_eq!(marked("beta body:\"alpha gamma\"", any), (json!(["beta"]), json!({ "body": [[0, 6, 10]] })));
}

#[test]
fn a_stemming_index_counts_the_entities_holding_each_word_as_written() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let completions = || {
        read(&db, json!({ "read": "suggest", "type": "Doc", "index": "stemmed", "term": "run" })).unwrap()["completions"]
            .clone()
    };
    write(
        &db,
        vec![
            create("Doc", "Doc-1", json!({ "body": "running running" })),
            create("Doc", "Doc-2", json!({ "body": "runner" })),
            create("Doc", "Doc-3", json!({ "body": "runner up" })),
        ],
    );
    // Most held first: an entity counts once however often it holds the word
    assert_eq!(completions(), json!(["runner", "running"]));
    write(
        &db,
        vec![
            json!({ "op": "destroy", "id": "Doc-2" }),
            json!({ "op": "set", "id": "Doc-3", "fields": { "body": "running late" } }),
        ],
    );
    assert_eq!(completions(), json!(["running"]));
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    write(&db, vec![create("Doc", "Doc-4", json!({ "body": "runner" }))]);
    assert_eq!(completions(), json!(["running", "runner"]));
    db.rebuild(Some(("Doc", "stemmed"))).unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    assert_eq!(completions(), json!(["running", "runner"]));
}

#[test]
fn a_term_an_entity_holds_more_or_less_often_than_it_did_is_filed_again() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    corpus(&db);
    let exact = json!({ "fuzzy": false });
    // Message-3 holds "lmdb" three times among six words, then once among two, then twice among three
    let score = |frequency: f64, length: f64, total: f64| bm25(frequency, 2.0, 4.0, length, total / 4.0);
    let of_third = || scores(&db, "Message", "lmdb", exact.clone()).into_iter().find(|(id, _)| id == "Message-3");
    let held = |expected: f64| {
        let (_, found) = of_third().unwrap();
        assert!((found - expected).abs() < 1e-9, "{found}, not {expected}");
        assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    };
    held(score(3.0, 6.0, 18.0));
    write(&db, vec![json!({ "op": "set", "id": "Message-3", "fields": { "text": "lmdb notes" } })]);
    held(score(1.0, 2.0, 14.0));
    write(&db, vec![json!({ "op": "set", "id": "Message-3", "fields": { "text": "lmdb about lmdb" } })]);
    held(score(2.0, 3.0, 15.0));
}
