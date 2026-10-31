//! Edge weights (`docs/roadmap/rank-weights.md`): a kind declared `weighted` takes a weight on each edge, stored on the
//! relation's row only when it isn't 1 and copied into its edge entries; rows answer it, `rank` walks and teleports by
//! weight and `communities` counts it, and `check()` holds the edge entries to the row.

use proptest::prelude::*;
use serde_json::{Value as Json, json};

use super::{batch, open, refused, write};
use crate::engine::algorithms::{self, Projection};
use crate::engine::batch::{Batch, Op, OpResult};
use crate::engine::maintenance::Load;
use crate::engine::plan::{ReadOp, Reads};
use crate::engine::unit::Read;
use crate::engine::{keys, relations};
use crate::error::ErrorCode;
use crate::schema::{Effect, Schema};
use crate::{Database, OpenOptions};

fn decode(bytes: &[u8]) -> Json {
    serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()
}

fn read(db: &Database, op: Json) -> crate::Result<Json> {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    db.read(&reads).map(|bytes| decode(&bytes)[0].clone())
}

fn query(db: &Database, plan: Json) -> crate::Result<Json> {
    read(db, json!({ "read": "query", "plan": plan }))
}

fn tasks(db: &Database, names: &[&str]) {
    write(db, names.iter().map(|n| json!({ "op": "create", "type": "Task", "id": n })).collect()).unwrap();
}

fn link(source: &str, kind: &str, target: &str, weight: Option<f64>) -> Json {
    let mut op = json!({ "op": "link", "source": source, "kind": kind, "target": target });
    if let Some(weight) = weight {
        op["weight"] = json!(weight);
    }
    op
}

/// The relation a link answered, and whether it linked a new one
fn linked(results: &[OpResult]) -> (String, bool) {
    match &results[0] {
        OpResult::Relation { id, linked } => (id.clone(), *linked),
        other => panic!("not a relation: {other:?}"),
    }
}

fn stored(db: &Database, relation: &str) -> relations::Stored {
    db.snapshot(|s| relations::get(s, relation)).unwrap().unwrap()
}

/// Every task ranked over `kinds` with these options, as `id → score`
fn scores(db: &Database, kinds: &[&str], options: Json) -> Vec<(String, f64)> {
    let mut step = json!({ "step": "rank", "kinds": kinds });
    step.as_object_mut().unwrap().extend(options.as_object().unwrap().clone());
    let hits = query(
        db,
        json!({ "start": { "from": "types", "types": ["Task"] }, "steps": [step], "terminal": { "answer": "hits" } }),
    )
    .unwrap();
    let mut found: Vec<(String, f64)> = hits["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["id"].as_str().unwrap().to_owned(), h["score"].as_f64().unwrap()))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

fn score(scores: &[(String, f64)], id: &str) -> f64 {
    scores.iter().find(|(at, _)| at == id).unwrap().1
}

#[test]
fn a_weighted_kind_stores_a_weight_other_than_1_and_its_rows_answer_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b", "Task-c", "Task-d"]);
    let (two, _) = linked(&write(&db, vec![link("Task-a", "weighs", "Task-b", Some(2.5))]).unwrap());
    let (one, _) = linked(&write(&db, vec![link("Task-a", "weighs", "Task-c", Some(1.0))]).unwrap());
    let (none, _) = linked(&write(&db, vec![link("Task-a", "weighs", "Task-d", None)]).unwrap());
    let (plain, _) = linked(&write(&db, vec![link("Task-a", "refs", "Task-b", None)]).unwrap());
    // 1, given or not, is no weight on the row, which leaves it the bytes an unweighted kind's row would be
    assert_eq!(stored(&db, &two).weight, Some(2.5));
    assert_eq!((stored(&db, &one).weight, stored(&db, &none).weight), (None, None));
    let row = |id: &str| read(&db, json!({ "read": "relation", "id": id })).unwrap()["relation"].clone();
    assert_eq!(row(&two)["weight"], json!(2.5));
    assert_eq!((row(&one)["weight"].clone(), row(&none)["weight"].clone()), (json!(1.0), json!(1.0)));
    assert!(row(&plain).get("weight").is_none(), "a kind not weighted answers no weight: {}", row(&plain));
    let found = read(&db, json!({ "read": "findRelations", "source": "Task-a", "kind": "weighs" })).unwrap();
    let weights: Vec<&Json> = found["relations"].as_array().unwrap().iter().map(|r| &r["weight"]).collect();
    assert_eq!(weights, [&json!(2.5), &json!(1.0), &json!(1.0)]);
    let edges = query(
        &db,
        json!({ "start": { "from": "ids", "ids": ["Task-a"] }, "terminal": { "answer": "edges", "kinds": ["weighs"] } }),
    )
    .unwrap();
    assert_eq!(edges["relations"][0]["weight"], json!(2.5), "{edges}");
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_weight_that_isnt_a_finite_number_of_at_least_0_or_on_a_kind_not_weighted_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b"]);
    let (relation, _) = linked(&write(&db, vec![link("Task-a", "weighs", "Task-b", Some(2.0))]).unwrap());
    let op = |weight: f64| Op::Link {
        source: "Task-a".into(),
        kind: "weighs".into(),
        target: "Task-b".into(),
        info: None,
        weight: Some(weight),
    };
    for weight in [-0.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let e = db.write(&Batch { ops: vec![op(weight)], now: Some(1000.0) }, false).unwrap_err();
        assert_eq!(e.code, ErrorCode::ValueInvalid, "{weight}: {e:?}");
        let update = Op::UpdateRelation {
            relation: relation.clone(),
            source: None,
            target: None,
            info: None,
            weight: Some(weight),
        };
        let e = db.write(&Batch { ops: vec![update], now: Some(1000.0) }, false).unwrap_err();
        assert_eq!(e.code, ErrorCode::ValueInvalid, "{weight}: {e:?}");
    }
    let e = refused(&db, vec![link("Task-a", "refs", "Task-b", Some(2.0))]);
    assert_eq!(e.code, ErrorCode::ValueInvalid);
    assert!(e.message.contains("refs"), "the message names the kind: {}", e.message);
    // 0 is a weight: an edge the walker never follows
    write(&db, vec![link("Task-b", "weighs", "Task-a", Some(0.0))]).unwrap();
    assert_eq!(super::edges(&db), ["Task-a|weighs|Task-b", "Task-b|weighs|Task-a"]);
    assert_eq!(stored(&db, &relation).weight, Some(2.0), "nothing a refused batch held was written");
}

#[test]
fn a_weight_is_part_of_an_edge_and_a_unique_kind_relinked_takes_the_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b"]);
    let (first, made) = linked(&write(&db, vec![link("Task-a", "weighs", "Task-b", Some(2.0))]).unwrap());
    assert!(made);
    // The same weight is the same edge, and no weight matches any edge between the ends
    assert_eq!(
        linked(&write(&db, vec![link("Task-a", "weighs", "Task-b", Some(2.0))]).unwrap()),
        (first.clone(), false)
    );
    assert_eq!(linked(&write(&db, vec![link("Task-a", "weighs", "Task-b", None)]).unwrap()), (first.clone(), false));
    // Another weight is a parallel edge, which adds to the first in a rank
    let (second, made) = linked(&write(&db, vec![link("Task-a", "weighs", "Task-b", Some(3.0))]).unwrap());
    assert!(made && second != first);

    let (rated, _) = linked(&write(&db, vec![link("Task-a", "rates", "Task-b", Some(2.0))]).unwrap());
    let changes = db.write(&batch(vec![link("Task-a", "rates", "Task-b", Some(5.0))]), false).unwrap().1;
    assert_eq!(changes.kinds.iter().collect::<Vec<_>>(), ["rates"]);
    assert_eq!(stored(&db, &rated).weight, Some(5.0));
    assert_eq!(super::edges(&db).iter().filter(|e| e.contains("rates")).count(), 1);
    let entry = db.snapshot(|s| s.get(&keys::edge(true, "Task-a", "rates", stored(&db, &rated).seq))).unwrap();
    assert_eq!(entry.unwrap(), relations::edge_value(&rated, "Task-b", Some(5.0)), "the edge entries filed again");
    // Back to 1: the row and its entries hold no weight
    write(&db, vec![link("Task-a", "rates", "Task-b", Some(1.0))]).unwrap();
    assert_eq!(stored(&db, &rated).weight, None);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
}

#[test]
fn update_relation_replaces_the_weight_with_or_without_a_move() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b", "Task-c"]);
    let (relation, _) = linked(&write(&db, vec![link("Task-a", "weighs", "Task-b", Some(2.0))]).unwrap());
    let update = |patch: Json| {
        let mut op = json!({ "op": "updateRelation", "relation": relation });
        op.as_object_mut().unwrap().extend(patch.as_object().unwrap().clone());
        db.write(&batch(vec![op]), false).unwrap().1
    };
    let changes = update(json!({ "weight": 4.0 }));
    assert_eq!(changes.kinds.iter().collect::<Vec<_>>(), ["weighs"]);
    assert_eq!(stored(&db, &relation).weight, Some(4.0));
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    // Info alone keeps the weight
    update(json!({ "info": "kept" }));
    assert_eq!(stored(&db, &relation).weight, Some(4.0));
    update(json!({ "target": "Task-c", "weight": 0.5 }));
    let moved = stored(&db, &relation);
    assert_eq!((moved.target.as_str(), moved.weight), ("Task-c", Some(0.5)));
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    let e = refused(&db, vec![json!({ "op": "updateRelation", "relation": relation, "weight": -1 })]);
    assert_eq!(e.code, ErrorCode::ValueInvalid);
}

#[test]
fn rank_walks_and_teleports_by_weight_unless_told_not_to() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b", "Task-c"]);
    write(&db, vec![link("Task-a", "weighs", "Task-b", Some(9.0)), link("Task-a", "weighs", "Task-c", Some(1.0))])
        .unwrap();
    let weighted = scores(&db, &["weighs"], json!({ "seeds": ["Task-a"] }));
    assert!(score(&weighted, "Task-b") > 8.0 * score(&weighted, "Task-c"), "{weighted:?}");
    let even = scores(&db, &["weighs"], json!({ "seeds": ["Task-a"], "weights": false }));
    assert_eq!(score(&even, "Task-b"), score(&even, "Task-c"));
    // Seeds weighted: b's walkers start there three times as often as c's, and an id is a seed of weight 1
    let seeded = scores(&db, &["weighs"], json!({ "seeds": [{ "id": "Task-b", "weight": 3 }, "Task-c"] }));
    assert!((score(&seeded, "Task-b") - 3.0 * score(&seeded, "Task-c")).abs() < 1e-12, "{seeded:?}");
    let twice = scores(&db, &["weighs"], json!({ "seeds": ["Task-b", "Task-b", "Task-b", "Task-c"] }));
    assert_eq!(seeded, twice, "a seed named again adds its weight");
    for bad in [json!(0), json!(-1)] {
        let e = query(
            &db,
            json!({ "start": { "from": "types", "types": ["Task"] },
                "steps": [{ "step": "rank", "kinds": ["weighs"], "seeds": [{ "id": "Task-a", "weight": bad }] }],
                "terminal": { "answer": "ids" } }),
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::BatchInvalid, "{bad}: {e:?}");
    }
    // A kind not weighted beside a weighted one weighs 1 on each edge
    write(&db, vec![link("Task-a", "refs", "Task-c", None)]).unwrap();
    let mixed = scores(&db, &["weighs", "refs"], json!({ "seeds": ["Task-a"] }));
    assert!((score(&mixed, "Task-b") / score(&mixed, "Task-c") - 9.0 / 2.0).abs() < 1e-9, "{mixed:?}");
}

#[test]
fn explain_reports_the_weight_a_rank_read() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b", "Task-c"]);
    write(&db, vec![link("Task-a", "weighs", "Task-b", Some(2.5)), link("Task-b", "weighs", "Task-c", None)]).unwrap();
    let explained = |options: Json| {
        let mut step = json!({ "step": "rank", "kinds": ["weighs"] });
        step.as_object_mut().unwrap().extend(options.as_object().unwrap().clone());
        read(
            &db,
            json!({ "read": "query", "explain": true, "plan": { "start": { "from": "types", "types": ["Task"] },
                "steps": [step], "terminal": { "answer": "ids" } } }),
        )
        .unwrap()["stages"]
            .clone()
    };
    for (options, weight) in [(json!({}), Some(json!(3.5))), (json!({ "weights": false }), None)] {
        let stages = explained(options);
        let stage = &stages[1];
        assert_eq!((&stage["nodes"], &stage["edges"]), (&json!(3), &json!(2)), "{stage}");
        assert_eq!(stage.get("weight"), weight.as_ref(), "{stage}");
        // Every field a rank's stage writes is in it, and the terminal's stage after it
        assert_eq!(stage["converged"], json!(true), "{stage}");
        assert!(stage["iterations"].as_u64().unwrap() > 0, "{stage}");
        assert_eq!(stages.as_array().unwrap().len(), 3, "{stages}");
        assert_eq!(stages[2]["stage"], json!("ids"), "{stages}");
    }
}

#[test]
fn communities_count_each_edge_by_its_weight_unless_told_not_to() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let names = ["Task-0", "Task-1", "Task-2", "Task-3"];
    tasks(&db, &names);
    // A square whose sides 0-3 and 1-2 are heavy
    let sides = [(0, 1, 1.0), (1, 2, 10.0), (2, 3, 1.0), (3, 0, 10.0)];
    write(&db, sides.iter().map(|&(a, b, w)| link(names[a], "weighs", names[b], Some(w))).collect()).unwrap();
    let communities = |weights: bool| {
        query(
            &db,
            json!({ "start": { "from": "types", "types": ["Task"] },
                "terminal": { "answer": "communities", "kinds": ["weighs"], "weights": weights } }),
        )
        .unwrap()["communities"]
            .clone()
    };
    assert_eq!(communities(true), json!([["Task-0", "Task-3"], ["Task-1", "Task-2"]]));
    let mut even = Projection::new();
    for v in 0..4u32 {
        even.node(sides.iter().filter(|s| s.0 == v as usize).map(|s| s.1 as u32));
    }
    let named: Vec<Vec<&str>> = algorithms::communities(&even, 1.0, 0, 100)
        .to_vecs()
        .into_iter()
        .map(|g| g.into_iter().map(|v| names[v as usize]).collect())
        .collect();
    assert_eq!(communities(false), json!(named));
}

fn weighs(weighted: bool) -> Schema {
    let mut schema = serde_json::to_value(super::schema()).unwrap();
    if !weighted {
        schema["relations"]["weighs"].as_object_mut().unwrap().remove("weighted");
    }
    Schema::from_json(&schema).unwrap()
}

#[test]
fn declaring_a_kind_weighted_or_not_is_applied_and_rewrites_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b", "Task-c"]);
    let (relation, _) = linked(&write(&db, vec![link("Task-a", "weighs", "Task-b", Some(9.0))]).unwrap());
    write(&db, vec![link("Task-a", "weighs", "Task-c", None)]).unwrap();
    let changes = db.set_schema(weighs(false)).unwrap();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!((changes[0].path.as_str(), changes[0].effect), ("relations.weighs", Effect::Applied));
    // The weight is kept, and nothing reads it: rows answer none, rank walks evenly and a new one is refused
    assert_eq!(stored(&db, &relation).weight, Some(9.0));
    assert!(read(&db, json!({ "read": "relation", "id": relation })).unwrap()["relation"].get("weight").is_none());
    let even = scores(&db, &["weighs"], json!({ "seeds": ["Task-a"] }));
    assert_eq!(score(&even, "Task-b"), score(&even, "Task-c"));
    assert_eq!(refused(&db, vec![link("Task-b", "weighs", "Task-c", Some(2.0))]).code, ErrorCode::ValueInvalid);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    // Declared again, the kept weight counts once more
    assert_eq!(db.set_schema(weighs(true)).unwrap()[0].effect, Effect::Applied);
    let weighted = scores(&db, &["weighs"], json!({ "seeds": ["Task-a"] }));
    assert!(score(&weighted, "Task-b") > 8.0 * score(&weighted, "Task-c"));
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn check_reports_an_edge_entry_whose_weight_isnt_its_rows_and_a_loaded_weight_a_write_would_refuse() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b"]);
    let (relation, _) = linked(&write(&db, vec![link("Task-a", "weighs", "Task-b", Some(2.0))]).unwrap());
    let seq = stored(&db, &relation).seq;
    db.store()
        .unwrap()
        .write(
            |txn, d| {
                d.put(
                    txn,
                    &keys::edge(true, "Task-a", "weighs", seq),
                    &relations::edge_value(&relation, "Task-b", Some(3.0)),
                )?;
                Ok(())
            },
            false,
        )
        .unwrap();
    let report = db.check().unwrap();
    assert_eq!(report.differences.len(), 1, "{report:?}");
    assert_eq!(report.differences[0].kind, "differs");
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean());

    db.bulk_load(&[Load::Relation {
        id: "Relation-loaded".into(),
        kind: "weighs".into(),
        source: "Task-b".into(),
        target: "Task-a".into(),
        info: None,
        weight: Some(-1.0),
    }])
    .unwrap();
    let report = db.check().unwrap();
    assert!(report.differences.is_empty(), "{report:?}");
    assert_eq!(report.violations.len(), 1, "{report:?}");
    assert!(report.violations[0].subject.ends_with("Relation-loaded"), "{report:?}");
    assert!(report.violations[0].message.contains("-1"), "{report:?}");
}

#[test]
fn a_dump_carries_each_weight_and_a_tracked_rank_is_written_into_by_a_new_one() {
    let work = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b"]);
    let (relation, _) = linked(&write(&db, vec![link("Task-a", "weighs", "Task-b", Some(2.0))]).unwrap());
    let path = work.path().join("db.dump");
    crate::dump::dump(&db, &path).unwrap();
    let target = work.path().join("loaded");
    crate::dump::load(&path, &target).unwrap();
    let loaded = Database::open(&target, OpenOptions { syncs: false, ..Default::default() }).unwrap();
    assert_eq!(stored(&loaded, &relation).weight, Some(2.0));
    assert!(loaded.check().unwrap().is_clean());

    let rank = json!({ "read": "query", "plan": { "start": { "from": "types", "types": ["Task"] },
        "steps": [{ "step": "rank", "kinds": ["weighs"] }], "terminal": { "answer": "ids" } } });
    db.read(&Reads { reads: vec![serde_json::from_value(rank).unwrap()], track: Some(7) }).unwrap();
    let touched = db
        .write(&batch(vec![json!({ "op": "updateRelation", "relation": relation, "weight": 3 })]), false)
        .unwrap()
        .1
        .touched;
    assert_eq!(touched, [7]);
}

const NODES: usize = 6;

fn name(n: usize) -> String {
    format!("Task-{n}")
}

/// Random weighted edges among the tasks, of `weighs` or of `refs`, which weighs 1 on every edge
fn weighted_edges() -> impl Strategy<Value = Vec<(usize, bool, usize, f64)>> {
    let weight = prop_oneof![Just(0.0), Just(1.0), (1u32..=12).prop_map(|q| f64::from(q) / 4.0)];
    prop::collection::vec((0..NODES, any::<bool>(), 0..NODES, weight), 0..=16)
        .prop_map(|edges| edges.into_iter().filter(|e| e.0 != e.2).collect())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn the_engine_ranks_the_weighted_graph_the_edges_describe(edges in weighted_edges(), seed in 0..NODES) {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let names: Vec<String> = (0..NODES).map(name).collect();
        tasks(&db, &names.iter().map(String::as_str).collect::<Vec<_>>());
        // Each edge as written, once: a second link of the same weight is the edge already there
        let mut held: Vec<(usize, usize, f64)> = Vec::new();
        for &(s, weighted, t, w) in &edges {
            let (kind, weight) = if weighted { ("weighs", Some(w)) } else { ("refs", None) };
            let (_, made) = linked(&write(&db, vec![link(&name(s), kind, &name(t), weight)]).unwrap());
            if made {
                held.push((s, t, if weighted { w } else { 1.0 }));
            }
        }
        let refs: Vec<(usize, usize)> = edges.iter().filter(|e| !e.1).map(|e| (e.0, e.2)).collect();
        // The engine reads a node's edges kind by kind, `weighs` before `refs`, each in link order
        let mut graph = Projection::weighted();
        for v in 0..NODES {
            let mut out: Vec<(u32, f64)> = Vec::new();
            let weighs = edges.iter().filter(|e| e.1 && e.0 == v);
            let mut seen: Vec<(usize, f64)> = Vec::new();
            for &(_, _, t, w) in weighs {
                if !seen.contains(&(t, w)) {
                    seen.push((t, w));
                    out.push((t as u32, w));
                }
            }
            let mut targets: Vec<usize> = Vec::new();
            for &(_, t) in refs.iter().filter(|e| e.0 == v) {
                if !targets.contains(&t) {
                    targets.push(t);
                    out.push((t as u32, 1.0));
                }
            }
            graph.weighted_node(out);
        }
        prop_assert_eq!(graph.edges(), held.len());
        let expected = algorithms::rank(&graph, &[(seed as u32, 1.0)], 0.85, 100, 1e-6).unwrap();
        let found = scores(&db, &["weighs", "refs"], json!({ "seeds": [name(seed)] }));
        for (id, got) in &found {
            let n: usize = id.trim_start_matches("Task-").parse().unwrap();
            prop_assert_eq!(*got, expected.scores[n], "{}", id);
        }
    }
}
