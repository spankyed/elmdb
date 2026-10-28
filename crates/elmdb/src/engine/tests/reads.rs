//! Reads, case by case: ported from apack's `tests/engine/queries.spec.ts` and the walk cases of `relations.spec.ts`.
//! A predicate can't cross the binding, so apack's cases that pass one are its facade's to run, and those that count
//! what a predicate saw count the entities a plan visited instead.

use serde_json::{Value as Json, json};

use crate::engine::plan::{Plan, ReadOp, Reads};
use crate::engine::reads::run_counted;
use crate::error::ErrorCode;
use crate::schema::Schema;
use crate::{Database, OpenOptions};

fn schema() -> Schema {
    Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "roles": { "focus": {} },
        "relations": {
            "contains": { "cardinality": "many-to-many" },
            "owns": { "cardinality": "many-to-many" },
            "refers": { "cardinality": "many-to-many" },
            "parent_of": { "cardinality": "one-to-many", "acyclic": true }
        },
        "types": {
            "Project": { "fields": { "title": { "type": "string", "optional": true }, "tag": { "type": "any", "optional": true } } },
            "Task": {
                "fields": {
                    "title": { "type": "string", "optional": true },
                    "rank": { "type": "number", "optional": true },
                    "status": { "type": "string", "optional": true },
                    "tag": { "type": "any", "optional": true }
                },
                "indexes": { "status": { "kind": "value", "fields": ["status"] } }
            },
            "Person": { "additionalFields": "allow", "fields": {} }
        }
    }))
    .unwrap()
}

/// apack's fixture: a project containing two tasks and owning a third, four tasks in all, one focused
struct Fixture {
    _dir: tempfile::TempDir,
    db: Database,
}

const P: &str = "Project-p";
const A: &str = "Task-a";
const B: &str = "Task-b";
const C: &str = "Task-c";
const D: &str = "Task-d";

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema()), syncs: false, ..Default::default() }).unwrap();
    super::write(&db, vec![
        json!({ "op": "create", "type": "Project", "id": P }),
        json!({ "op": "create", "type": "Task", "id": A, "fields": { "title": "banana", "rank": 2, "status": "open" } }),
        json!({ "op": "create", "type": "Task", "id": B, "fields": { "title": "apple", "rank": 10, "status": "done" } }),
        json!({ "op": "create", "type": "Task", "id": C, "fields": { "title": "cherry", "status": "open" } }),
        json!({ "op": "create", "type": "Task", "id": D, "fields": { "title": "apple", "rank": 1, "status": "open" } }),
        json!({ "op": "link", "source": P, "kind": "contains", "target": A }),
        json!({ "op": "link", "source": P, "kind": "contains", "target": B }),
        json!({ "op": "link", "source": P, "kind": "owns", "target": C }),
        json!({ "op": "grant", "id": A, "role": "focus" }),
    ])
    .unwrap();
    Fixture { _dir: dir, db }
}

fn decode(bytes: &[u8]) -> Json {
    serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()
}

/// Runs reads, each given as JSON, and answers their envelope
fn reads(db: &Database, ops: Vec<Json>) -> crate::Result<Json> {
    let reads =
        Reads { reads: ops.into_iter().map(|o| serde_json::from_value::<ReadOp>(o).unwrap()).collect(), track: None };
    db.read(&reads).map(|b| decode(&b))
}

/// One query's answer
fn query(db: &Database, plan: Json) -> Json {
    let mut answer = reads(db, vec![json!({ "read": "query", "plan": plan })]).unwrap();
    answer[0].take()
}

fn ids(db: &Database, start: Json, steps: Json) -> Json {
    query(db, json!({ "start": start, "steps": steps, "terminal": { "answer": "ids" } }))["ids"].clone()
}

fn types(t: &[&str]) -> Json {
    json!({ "from": "types", "types": t })
}

fn of_ids(i: &[&str]) -> Json {
    json!({ "from": "ids", "ids": i })
}

#[test]
fn starts_from_a_type_types_ids_and_every_entity() {
    let f = fixture();
    assert_eq!(ids(&f.db, types(&["Task"]), json!([])), json!([A, B, C, D]));
    assert_eq!(ids(&f.db, types(&["Project", "Task"]), json!([])), json!([P, A, B, C, D]));
    assert_eq!(
        ids(&f.db, of_ids(&[D, "Task-gone", A]), json!([])),
        json!([D, A]),
        "ids still in the store, in the order given"
    );
    assert_eq!(ids(&f.db, json!({ "from": "all" }), json!([])), json!([P, A, B, C, D]));
    let e = reads(
        &f.db,
        vec![json!({ "read": "query", "plan": { "start": types(&["Nope"]), "terminal": { "answer": "ids" } } })],
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnknownType);
}

#[test]
fn filters_of_type_within_where_has_lacks_with_role_and_by_link() {
    let f = fixture();
    let db = &f.db;
    assert_eq!(ids(db, json!({ "from": "all" }), json!([{ "step": "ofType", "type": "Project" }])), json!([P]));
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "within", "ids": [D, A] }])), json!([A, D]));
    assert_eq!(
        ids(db, types(&["Task"]), json!([{ "step": "where", "field": "status", "value": "open" }])),
        json!([A, C, D])
    );
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "has", "field": "rank" }])), json!([A, B, D]));
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "lacks", "field": "rank" }])), json!([C]));
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "where", "field": "rank", "value": 10 }])), json!([B]));
    super::write(
        db,
        vec![
            json!({ "op": "append", "id": C, "field": "tag", "value": "x" }),
            json!({ "op": "append", "id": C, "field": "tag", "value": "y" }),
            json!({ "op": "append", "id": D, "field": "tag", "value": "y" }),
            json!({ "op": "set", "id": A, "fields": { "tag": { "k": "y" } } }),
        ],
    )
    .unwrap();
    // A value among several of a list's, within what the query already holds
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "where", "field": "tag", "value": "y" }])), json!([C, D]));
    assert_eq!(ids(db, of_ids(&[D, C]), json!([{ "step": "where", "field": "tag", "value": "y" }])), json!([D, C]));
    // A map is never matched by value
    assert_eq!(
        ids(db, types(&["Task"]), json!([{ "step": "where", "field": "tag", "value": { "k": "y" } }])),
        json!([])
    );
    assert_eq!(ids(db, json!({ "from": "all" }), json!([{ "step": "withRole", "role": "focus" }])), json!([A]));
    assert_eq!(
        ids(db, types(&["Task"]), json!([{ "step": "linkedFrom", "kind": "contains", "other": P }])),
        json!([A, B])
    );
    assert_eq!(
        ids(db, types(&["Project"]), json!([{ "step": "linkedTo", "kind": "contains", "other": A }])),
        json!([P])
    );
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "linkedWith", "other": P }])), json!([A, B, C]));
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "linkedFrom", "kind": "contains" }])), json!([A, B]));
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "unlinkedFrom", "kind": "contains" }])), json!([C, D]));
}

#[test]
fn a_null_probe_matches_a_field_that_is_null_or_missing() {
    let f = fixture();
    super::write(&f.db, vec![json!({ "op": "set", "id": B, "fields": { "tag": null } })]).unwrap();
    let null = json!([{ "step": "where", "field": "tag", "value": null }]);
    assert_eq!(ids(&f.db, types(&["Task"]), null.clone()), json!([A, B, C, D]));
    assert_eq!(ids(&f.db, types(&["Task"]), json!([{ "step": "where", "field": "status", "value": null }])), json!([]));
}

#[test]
fn out_and_in_walk_edges_by_kinds_and_types_and_edges_answer_relation_rows() {
    let f = fixture();
    let db = &f.db;
    assert_eq!(ids(db, of_ids(&[P]), json!([{ "step": "out", "kinds": ["contains", "owns"] }])), json!([A, B, C]));
    assert_eq!(
        ids(db, of_ids(&[P]), json!([{ "step": "out", "kinds": ["contains"], "types": ["Project"] }])),
        json!([])
    );
    assert_eq!(
        ids(db, of_ids(&[P]), json!([{ "step": "out", "kinds": ["contains"], "types": ["Task"] }])),
        json!([A, B])
    );
    assert_eq!(ids(db, of_ids(&[A, B]), json!([{ "step": "in", "kinds": ["contains"] }])), json!([P]));
    let edges = query(
        db,
        json!({ "start": of_ids(&[P]), "terminal": { "answer": "edges", "kinds": ["owns", "contains"], "outward": true } }),
    );
    let shape: Vec<(Json, Json, Json)> = edges["relations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["kind"].clone(), r["source"].clone(), r["target"].clone()))
        .collect();
    assert_eq!(
        shape,
        [
            (json!("owns"), json!(P), json!(C)),
            (json!("contains"), json!(P), json!(A)),
            (json!("contains"), json!(P), json!(B))
        ]
    );
    assert!(edges["relations"][0]["id"].as_str().unwrap().starts_with("Relation-"));
    // An edge is findRelations' row
    let found = reads(db, vec![json!({ "read": "findRelations", "source": P, "kind": "owns" })]).unwrap();
    assert_eq!(found[0]["relations"][0], edges["relations"][0]);
    let picked = query(
        db,
        json!({ "start": of_ids(&[P]), "steps": [{ "step": "out", "kinds": ["contains", "owns"], "types": ["Task"] }], "terminal": { "answer": "rows", "fields": ["rank"] } }),
    );
    // A field the row lacks is left out
    assert_eq!(picked["rows"], json!([{ "id": A, "rank": 2 }, { "id": B, "rank": 10 }, { "id": C }]));
}

#[test]
fn a_walk_steps_around_a_far_end_in_the_trash() {
    let f = fixture();
    super::write(&f.db, vec![json!({ "op": "trash", "id": A })]).unwrap();
    assert_eq!(ids(&f.db, of_ids(&[P]), json!([{ "step": "out", "kinds": ["contains"] }])), json!([B]));
    let with = query(
        &f.db,
        json!({ "scope": "withTrashed", "start": of_ids(&[P]), "steps": [{ "step": "out", "kinds": ["contains"] }], "terminal": { "answer": "ids" } }),
    );
    assert_eq!(with["ids"], json!([A, B]));
    let trashed =
        query(&f.db, json!({ "scope": "trashed", "start": types(&["Task"]), "terminal": { "answer": "ids" } }));
    assert_eq!(trashed["ids"], json!([A]));
    assert_eq!(query(&f.db, json!({ "start": types(&["Task"]), "terminal": { "answer": "count" } }))["n"], 3);
}

#[test]
fn descendants_and_ancestors_reach_each_entity_once_nearest_first() {
    let f = fixture();
    let db = &f.db;
    super::write(
        db,
        vec![
            json!({ "op": "link", "source": A, "kind": "parent_of", "target": B }),
            json!({ "op": "link", "source": B, "kind": "parent_of", "target": C }),
            json!({ "op": "link", "source": A, "kind": "parent_of", "target": D }),
        ],
    )
    .unwrap();
    assert_eq!(ids(db, of_ids(&[A]), json!([{ "step": "descendants", "kinds": ["parent_of"] }])), json!([B, D, C]));
    assert_eq!(ids(db, of_ids(&[C]), json!([{ "step": "ancestors", "kinds": ["parent_of"] }])), json!([B, A]));
    let e = reads(db, vec![json!({ "read": "query", "plan": { "start": of_ids(&[A]), "steps": [{ "step": "out", "kinds": ["follows"] }], "terminal": { "answer": "ids" } } })]).unwrap_err();
    assert_eq!(
        e.code,
        ErrorCode::UnknownRelationKind,
        "a read naming an undeclared kind is refused, not answered with nothing"
    );
    let cycle =
        reads(db, vec![json!({ "read": "wouldCreateCycle", "source": C, "target": A, "kinds": ["parent_of"] })])
            .unwrap();
    assert_eq!(cycle[0]["value"], true);
    // B, between them, in the trash: its edges still count, for the read as for the link it answers about
    super::write(db, vec![json!({ "op": "trash", "id": B })]).unwrap();
    let cycle =
        reads(db, vec![json!({ "read": "wouldCreateCycle", "source": C, "target": A, "kinds": ["parent_of"] })])
            .unwrap();
    assert_eq!(cycle[0]["value"], true);
    let refused = super::write(db, vec![json!({ "op": "link", "source": C, "kind": "parent_of", "target": A })]);
    assert_eq!(refused.unwrap_err().code, ErrorCode::Cycle);
}

#[test]
fn order_by_numbers_and_strings_missing_last_reverse_and_limit() {
    let f = fixture();
    let db = &f.db;
    let order = |field: &str, descending: bool| {
        ids(db, types(&["Task"]), json!([{ "step": "orderBy", "field": field, "descending": descending }]))
    };
    assert_eq!(order("rank", false), json!([D, A, B, C]));
    assert_eq!(order("rank", true), json!([C, B, A, D]));
    assert_eq!(order("title", false), json!([B, D, A, C]), "ties keep the query's order");
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "reverse" }])), json!([D, C, B, A]));
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "limit", "n": 2 }])), json!([A, B]));
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "limit", "n": 2.5 }])), json!([A, B]));
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "limit", "n": -1 }])), json!([]));
}

#[test]
fn order_by_sorts_by_rank_then_value_a_timestamp_by_its_time() {
    let f = fixture();
    let date = |ms: u64| {
        let secs = (ms / 1000) as u32;
        rmpv::Value::Ext(-1, secs.to_be_bytes().to_vec())
    };
    let mut ops = Vec::new();
    let values: Vec<(&str, Option<rmpv::Value>)> = vec![
        ("text", Some("pear".into())),
        ("none", None),
        ("nil", Some(rmpv::Value::Nil)),
        ("ten", Some(10.into())),
        ("later", Some(date(1_769_904_000_000))),
        ("two", Some(2.into())),
        ("earlier", Some(date(1_767_225_600_000))),
        ("flag", Some(true.into())),
        ("word", Some("apple".into())),
        ("list", Some(rmpv::Value::Array(vec!["x".into()]))),
    ];
    for (name, value) in &values {
        let mut fields = std::collections::BTreeMap::new();
        if let Some(v) = value {
            fields.insert("value".to_owned(), v.clone());
        }
        ops.push(crate::engine::batch::Op::Create {
            entity_type: "Person".into(),
            id: Some(format!("Person-{name}")),
            fields,
            short_code: None,
            created_at: None,
            updated_at: None,
        });
    }
    f.db.write(&crate::engine::batch::Batch { ops, now: Some(1.0) }, false).unwrap();
    let order = |descending: bool| -> Vec<String> {
        ids(&f.db, types(&["Person"]), json!([{ "step": "orderBy", "field": "value", "descending": descending }]))
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().trim_start_matches("Person-").to_owned())
            .collect()
    };
    assert_eq!(order(false), ["flag", "two", "ten", "earlier", "later", "word", "text", "list", "none", "nil"]);
    assert_eq!(order(true), ["none", "nil", "list", "text", "word", "later", "earlier", "ten", "two", "flag"]);
}

#[test]
fn order_by_writes_a_list_s_numbers_as_json_stringify_does() {
    let f = fixture();
    // `JSON.stringify` writes [1e-7] and [5e-8] in exponent notation, so "[1" sorts before "[5"; and 2^64 - 1 as
    // the double it reads as, 18446744073709552000, after 1e19's 10000000000000000000
    let values: Vec<(&str, rmpv::Value)> = vec![
        ("max", rmpv::Value::Array(vec![u64::MAX.into()])),
        ("e19", rmpv::Value::Array(vec![1e19.into()])),
        ("small", rmpv::Value::Array(vec![5e-8.into()])),
        ("tiny", rmpv::Value::Array(vec![1e-7.into()])),
    ];
    let ops = values
        .into_iter()
        .map(|(name, value)| crate::engine::batch::Op::Create {
            entity_type: "Person".into(),
            id: Some(format!("Person-{name}")),
            fields: [("value".to_owned(), value)].into(),
            short_code: None,
            created_at: None,
            updated_at: None,
        })
        .collect();
    f.db.write(&crate::engine::batch::Batch { ops, now: Some(1.0) }, false).unwrap();
    let ordered = ids(&f.db, types(&["Person"]), json!([{ "step": "orderBy", "field": "value" }]));
    assert_eq!(ordered, json!(["Person-e19", "Person-max", "Person-tiny", "Person-small"]));
}

#[test]
fn distinct_and_group_by() {
    let f = fixture();
    let db = &f.db;
    assert_eq!(ids(db, of_ids(&[A, A, B]), json!([{ "step": "distinct" }])), json!([A, B]));
    assert_eq!(ids(db, types(&["Task"]), json!([{ "step": "distinct", "field": "title" }])), json!([A, B, C]));
    let groups =
        query(db, json!({ "start": types(&["Task"]), "terminal": { "answer": "groupBy", "field": "status" } }));
    assert_eq!(groups["groups"], json!([["open", [A, C, D]], ["done", [B]]]));
}

#[test]
fn terminals_count_first_last_exists_and_rows() {
    let f = fixture();
    let db = &f.db;
    let open = json!([{ "step": "where", "field": "status", "value": "open" }]);
    let answer = |terminal: Json, steps: &Json| {
        query(db, json!({ "start": types(&["Task"]), "steps": steps, "terminal": terminal }))
    };
    assert_eq!(answer(json!({ "answer": "count" }), &open)["n"], 3);
    assert_eq!(answer(json!({ "answer": "first" }), &open)["id"], A);
    assert_eq!(answer(json!({ "answer": "last" }), &open)["id"], D);
    assert_eq!(answer(json!({ "answer": "exists" }), &open)["value"], true);
    let gone = json!([{ "step": "where", "field": "status", "value": "gone" }]);
    assert_eq!(answer(json!({ "answer": "first" }), &gone)["id"], Json::Null);
    assert_eq!(answer(json!({ "answer": "exists" }), &gone)["value"], false);
    let rank1 = json!([{ "step": "where", "field": "rank", "value": 1 }]);
    assert_eq!(
        answer(json!({ "answer": "rows", "fields": ["id", "title", "nope"] }), &rank1)["rows"],
        json!([{ "id": D, "title": "apple" }])
    );
    assert_eq!(
        answer(json!({ "answer": "firstRow", "fields": ["title"] }), &json!([]))["row"],
        json!({ "id": A, "title": "banana" })
    );
    assert_eq!(
        answer(json!({ "answer": "firstRow" }), &json!([{ "step": "where", "field": "rank", "value": 99 }]))["row"],
        Json::Null
    );
    let row = query(db, json!({ "start": of_ids(&[A]), "terminal": { "answer": "rows" } }))["rows"][0].clone();
    assert_eq!(row["entityType"], "Task");
    assert_eq!(
        (row["title"].clone(), row["rank"].clone(), row["role"].clone()),
        (json!("banana"), json!(2), json!(["focus"]))
    );
}

#[test]
fn rows_say_whether_a_map_may_hold_proto_and_hold_it_as_a_field() {
    let f = fixture();
    let rows = query(&f.db, json!({ "start": of_ids(&[A]), "terminal": { "answer": "rows" } }));
    assert_eq!(rows["plainObjects"], true);
    super::write(
        &f.db,
        vec![json!({ "op": "create", "type": "Person", "id": "Person-p", "fields": { "__proto__": { "x": 1 } } })],
    )
    .unwrap();
    let rows = query(&f.db, json!({ "start": of_ids(&["Person-p"]), "terminal": { "answer": "rows" } }));
    assert_eq!(rows["plainObjects"], false);
    assert_eq!(rows["rows"][0]["__proto__"], json!({ "x": 1 }));
}

#[test]
fn first_exists_limit_and_count_read_only_as_far_as_their_answer() {
    let f = fixture();
    let plan = |steps: Json, terminal: Json| -> Plan {
        serde_json::from_value(json!({ "start": types(&["Task"]), "steps": steps, "terminal": terminal })).unwrap()
    };
    let visited = |p: Plan| f.db.snapshot(|s| run_counted(s, &p)).unwrap().1.entities;
    assert_eq!(visited(plan(json!([]), json!({ "answer": "first" }))), 1);
    assert_eq!(visited(plan(json!([]), json!({ "answer": "exists" }))), 1);
    assert_eq!(visited(plan(json!([]), json!({ "answer": "firstRow" }))), 1);
    assert_eq!(visited(plan(json!([{ "step": "limit", "n": 2 }]), json!({ "answer": "ids" }))), 2);
    assert_eq!(visited(plan(json!([{ "step": "has", "field": "title" }]), json!({ "answer": "count" }))), 4);
    assert_eq!(visited(plan(json!([]), json!({ "answer": "count" }))), 0, "a type's count is read off its counter");
    // An indexed where reads the index's matches, not the type's rows
    assert_eq!(
        visited(plan(json!([{ "step": "where", "field": "status", "value": "done" }]), json!({ "answer": "ids" }))),
        1
    );
    // Unindexed, it walks the rows and finds the same
    assert_eq!(
        visited(plan(json!([{ "step": "where", "field": "title", "value": "apple" }]), json!({ "answer": "ids" }))),
        4
    );
}

#[test]
fn direct_reads_answer_a_row_a_field_and_a_relation() {
    let f = fixture();
    let answers = reads(
        &f.db,
        vec![
            json!({ "read": "row", "id": A }),
            json!({ "read": "row", "id": "Task-none" }),
            json!({ "read": "field", "id": A, "field": "title" }),
            json!({ "read": "field", "id": A, "field": "rank" }),
            json!({ "read": "field", "id": C, "field": "rank" }),
            json!({ "read": "field", "id": A, "field": "entityType" }),
        ],
    )
    .unwrap();
    assert_eq!(answers[0]["row"]["title"], "banana");
    assert_eq!(answers[1]["row"], Json::Null);
    assert_eq!((answers[2]["present"].clone(), answers[2]["value"].clone()), (json!(true), json!("banana")));
    assert_eq!(answers[3]["value"], 2);
    assert_eq!(answers[4]["present"], false);
    assert_eq!(answers[5]["value"], "Task");
}

#[test]
fn a_unit_reads_its_own_writes() {
    let f = fixture();
    let mut unit = f.db.begin().unwrap();
    unit.write(&super::batch(vec![
        json!({ "op": "create", "type": "Task", "id": "Task-e", "fields": { "status": "open" } }),
    ]))
    .unwrap();
    let plan: Plan = serde_json::from_value(json!({ "start": types(&["Task"]), "steps": [{ "step": "where", "field": "status", "value": "open" }], "terminal": { "answer": "ids" } })).unwrap();
    let inside = decode(
        &unit
            .read(&Reads {
                reads: vec![ReadOp::Query { plan: plan.clone(), visits: false, explain: false }],
                track: None,
            })
            .unwrap(),
    );
    assert_eq!(inside[0]["ids"], json!([A, C, D, "Task-e"]));
    let outside = decode(
        &f.db.read(&Reads { reads: vec![ReadOp::Query { plan, visits: false, explain: false }], track: None }).unwrap(),
    );
    assert_eq!(outside[0]["ids"], json!([A, C, D]), "a reader sees none of an open unit");
    unit.abort();
}

#[test]
fn a_query_asked_for_its_visits_answers_how_many_entities_its_walk_reached() {
    let f = fixture();
    let walk = |terminal: &str| {
        json!({ "read": "query", "visits": true, "plan": {
            "start": types(&["Task"]),
            "steps": [{ "step": "where", "field": "rank", "value": 10 }],
            "terminal": { "answer": terminal }
        } })
    };
    let answers = reads(&f.db, vec![walk("first"), walk("ids")]).unwrap();
    // `first` stops at the match, the second Task; `ids` walks every Task
    assert_eq!(answers[0], json!({ "kind": "visited", "answer": { "kind": "id", "id": B }, "visited": 2 }));
    assert_eq!(answers[1]["visited"], 4);
    assert_eq!(answers[1]["answer"]["ids"], json!([B]));
    let plain = reads(
        &f.db,
        vec![json!({ "read": "query", "plan": { "start": types(&["Task"]), "terminal": { "answer": "count" } } })],
    )
    .unwrap();
    assert_eq!(plain[0], json!({ "kind": "count", "n": 4 }), "without visits, the answer itself");
}

#[test]
fn with_role_after_another_filter_keeps_what_that_filter_kept() {
    let f = fixture();
    super::write(&f.db, vec![json!({ "op": "grant", "id": B, "role": "focus" })]).unwrap();
    assert_eq!(ids(&f.db, types(&["Task"]), json!([{ "step": "withRole", "role": "focus" }])), json!([A, B]));
    let open_focused =
        json!([{ "step": "where", "field": "status", "value": "open" }, { "step": "withRole", "role": "focus" }]);
    assert_eq!(ids(&f.db, types(&["Task"]), open_focused), json!([A]));
}

#[test]
fn find_relations_by_both_ends_or_neither() {
    let f = fixture();
    let found = |op: Json| -> Vec<(Json, Json, Json)> {
        reads(&f.db, vec![op]).unwrap()[0]["relations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["kind"].clone(), r["source"].clone(), r["target"].clone()))
            .collect()
    };
    assert_eq!(
        found(json!({ "read": "findRelations", "source": P, "target": B })),
        [(json!("contains"), json!(P), json!(B))]
    );
    assert_eq!(found(json!({ "read": "findRelations", "kind": "owns" })), [(json!("owns"), json!(P), json!(C))]);
    assert_eq!(found(json!({ "read": "findRelations" })).len(), 3);
}

#[test]
fn a_row_read_says_whether_its_map_may_hold_proto() {
    let f = fixture();
    let row = |id: &str| reads(&f.db, vec![json!({ "read": "row", "id": id })]).unwrap()[0].take();
    assert_eq!(row(A)["plainObjects"], true);
    super::write(
        &f.db,
        vec![json!({ "op": "create", "type": "Person", "id": "Person-p", "fields": { "__proto__": { "x": 1 } } })],
    )
    .unwrap();
    assert_eq!(row("Person-p")["plainObjects"], false);
}
