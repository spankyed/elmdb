//! A rank index at the default tolerance holds the top of a graph with hubs to exact PageRank: what that default was chosen
//! for (`docs/evidence/rank-index.md`), so loosening it past what readers can see fails here.
#[allow(dead_code)]
mod support;

use std::collections::HashSet;

use elmdb::Database;
use elmdb::engine::batch::Batch;
use elmdb::engine::plan::Reads;
use elmdb::schema::{SCHEMA_LANGUAGE, Schema};
use serde_json::{Value as Json, json};

fn ids(db: &Database, step: Json, limit: Option<usize>) -> Vec<String> {
    let mut steps = vec![step];
    steps.extend(limit.map(|n| json!({ "step": "limit", "n": n })));
    let reads: Reads = serde_json::from_value(json!({ "reads": [{ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Doc"] }, "steps": steps, "terminal": { "answer": "ids" } } }] }))
    .unwrap();
    let bytes = db.read(&reads).unwrap();
    let answer = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    answer[0]["ids"].as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned()).collect()
}

/// The tolerance an index declaring none is kept to
fn default_tolerance() -> f64 {
    let schema = Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": { "Doc": { "fields": {} } },
        "relations": { "refs": { "cardinality": "many-to-many" } },
        "ranks": { "docRank": { "types": ["Doc"], "relations": ["refs"] } }
    }))
    .unwrap();
    schema.ranks["docRank"].tolerance
}

#[test]
fn the_default_tolerance_keeps_the_top_of_a_graph_with_hubs_exact() {
    for rows in [1000, 4000] {
        let f = support::Fixture::ranked_hub_graph_at(rows, 5, default_tolerance());
        // Changes from later documents, each drained or within the budget, as a graph in use has
        for i in 0..30 {
            let batch = Batch {
                ops: vec![
                    serde_json::from_value(json!({ "op": "link", "source": format!("Doc-{:08}", rows - 1 - i),
                        "kind": "refs", "target": format!("Doc-{:08}", i * 7 + 1) }))
                    .unwrap(),
                ],
                now: Some(1.0),
            };
            f.db.write(&batch, false).unwrap();
            f.db.wait_for_index_builds().unwrap();
        }
        let exact = |n| {
            let step = json!({ "step": "rank", "kinds": ["refs"], "iterations": 100000, "tolerance": 1e-13 });
            ids(&f.db, step, Some(n))
        };
        let top = |n| ids(&f.db, json!({ "step": "ranked", "index": "docRank" }), Some(n));
        assert_eq!(top(10), exact(10), "{rows}");
        let truth: HashSet<String> = exact(100).into_iter().collect();
        let held = top(100).iter().filter(|id| truth.contains(*id)).count();
        assert!(held >= 99, "{held} of the true top 100 at {rows}");
    }
}
