//! Substring and pattern search (`docs/architecture/search.md`, Substrings and patterns): on a fixed corpus of a chat's
//! kind — error text, ids and code — what `containing` and `matching` answer is what a scan of every message finds;
//! the index narrows what is read, follows every write, is built over rows written before it was declared, and is what
//! `check()` compares and `rebuild()` makes.

use serde_json::{Value as Json, json};

use super::write;
use crate::engine::keys;
use crate::engine::plan::{ReadOp, Reads};
use crate::engine::unit::Read;
use crate::error::ErrorCode;
use crate::schema::Schema;
use crate::{Database, OpenOptions};

fn schema(indexes: Json) -> Schema {
    Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "types": { "Message": {
            "fields": {
                "text": { "type": "string", "optional": true },
                "tags": { "type": "list", "of": { "type": "string" }, "optional": true },
                "n": { "type": "number", "optional": true }
            },
            "indexes": indexes
        } }
    }))
    .unwrap()
}

fn grams() -> Json {
    json!({ "grams": { "kind": "trigram", "fields": ["text", "tags"] } })
}

fn open(dir: &tempfile::TempDir, indexes: Json) -> Database {
    Database::open(dir.path(), OpenOptions { schema: Some(schema(indexes)), syncs: false, ..Default::default() })
        .unwrap()
}

/// A chat's messages: errors, ids, paths and code, in several cases, and tags as a list
fn corpus() -> Vec<(String, Json)> {
    let texts = [
        "Error: ETIMEDOUT connecting to 127.0.0.1:5432 after 30000ms",
        "error: timeout while reading packages/apack-ears/src/watch.ts",
        "TypeError: Cannot read properties of undefined (reading 'seq')",
        "fixed in a1b6603e5a, the scheduler clears a superseded timer",
        "fn need_of(hir: &Hir) -> Need { match hir.kind() { _ => Need::Any } }",
        "THR-1024 asks why THR-998 was closed",
        "the build failed: error[E0308]: mismatched types",
        "all good, nothing to report",
        "ünïcödé ERROR in ÜNÏCÖDÉ text",
        "",
    ];
    let mut out: Vec<(String, Json)> =
        texts.iter().enumerate().map(|(i, text)| (format!("Message-{i}"), json!({ "text": text, "n": i }))).collect();
    out.push(("Message-tags".into(), json!({ "tags": ["timeout", "needs-review", "E0308"] })));
    out.push(("Message-none".into(), json!({ "n": 42 })));
    out
}

fn load(db: &Database) {
    let ops = corpus()
        .into_iter()
        .map(|(id, fields)| json!({ "op": "create", "type": "Message", "id": id, "fields": fields }))
        .collect();
    write(db, ops).unwrap();
}

fn query(db: &Database, steps: Json, explain: bool) -> crate::Result<Json> {
    let op: ReadOp = serde_json::from_value(json!({
        "read": "query",
        "plan": { "start": { "from": "types", "types": ["Message"] }, "steps": steps, "terminal": { "answer": "ids" } },
        "explain": explain
    }))
    .unwrap();
    let bytes = db.read(&Reads { reads: vec![op], track: None })?;
    let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    Ok(answers[0].take())
}

fn ids(db: &Database, step: Json) -> Vec<String> {
    let answer = query(db, json!([step]), false).unwrap();
    answer["ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_owned()).collect()
}

/// How many entities the plan read
fn visited(db: &Database, step: Json) -> u64 {
    query(db, json!([step]), true).unwrap()["visited"].as_u64().unwrap()
}

/// What a scan of the corpus finds: the messages, in creation order, a text of whose holds a match
fn scanned(found: impl Fn(&str) -> bool) -> Vec<String> {
    corpus()
        .into_iter()
        .filter(|(_, fields)| {
            let texts: Vec<&str> = match (&fields["text"], &fields["tags"]) {
                (Json::String(s), _) => vec![s.as_str()],
                (_, Json::Array(items)) => items.iter().filter_map(Json::as_str).collect(),
                _ => vec![],
            };
            texts.into_iter().any(&found)
        })
        .map(|(id, _)| id)
        .collect()
}

const SUBSTRINGS: [&str; 10] =
    ["error", "Error", "ETIMEDOUT", "timeout", "THR-", "a1b6603", "127.0.0.1", "E0308", "ÜNÏ", "ab"];

const PATTERNS: [&str; 10] = [
    r"THR-\d+",
    r"(Type)?Error: ",
    r"(?i)error",
    r"^error",
    r"error\[E\d{4}\]",
    r"[a-f0-9]{10}",
    r"timeout|ETIMEDOUT",
    r"packages/[a-z-]+/src",
    r"fn \w+\(",
    r"",
];

fn assert_as_scanned(db: &Database) {
    for text in SUBSTRINGS {
        let exact = ids(db, json!({ "step": "containing", "text": text }));
        assert_eq!(exact, scanned(|s| s.contains(text)), "containing {text:?}");
        let folded = ids(db, json!({ "step": "containing", "text": text, "caseInsensitive": true }));
        assert_eq!(
            folded,
            scanned(|s| s.to_lowercase().contains(&text.to_lowercase())),
            "containing {text:?}, any case"
        );
    }
    for pattern in PATTERNS {
        let regex = regex::Regex::new(pattern).unwrap();
        let found = ids(db, json!({ "step": "matching", "pattern": pattern }));
        assert_eq!(found, scanned(|s| regex.is_match(s)), "matching {pattern:?}");
    }
}

#[test]
fn what_a_search_finds_is_what_a_scan_finds() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir, grams());
    load(&db);
    assert_as_scanned(&db);
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn the_index_narrows_what_is_read_and_a_pattern_it_cant_narrow_by_reads_every_message() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir, grams());
    load(&db);
    let all = corpus().len() as u64;
    // Read: the candidates holding each trigram, each then checked
    assert_eq!(visited(&db, json!({ "step": "containing", "text": "ETIMEDOUT" })), 1);
    assert_eq!(visited(&db, json!({ "step": "matching", "pattern": r"THR-\d+" })), 1);
    assert_eq!(visited(&db, json!({ "step": "matching", "pattern": "timeout|ETIMEDOUT" })), 3);
    // A text shorter than a trigram, a class, or an alternation with such a branch narrows by nothing
    assert_eq!(visited(&db, json!({ "step": "containing", "text": "ab" })), all);
    assert_eq!(visited(&db, json!({ "step": "matching", "pattern": "[a-f0-9]{10}" })), all);
    assert_eq!(visited(&db, json!({ "step": "matching", "pattern": "timeout|x" })), all);
}

#[test]
fn the_index_follows_every_write_and_check_and_rebuild_cover_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir, grams());
    load(&db);
    write(
        &db,
        vec![
            json!({ "op": "set", "id": "Message-7", "fields": { "text": "now it times out: ETIMEDOUT" } }),
            json!({ "op": "delete", "id": "Message-0", "field": "text" }),
            json!({ "op": "destroy", "id": "Message-5" }),
            json!({ "op": "append", "id": "Message-tags", "field": "tags", "value": "ETIMEDOUT-again" }),
            json!({ "op": "trash", "id": "Message-1" }),
        ],
    )
    .unwrap();
    assert_eq!(ids(&db, json!({ "step": "containing", "text": "ETIMEDOUT" })), ["Message-7", "Message-tags"]);
    assert_eq!(ids(&db, json!({ "step": "containing", "text": "THR-" })), Vec::<String>::new());
    assert_eq!(ids(&db, json!({ "step": "containing", "text": "timeout" })), ["Message-tags"], "the trash is left out");
    assert!(db.check().unwrap().is_clean());

    // A block missing a place and one nothing derives are what `check()` reports, and a rebuild of the index repairs
    // both
    let held = db.snapshot(|r| Read::scan(r, &keys::trigram_places("Message", "grams", "eti"))).unwrap();
    assert_eq!(held.len(), 1);
    let (key, block) = &held[0];
    let mut places = crate::engine::text::decode_postings(block).unwrap();
    assert_eq!(places.len(), 2);
    places.remove(places.min().unwrap());
    db.store()
        .unwrap()
        .write(
            |txn, d| {
                d.put(txn, key, &crate::engine::text::encode_postings(&places))?;
                let stray: roaring::RoaringTreemap = [3u64].into_iter().collect();
                d.put(
                    txn,
                    &keys::trigram("Message", "grams", "zzz", 0),
                    &crate::engine::text::encode_postings(&stray),
                )?;
                Ok(())
            },
            false,
        )
        .unwrap();
    let report = db.check().unwrap();
    let mut kinds: Vec<&str> = report.differences.iter().map(|d| d.kind.as_str()).collect();
    kinds.sort();
    assert_eq!(kinds, ["differs", "extra"], "{report:?}");
    db.rebuild(Some(("Message", "grams"))).unwrap();
    assert!(db.check().unwrap().is_clean());
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean());
    assert_eq!(ids(&db, json!({ "step": "containing", "text": "ETIMEDOUT" })), ["Message-7", "Message-tags"]);
}

#[test]
fn an_index_declared_after_the_rows_is_built_over_them_and_read_whole_until_then() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir, json!({}));
    load(&db);
    let refused = query(&db, json!([{ "step": "containing", "text": "error" }]), false).unwrap_err();
    assert_eq!(refused.code, ErrorCode::UnknownIndex, "{}", refused.message);
    db.set_schema(schema(grams())).unwrap();
    db.wait_for_index_builds().unwrap();
    assert_as_scanned(&db);
    assert!(db.check().unwrap().is_clean());
    // Dropped, its keys go with it
    db.set_schema(schema(json!({}))).unwrap();
    assert!(db.check().unwrap().is_clean());
    let left = db.snapshot(|r| Read::scan(r, &keys::trigram_index("Message", "grams"))).unwrap();
    assert!(left.is_empty(), "{} keys left", left.len());
}

#[test]
fn a_pattern_the_syntax_doesnt_hold_is_refused_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir, grams());
    load(&db);
    for pattern in ["error(?=:)", r"(\w)\1"] {
        let refused = query(&db, json!([{ "step": "matching", "pattern": pattern }]), false).unwrap_err();
        assert_eq!(refused.code, ErrorCode::PatternInvalid);
        assert!(refused.message.contains(pattern), "{}", refused.message);
    }
}

#[test]
fn among_a_set_a_type_with_several_trigram_indexes_and_none_named_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(
        &dir,
        json!({ "grams": { "kind": "trigram", "fields": ["text"] }, "tagGrams": { "kind": "trigram", "fields": ["tags"] } }),
    );
    load(&db);
    let among = |step: Json| {
        let op: ReadOp = serde_json::from_value(json!({ "read": "query", "plan": {
            "start": { "from": "ids", "ids": ["Message-1", "Message-tags"] }, "steps": [step], "terminal": { "answer": "ids" }
        } }))
        .unwrap();
        db.read(&Reads { reads: vec![op], track: None })
    };
    let refused = among(json!({ "step": "containing", "text": "timeout" })).unwrap_err();
    assert_eq!(refused.code, ErrorCode::UnknownIndex, "{}", refused.message);
    assert!(refused.message.contains("several"), "{}", refused.message);
    // Named, each index reads its own fields: Message-1's text says timeout, Message-tags' tags do
    let found = |index: &str| {
        let bytes = among(json!({ "step": "containing", "text": "timeout", "index": index })).unwrap();
        serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()[0]["ids"].clone()
    };
    assert_eq!(found("grams"), json!(["Message-1"]));
    assert_eq!(found("tagGrams"), json!(["Message-tags"]));
}
