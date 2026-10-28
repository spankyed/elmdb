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
                crate::format::Prefix::TextDocument,
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
