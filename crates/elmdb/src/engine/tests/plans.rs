//! The planner's choices, each shown by what a plan visited and held to the answer the plan's plain reading gives: a
//! type start's leading filters read from an index wherever the indexed `where` stands among them, an index lookup
//! that stops at a `limit`, `orderBy` with `limit` holding only the first, and EXPLAIN's stages.

use serde_json::{Value as Json, json};

use crate::engine::plan::{ReadOp, Reads};
use crate::schema::Schema;
use crate::{Database, OpenOptions};

const OPEN: usize = 13_000;
const DONE: usize = 2_000;

fn schema() -> Schema {
    Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "roles": { "focus": {} },
        "relations": {},
        "types": {
            "Task": {
                "fields": {
                    "title": { "type": "string", "optional": true },
                    "rank": { "type": "number", "optional": true },
                    "status": { "type": "string", "optional": true }
                },
                "indexes": { "status": { "kind": "value", "fields": ["status"] } }
            }
        }
    }))
    .unwrap()
}

/// 13,000 open tasks and 2,000 done ones, interleaved, every third without a title, ranks repeating every eleven
fn fixture() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema()), syncs: false, ..Default::default() }).unwrap();
    let ops: Vec<Json> = (0..OPEN + DONE)
        .map(|i| {
            let status = if i % ((OPEN + DONE) / DONE) == 0 { "done" } else { "open" };
            let mut fields = json!({ "status": status, "rank": i % 11 });
            if i % 3 != 0 {
                fields["title"] = json!(format!("task {i}"));
            }
            json!({ "op": "create", "type": "Task", "id": format!("Task-{i:05}"), "fields": fields })
        })
        .collect();
    super::write(&db, ops).unwrap();
    (dir, db)
}

fn read(db: &Database, op: Json) -> Json {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    let bytes = db.read(&reads).unwrap();
    let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    answers[0].take()
}

fn plan(steps: Json, terminal: &str) -> Json {
    json!({ "start": { "from": "types", "types": ["Task"] }, "steps": steps, "terminal": { "answer": terminal } })
}

/// A plan's answer — its ids, for the `ids` terminal — and the entities it visited
fn visited(db: &Database, steps: Json, terminal: &str) -> (Json, u64) {
    let answer = read(db, json!({ "read": "query", "visits": true, "plan": plan(steps, terminal) }));
    let found = if terminal == "ids" { answer["answer"]["ids"].clone() } else { answer["answer"].clone() };
    (found, answer["visited"].as_u64().unwrap())
}

fn count(db: &Database, status: &str) -> usize {
    read(db, json!({ "read": "query", "plan": plan(json!([{ "step": "where", "field": "status", "value": status }]), "count") }))["n"]
        .as_u64()
        .unwrap() as usize
}

#[test]
fn an_indexed_where_with_a_limit_visits_its_limit_not_every_match() {
    let (_dir, db) = fixture();
    let open = count(&db, "open");
    assert!(open > 12_000, "the fixture's open tasks: {open}");
    let where_open = json!({ "step": "where", "field": "status", "value": "open" });
    let (page, visits) = visited(&db, json!([where_open, { "step": "limit", "n": 50 }]), "ids");
    assert_eq!(visits, 50, "the lookup stops at the limit");
    let (all, every) = visited(&db, json!([where_open]), "ids");
    assert_eq!(every as usize, open, "without a limit it reads every match");
    assert_eq!(page.as_array().unwrap()[..], all.as_array().unwrap()[..50], "the first fifty, in creation order");
    let (_, first) = visited(&db, json!([where_open]), "first");
    assert_eq!(first, 1);
}

#[test]
fn a_where_after_other_filters_reads_its_index_and_answers_as_the_walk_would() {
    let (_dir, db) = fixture();
    let done = count(&db, "done");
    let where_done = json!({ "step": "where", "field": "status", "value": "done" });
    for before in [
        json!({ "step": "has", "field": "title" }),
        json!({ "step": "lacks", "field": "title" }),
        json!({ "step": "where", "field": "rank", "value": 3 }),
        json!({ "step": "ofType", "type": "Task" }),
    ] {
        let (indexed, visits) = visited(&db, json!([before, where_done]), "ids");
        assert_eq!(
            visits as usize,
            done,
            "{before}: read from the index's {done} matches, not the type's {}",
            OPEN + DONE
        );
        // The same filters with the index first: the set and the order the walk keeps
        let (first, _) = visited(&db, json!([where_done, before]), "ids");
        assert_eq!(indexed, first, "{before}");
        assert!(!indexed.as_array().unwrap().is_empty(), "{before}: the case keeps something");
    }
    // After a step that isn't a filter of the type start, the where filters what reaches it
    let (_, walked) = visited(&db, json!([{ "step": "limit", "n": 100 }, where_done]), "ids");
    assert_eq!(walked, 100);
}

#[test]
fn order_by_then_limit_answers_what_sorting_everything_would() {
    let (_dir, db) = fixture();
    for descending in [false, true] {
        let order = json!({ "step": "orderBy", "field": "rank", "descending": descending });
        let (sorted, _) = visited(&db, json!([order]), "ids");
        let sorted = sorted.as_array().unwrap().clone();
        for n in [1, 10, 2_500, OPEN + DONE + 5] {
            let (first, _) = visited(&db, json!([order, { "step": "limit", "n": n }]), "ids");
            let expected = &sorted[..n.min(sorted.len())];
            assert_eq!(first.as_array().unwrap()[..], expected[..], "descending {descending}, limit {n}");
        }
        // A missing field sorts as its own rank, and ties keep creation order, filtered or not
        let titled = json!({ "step": "orderBy", "field": "title", "descending": descending });
        let (all, _) = visited(&db, json!([{ "step": "where", "field": "rank", "value": 2 }, titled]), "ids");
        let (some, _) = visited(
            &db,
            json!([{ "step": "where", "field": "rank", "value": 2 }, titled, { "step": "limit", "n": 30 }]),
            "ids",
        );
        assert_eq!(some.as_array().unwrap()[..], all.as_array().unwrap()[..30], "titles, descending {descending}");
    }
}

/// EXPLAIN's answer, as `{ stage, index?, visited, kept }` per stage, without the times
fn explained(db: &Database, plan: Json) -> (Json, Json, Vec<Json>) {
    let answer = read(db, json!({ "read": "query", "explain": true, "plan": plan.clone() }));
    let plain = read(db, json!({ "read": "query", "plan": plan }));
    assert_eq!(answer["kind"], "explained");
    assert!(answer["ms"].as_f64().unwrap() >= 0.0);
    let stages = answer["stages"].as_array().unwrap();
    let total: f64 = stages.iter().map(|s| s["ms"].as_f64().unwrap()).sum();
    assert!(
        total <= answer["ms"].as_f64().unwrap() + 0.5,
        "stage times are each stage's own: {total} of {}",
        answer["ms"]
    );
    let stages = stages
        .iter()
        .map(|s| {
            let mut s = s.clone();
            s.as_object_mut().unwrap().remove("ms");
            s
        })
        .collect();
    (answer["answer"].clone(), plain, stages)
}

#[test]
fn explain_answers_each_stage_its_index_what_it_visited_and_kept() {
    let (_dir, db) = fixture();
    let open = count(&db, "open") as u64;
    let done = count(&db, "done") as u64;
    let all = (OPEN + DONE) as u64;

    // An index read straight after the start, stopped by a limit, answering rows
    let (answer, plain, stages) = explained(
        &db,
        plan(json!([{ "step": "where", "field": "status", "value": "open" }, { "step": "limit", "n": 50 }]), "rows"),
    );
    assert_eq!(answer, plain, "the answer is the plain read's");
    assert_eq!(
        stages,
        vec![
            json!({ "stage": "types", "visited": 0, "kept": 0 }),
            json!({ "stage": "where", "index": "status", "visited": 50, "kept": 50 }),
            json!({ "stage": "limit", "visited": 50, "kept": 50 }),
            json!({ "stage": "rows", "visited": 50, "kept": 50 }),
        ]
    );

    // A where after the start that no index covers walks the type itself; one after another step filters what
    // reaches it
    let (answer, plain, stages) =
        explained(&db, plan(json!([{ "step": "where", "field": "rank", "value": 3 }]), "count"));
    assert_eq!(answer, plain);
    let ranked = plain["n"].as_u64().unwrap();
    assert_eq!(
        stages,
        vec![
            json!({ "stage": "types", "visited": 0, "kept": 0 }),
            json!({ "stage": "where", "index": "none: walks the type", "visited": all, "kept": ranked }),
            json!({ "stage": "count", "visited": ranked, "kept": ranked }),
        ]
    );
    let (answer, plain, stages) = explained(
        &db,
        plan(json!([{ "step": "lacks", "field": "title" }, { "step": "where", "field": "rank", "value": 3 }]), "count"),
    );
    assert_eq!(answer, plain);
    let untitled = (0..OPEN + DONE).filter(|i| i % 3 == 0).count() as u64;
    let both = plain["n"].as_u64().unwrap();
    assert_eq!(
        stages,
        vec![
            json!({ "stage": "types", "visited": all, "kept": all }),
            json!({ "stage": "lacks", "visited": all, "kept": untitled }),
            json!({ "stage": "where", "visited": untitled, "kept": both }),
            json!({ "stage": "count", "visited": both, "kept": both }),
        ]
    );

    // An index driving the filters before it: they keep their part inside the stage that reads it
    let (answer, plain, stages) = explained(
        &db,
        plan(
            json!([{ "step": "has", "field": "title" }, { "step": "where", "field": "status", "value": "done" }]),
            "ids",
        ),
    );
    assert_eq!(answer, plain);
    let titled_done = plain["ids"].as_array().unwrap().len() as u64;
    assert_eq!(
        stages,
        vec![
            json!({ "stage": "types", "visited": 0, "kept": 0 }),
            json!({ "stage": "has", "visited": 0, "kept": 0 }),
            json!({ "stage": "where", "index": "status", "visited": done, "kept": titled_done }),
            json!({ "stage": "ids", "visited": titled_done, "kept": titled_done }),
        ]
    );

    // Every entity, ordered, its first ten: the sort holds ten while it reads them all
    let (answer, plain, stages) = explained(
        &db,
        json!({ "start": { "from": "all" }, "steps": [
            { "step": "orderBy", "field": "rank" }, { "step": "limit", "n": 10 }
        ], "terminal": { "answer": "ids" } }),
    );
    assert_eq!(answer, plain);
    assert_eq!(
        stages,
        vec![
            json!({ "stage": "all", "visited": all, "kept": all }),
            json!({ "stage": "orderBy", "visited": all, "kept": 10 }),
            json!({ "stage": "limit", "visited": 10, "kept": 10 }),
            json!({ "stage": "ids", "visited": 10, "kept": 10 }),
        ]
    );

    // Ids given, one of them no entity's, then a where on an index the start can't read from
    let (answer, plain, stages) = explained(
        &db,
        json!({ "start": { "from": "ids", "ids": ["Task-00001", "Task-missing", "Task-00007"] }, "steps": [
            { "step": "where", "field": "status", "value": "open" }
        ], "terminal": { "answer": "first" } }),
    );
    assert_eq!(answer, plain);
    assert_eq!(
        stages,
        vec![
            json!({ "stage": "ids", "visited": 1, "kept": 1 }),
            json!({ "stage": "where", "visited": 1, "kept": 1 }),
            json!({ "stage": "first", "visited": 1, "kept": 1 }),
        ]
    );
    assert!(open > 0);
}
