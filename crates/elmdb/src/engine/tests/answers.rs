//! Every kind of answer a read can give, each decoded into the type that declares it (`answer.rs`): the definition the
//! binding's `protocol/Answer.ts` is generated from. `answer::conforms` holds a read's bytes to that type field for
//! field, so a key the engine writes that the type lacks, or one the type declares that the engine doesn't write, fails
//! here.

use std::collections::BTreeSet;

use serde_json::{Value as Json, json};

use super::write;
use crate::engine::answer::{self, Answer, Components, Groups, HitsRank};
use crate::engine::plan::Reads;
use crate::{Database, OpenOptions};

/// The kind each answer names on the wire, matched without a wildcard: a kind added to `Answer` fails to compile here
/// until it is named, and then fails `every_kind_of_answer_decodes_into_its_type` until a read answers it
fn kind(answer: &Answer) -> &'static str {
    match answer {
        Answer::Ids { .. } => "ids",
        Answer::Count { .. } => "count",
        Answer::Bool { .. } => "bool",
        Answer::Id { .. } => "id",
        Answer::Rows { .. } => "rows",
        Answer::Row { .. } => "row",
        Answer::Field { .. } => "field",
        Answer::Relation { .. } => "relation",
        Answer::Relations { .. } => "relations",
        Answer::Degrees { .. } => "degrees",
        Answer::WeakComponents { .. } => "weakComponents",
        Answer::StrongComponents { .. } => "strongComponents",
        Answer::Communities { .. } => "communities",
        Answer::Least { .. } => "least",
        Answer::Greatest { .. } => "greatest",
        Answer::Sum { .. } => "sum",
        Answer::Groups { .. } => "groups",
        Answer::Hits { .. } => "hits",
        Answer::Labels { .. } => "labels",
        Answer::Suggestions { .. } => "suggestions",
        Answer::Pending { .. } => "pending",
        Answer::Catalog { .. } => "catalog",
        Answer::Visited { .. } => "visited",
        Answer::Explained { .. } => "explained",
    }
}

/// Every kind `kind` names
const KINDS: [&str; 24] = [
    "ids",
    "count",
    "bool",
    "id",
    "rows",
    "row",
    "field",
    "relation",
    "relations",
    "degrees",
    "weakComponents",
    "strongComponents",
    "communities",
    "least",
    "greatest",
    "sum",
    "groups",
    "hits",
    "labels",
    "suggestions",
    "pending",
    "catalog",
    "visited",
    "explained",
];

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let schema = crate::schema::Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "roles": { "pinned": {} },
        "relations": {
            "refs": { "cardinality": "many-to-many" },
            "weighs": { "cardinality": "many-to-many", "weighted": true }
        },
        "types": {
            "Note": {
                "fields": {
                    "title": { "type": "string", "optional": true },
                    "n": { "type": "number", "optional": true },
                    "tags": { "type": "any", "optional": true }
                },
                "indexes": {
                    "text": { "kind": "text", "fields": { "title": {} } },
                    "n": { "kind": "value", "fields": ["n"] }
                }
            },
            "Doc": {
                "fields": { "text": { "type": "string", "optional": true } },
                "indexes": { "meaning": { "kind": "vector", "from": ["text"], "dims": 2, "model": "m" } }
            }
        },
        "ranks": { "noteRank": { "types": ["Note"], "relations": ["refs"] } }
    }))
    .unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap();
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "title": "alpha beta", "n": 1 } }),
            json!({ "op": "create", "type": "Note", "id": "Note-2", "fields": { "title": "alpha", "n": 2 } }),
            json!({ "op": "create", "type": "Note", "id": "Note-3", "fields": { "title": "gamma", "n": 2 } }),
            json!({ "op": "create", "type": "Doc", "id": "Doc-1", "fields": { "text": "hello" } }),
            json!({ "op": "link", "source": "Note-1", "kind": "refs", "target": "Note-2" }),
            json!({ "op": "link", "source": "Note-2", "kind": "refs", "target": "Note-3" }),
            json!({ "op": "link", "source": "Note-1", "kind": "refs", "target": "Note-3", "info": { "why": "see" } }),
            json!({ "op": "link", "source": "Note-1", "kind": "weighs", "target": "Note-3", "weight": 2.5 }),
        ],
    )
    .unwrap();
    db.wait_for_index_builds().unwrap();
    (dir, db)
}

/// One read's answer, held to its type
fn read(db: &Database, read: Json) -> Answer {
    let reads: Reads = serde_json::from_value(json!({ "reads": [read] })).unwrap();
    let bytes = db.read(&reads).unwrap();
    let mut answers = answer::conforms(&bytes).unwrap_or_else(|e| panic!("{read}: {e}")).0;
    assert_eq!(answers.len(), 1);
    answers.pop().unwrap()
}

/// A query of the notes, with these steps and this terminal
fn query(steps: Json, terminal: Json) -> Json {
    json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Note"] }, "steps": steps, "terminal": terminal
    } })
}

#[test]
fn every_kind_of_answer_decodes_into_its_type() {
    let (_dir, db) = open();
    let search = json!([{ "step": "search", "query": "alpha" }]);
    let reads = [
        query(json!([]), json!({ "answer": "ids" })),
        query(json!([]), json!({ "answer": "count" })),
        query(json!([]), json!({ "answer": "first" })),
        query(json!([]), json!({ "answer": "last" })),
        query(json!([{ "step": "where", "field": "n", "value": 9 }]), json!({ "answer": "first" })),
        query(json!([]), json!({ "answer": "exists" })),
        query(json!([]), json!({ "answer": "firstRow" })),
        query(json!([{ "step": "where", "field": "n", "value": 9 }]), json!({ "answer": "firstRow" })),
        query(json!([]), json!({ "answer": "degrees", "kinds": ["refs"] })),
        query(json!([]), json!({ "answer": "weakComponents", "kinds": ["refs"] })),
        query(json!([]), json!({ "answer": "weakComponents", "kinds": ["refs"], "counts": true })),
        query(json!([]), json!({ "answer": "strongComponents", "kinds": ["refs"] })),
        query(json!([]), json!({ "answer": "strongComponents", "kinds": ["refs"], "counts": true })),
        query(json!([]), json!({ "answer": "communities", "kinds": ["refs"] })),
        query(json!([]), json!({ "answer": "groupBy", "field": "n" })),
        query(json!([]), json!({ "answer": "groupBy", "field": "n", "counts": true })),
        query(json!([]), json!({ "answer": "least", "field": "n" })),
        query(json!([]), json!({ "answer": "least", "field": "tags" })),
        query(json!([]), json!({ "answer": "greatest", "field": "title" })),
        query(json!([]), json!({ "answer": "sum", "field": "n" })),
        query(search.clone(), json!({ "answer": "hits" })),
        query(json!([{ "step": "rank", "kinds": ["refs"] }]), json!({ "answer": "hits" })),
        query(json!([{ "step": "ranked", "index": "noteRank" }]), json!({ "answer": "hits" })),
        query(
            json!([{ "step": "similar", "to": "Note-3", "kinds": ["refs"], "direction": "both" }]),
            json!({ "answer": "hits" }),
        ),
        json!({ "read": "row", "id": "Note-1" }),
        json!({ "read": "row", "id": "Note-9" }),
        json!({ "read": "field", "id": "Note-1", "field": "title" }),
        json!({ "read": "field", "id": "Note-1", "field": "tags" }),
        json!({ "read": "relation", "id": "Relation-9" }),
        json!({ "read": "findRelations", "source": "Note-1" }),
        json!({ "read": "suggest", "type": "Note", "term": "alp" }),
        json!({ "read": "findLabels", "query": "al", "fields": { "Note": "title" } }),
        json!({ "read": "catalog" }),
        json!({ "read": "wouldCreateCycle", "source": "Note-3", "target": "Note-1", "kinds": ["refs"] }),
        json!({ "read": "query", "visits": true, "plan": {
            "start": { "from": "types", "types": ["Note"] }, "terminal": { "answer": "ids" } } }),
        json!({ "read": "query", "explain": true, "plan": {
            "start": { "from": "types", "types": ["Note"] }, "steps": search.clone(), "terminal": { "answer": "hits" } } }),
    ];
    let mut answers: Vec<Answer> = reads.into_iter().map(|r| read(&db, r)).collect();
    // The reads whose answers are checked past their round trip, each made by name
    let edges =
        read(&db, query(json!([]), json!({ "answer": "edges", "kinds": ["refs", "weighs"], "direction": "both" })));
    let whole = read(&db, query(json!([]), json!({ "answer": "rows" })));
    let picked = read(
        &db,
        query(json!([]), json!({ "answer": "rows", "fields": ["title", "entityType", "unheld", "title", "id"] })),
    );
    let marked = read(&db, query(search, json!({ "answer": "hits", "fields": ["title"], "highlights": true })));
    let pending = read(&db, json!({ "read": "pendingEmbeddings", "type": "Doc" }));
    answers.extend([&edges, &whole, &picked, &marked, &pending].map(Answer::clone));
    // A relation read of one that exists, by the id the walk answered
    let Answer::Relations { relations, .. } = &edges else { panic!("edges answered {edges:?}") };
    let found = read(&db, json!({ "read": "relation", "id": relations[0].id }));
    assert!(matches!(&found, Answer::Relation { relation: Some(row), .. } if *row == relations[0]), "{found:?}");
    answers.push(found);
    let seen: BTreeSet<&str> = answers.iter().map(kind).collect();

    let missing: Vec<&str> = KINDS.iter().copied().filter(|k| !seen.contains(k)).collect();
    assert!(missing.is_empty(), "no read here answers {missing:?}");
    assert_eq!(seen.len(), KINDS.len(), "a kind is read here that `KINDS` doesn't list: {seen:?}");

    // The answers each optional part of the types is read in, so the round trip covers it
    let weighted = relations.iter().filter(|r| r.weight.is_some()).count();
    assert!(weighted > 0 && weighted < relations.len(), "a weighted kind's row and another's: {relations:?}");
    assert!(relations.iter().any(|r| !r.info.is_nil()), "a relation carrying info");
    let shapes = |pick: fn(&Answer) -> bool| answers.iter().filter(|a| pick(a)).count();
    assert_eq!(shapes(|a| matches!(a, Answer::WeakComponents { weak_components: Components::Sizes(_) })), 1);
    assert_eq!(shapes(|a| matches!(a, Answer::StrongComponents { strong_components: Components::Sizes(_) })), 1);
    assert_eq!(shapes(|a| matches!(a, Answer::Groups { groups: Groups::Counts(_), .. })), 1);
    assert_eq!(shapes(|a| matches!(a, Answer::Groups { groups: Groups::Members(_), .. })), 1);
    assert_eq!(shapes(|a| matches!(a, Answer::Hits { rank: Some(HitsRank::Rank { .. }), .. })), 1);
    assert_eq!(shapes(|a| matches!(a, Answer::Hits { rank: Some(HitsRank::Ranked { .. }), .. })), 1);
    let Answer::Hits { hits, .. } = &marked else { panic!("hits answered {marked:?}") };
    assert!(hits.iter().all(|h| h.highlights.is_some() && h.row.is_some()), "{hits:?}");
    let Answer::Rows { rows, .. } = &whole else { panic!("rows answered {whole:?}") };
    assert!(rows.iter().all(|r| r.fields.contains_key("entityType") && r.fields.contains_key("createdAt")));
    // A field asked for twice is answered once: written twice, the row's map would hold the key twice, which
    // `conforms` tells in `read`. A field the entity lacks isn't answered, and `id` only as the row's id
    let Answer::Rows { rows, .. } = &picked else { panic!("rows answered {picked:?}") };
    assert_eq!(rows[0].id, "Note-1");
    assert_eq!(rows[0].fields.keys().collect::<Vec<_>>(), ["entityType", "title"]);
    let Answer::Pending { chunks, .. } = &pending else { panic!("pending answered {pending:?}") };
    assert_eq!(chunks.len(), 1);
}

/// Whether an answer says no map in its stored values may hold a key `__proto__`, for each kind that holds them
fn plain(answer: &Answer) -> bool {
    match answer {
        Answer::Rows { plain_objects, .. }
        | Answer::Row { plain_objects, .. }
        | Answer::Field { plain_objects, .. }
        | Answer::Relation { plain_objects, .. }
        | Answer::Relations { plain_objects, .. }
        | Answer::Least { plain_objects, .. }
        | Answer::Greatest { plain_objects, .. }
        | Answer::Groups { plain_objects, .. }
        | Answer::Hits { plain_objects, .. } => *plain_objects,
        other => panic!("{other:?} holds no stored value"),
    }
}

#[test]
fn every_answer_holding_a_stored_value_says_whether_a_map_in_it_may_hold_proto() {
    let (_dir, db) = open();
    let reads = |relation: &str| {
        [
            json!({ "read": "field", "id": "Note-1", "field": "tags" }),
            json!({ "read": "relation", "id": relation }),
            json!({ "read": "findRelations", "source": "Note-1", "kind": "refs" }),
            query(json!([]), json!({ "answer": "edges", "kinds": ["refs"] })),
            query(json!([]), json!({ "answer": "least", "field": "tags" })),
            query(json!([]), json!({ "answer": "greatest", "field": "tags" })),
            query(json!([]), json!({ "answer": "rows" })),
            query(json!([]), json!({ "answer": "groupBy", "field": "tags" })),
        ]
        .map(|r| read(&db, r))
    };
    let Answer::Relations { relations, .. } = read(&db, json!({ "read": "findRelations", "kind": "refs" })) else {
        panic!("findRelations answered no relations")
    };
    let relation = &relations[0].id;
    assert!(reads(relation).iter().all(plain), "{:?}", reads(relation));
    // A stored map holding `__proto__`, as a field's value and as what a link carries
    let proto = json!({ "__proto__": 1 });
    write(
        &db,
        vec![
            json!({ "op": "set", "id": "Note-1", "fields": { "tags": proto } }),
            json!({ "op": "updateRelation", "relation": relation, "info": proto }),
        ],
    )
    .unwrap();
    let answers = reads(relation);
    assert!(answers.iter().all(|a| !plain(a)), "{answers:?}");
}

#[test]
fn a_key_the_types_do_not_declare_or_one_missing_is_told() {
    let encode = |value: Json| rmp_serde::to_vec_named(&value).unwrap();
    assert!(answer::conforms(&encode(json!([{ "kind": "count", "n": 3 }]))).is_ok());
    // An extra key decodes, serde passing over it, and the round trip tells it
    let extra = answer::conforms(&encode(json!([{ "kind": "count", "n": 3, "more": 1 }]))).unwrap_err();
    assert!(extra.contains("more"), "{extra}");
    // A missing `Option` decodes as `None`, and the round trip tells it
    assert!(answer::conforms(&encode(json!([{ "kind": "id" }]))).is_err());
    assert!(answer::conforms(&encode(json!([{ "kind": "id", "id": null }]))).is_ok());
    assert!(answer::conforms(&encode(json!([{ "kind": "nothing" }]))).is_err());
    assert!(
        answer::conforms(&encode(json!([{ "kind": "row", "row": { "title": "x" }, "plainObjects": true }]))).is_err()
    );
}
