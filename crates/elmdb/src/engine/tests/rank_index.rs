//! Rank indexes (`docs/archive/roadmap/rank-index.md`): scores kept current by every commit, within the bound they report of
//! the PageRank `rank` computes over the same graph; read by `ranked`, the top of them read alone; computed while
//! building; held to that bound and to their own invariant by `check()`.

use proptest::prelude::*;
use serde_json::{Value as Json, json};

use super::rank_bound::{RankIndex, read};
use crate::engine::plan::{ReadOp, Reads};
use crate::engine::unit::Read as _;
use crate::engine::{keys, ranks};
use crate::error::ErrorCode;
use crate::schema::Schema;
use crate::{Database, OpenOptions};

fn schema(direction: &str, weights: bool) -> Schema {
    Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "relations": {
            "refs": { "cardinality": "many-to-many" },
            "weighs": { "cardinality": "many-to-many", "weighted": true, "key": ["n"] },
            "rates": { "cardinality": "many-to-many", "weighted": true },
            "other": { "cardinality": "many-to-many" }
        },
        "types": { "Task": { "fields": {} }, "Note": { "fields": {} }, "Tag": { "fields": {} } },
        "ranks": {
            "taskRank": { "types": ["Task", "Note"], "relations": ["refs", "weighs", "rates"],
                          "direction": direction, "weights": weights, "tolerance": 1e-9 }
        }
    }))
    .unwrap()
}

fn open_with(dir: &tempfile::TempDir, schema: Schema) -> Database {
    Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap()
}

fn write(db: &Database, ops: Vec<Json>) -> crate::Result<()> {
    db.write(&super::batch(ops), false).map(|_| ())
}

/// `taskRank` against `rank` in a direction, with or without weights
fn task_rank(direction: &str, weights: bool) -> RankIndex<'_> {
    RankIndex { name: "taskRank", types: &["Task", "Note"], kinds: &["refs", "weighs", "rates"], direction, weights }
}

/// The index's scores and bound, after its pushes are done
fn indexed(db: &Database) -> (Vec<(String, f64)>, f64) {
    task_rank("out", true).indexed(db)
}

/// The index within its bound of `rank`, and `check()` clean
fn assert_within_bound(db: &Database, direction: &str, weights: bool) {
    task_rank(direction, weights).assert_within_bound(db);
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn the_index_scores_within_its_bound_of_rank_as_the_graph_changes() {
    for (direction, weights) in [("out", true), ("in", true), ("both", true), ("out", false)] {
        let dir = tempfile::tempdir().unwrap();
        let db = open_with(&dir, schema(direction, weights));
        let ids: Vec<String> = (0..6).map(|i| format!("Task-{i}")).chain(["Note-a".into()]).collect();
        write(
            &db,
            ids.iter().map(|id| json!({ "op": "create", "type": id.split('-').next().unwrap(), "id": id })).collect(),
        )
        .unwrap();
        assert_within_bound(&db, direction, weights);
        let link = |s: &str, k: &str, t: &str, w: Option<f64>| {
            let mut op = json!({ "op": "link", "source": s, "kind": k, "target": t });
            if let Some(w) = w {
                op["weight"] = json!(w);
            }
            op
        };
        write(
            &db,
            vec![
                link("Task-0", "refs", "Task-1", None),
                link("Task-1", "weighs", "Task-2", Some(3.0)),
                link("Task-2", "refs", "Task-0", None),
                link("Task-3", "weighs", "Task-0", Some(0.5)),
                link("Note-a", "rates", "Task-0", Some(7.0)),
                link("Task-4", "weighs", "Task-5", Some(0.0)),
            ],
        )
        .unwrap();
        assert_within_bound(&db, direction, weights);
        // A parallel edge, which the key tells apart, a weight replaced, an edge removed
        let mut parallel = link("Task-1", "weighs", "Task-2", Some(1.5));
        parallel["info"] = json!({ "n": 2 });
        write(&db, vec![parallel, link("Note-a", "rates", "Task-0", Some(2.0))]).unwrap();
        write(&db, vec![json!({ "op": "unlink", "source": "Task-2", "kind": "refs", "target": "Task-0" })]).unwrap();
        assert_within_bound(&db, direction, weights);
        // A node leaving with its edges, and coming back with them
        write(&db, vec![json!({ "op": "trash", "id": "Task-0" })]).unwrap();
        assert_within_bound(&db, direction, weights);
        write(&db, vec![json!({ "op": "restore", "id": "Task-0" })]).unwrap();
        assert_within_bound(&db, direction, weights);
        write(&db, vec![json!({ "op": "destroy", "id": "Task-1" })]).unwrap();
        assert_within_bound(&db, direction, weights);
    }
}

#[test]
fn ranked_reads_the_top_alone_and_refuses_what_it_cannot_answer() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    let mut ops: Vec<Json> =
        // Enough that the third's score is too small a share for its cell to be wider than the order promises
        (0..500).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend(
        (1..500).map(|i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": "Task-0" })),
    );
    ops.extend(
        (2..500).map(|i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": "Task-1" })),
    );
    write(&db, ops).unwrap();
    write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-a" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    let ranked = |limit: Option<u32>, explain: bool| {
        let mut steps = vec![json!({ "step": "ranked", "index": "taskRank" })];
        if let Some(n) = limit {
            steps.push(json!({ "step": "limit", "n": n }));
        }
        let reads = Reads {
            reads: vec![serde_json::from_value::<ReadOp>(json!({ "read": "query", "explain": explain, "plan": {
                "start": { "from": "types", "types": ["Task", "Note"] }, "steps": steps, "terminal": { "answer": "ids" } } }))
            .unwrap()],
            track: None,
        };
        let bytes = db.read(&reads).unwrap();
        serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()[0].clone()
    };
    let all = ranked(None, false)["ids"].as_array().unwrap().clone();
    assert_eq!(&all[..2], &[json!("Task-0"), json!("Task-1")]);
    assert_eq!(ranked(Some(3), false)["ids"].as_array().unwrap(), &all[..3]);
    let explained = ranked(Some(3), true);
    assert_eq!(explained["visited"], json!(3), "read three of the index's keys and nothing else: {explained}");
    assert_eq!(explained["stages"][1]["index"], json!("taskRank"));
    assert!(explained["stages"][1]["error"].as_f64().unwrap() <= 1e-9);
    // Ranked over a set the index doesn't hold entirely: a type it doesn't cover is refused
    let refused = read(
        &db,
        json!({ "start": { "from": "ids", "ids": ["Task-0", "Task-9999"] },
        "steps": [{ "step": "ranked", "index": "taskRank" }], "terminal": { "answer": "ids" } }),
    );
    assert_eq!(refused.unwrap()["ids"], json!(["Task-0"]), "an id no entity has is no member of the set");
    write(&db, vec![json!({ "op": "trash", "id": "Task-5" })]).unwrap();
    let e = read(
        &db,
        json!({ "scope": "withTrashed", "start": { "from": "ids", "ids": ["Task-5"] },
        "steps": [{ "step": "ranked", "index": "taskRank" }], "terminal": { "answer": "ids" } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::BatchInvalid, "{e:?}");
    let e = read(
        &db,
        json!({ "start": { "from": "types", "types": ["Task"] },
        "steps": [{ "step": "ranked", "index": "nope" }], "terminal": { "answer": "ids" } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::UnknownIndex);
}

#[test]
fn an_index_not_built_yet_answers_as_it_will_once_built_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    let mut ops: Vec<Json> =
        (0..40).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend(
        (1..40).map(|i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": "Task-0" })),
    );
    ops.extend(
        (2..20).map(|i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": "Task-1" })),
    );
    ops.push(json!({ "op": "create", "type": "Tag", "id": "Tag-0" }));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let plan = |steps: Json, answer: Json| json!({ "start": { "from": "types", "types": ["Task", "Note"] }, "steps": steps, "terminal": answer });
    let top =
        |answer: Json| plan(json!([{ "step": "ranked", "index": "taskRank" }, { "step": "limit", "n": 3 }]), answer);
    let all = plan(json!([{ "step": "ranked", "index": "taskRank" }]), json!({ "answer": "hits" }));
    let scores = |answer: &Json| -> Vec<(String, f64)> {
        answer["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| (h["id"].as_str().unwrap().to_owned(), h["score"].as_f64().unwrap()))
            .collect()
    };
    let built = scores(&read(&db, all.clone()).unwrap());
    let built_top = read(&db, top(json!({ "answer": "ids" }))).unwrap();

    // As a build leaves it: the totals say nothing is built
    db.store()
        .unwrap()
        .write(|txn, d| d.delete(txn, &keys::rank_totals("taskRank")).map(|_| ()).map_err(Into::into), false)
        .unwrap();

    let building = scores(&read(&db, all).unwrap());
    assert_eq!(building.len(), built.len());
    let tolerance = 1e-9;
    let distance: f64 = {
        let held: std::collections::HashMap<_, _> = built.iter().cloned().collect();
        building.iter().map(|(id, score)| (score - held[id]).abs()).sum()
    };
    assert!(distance < 4.0 * tolerance, "{distance} from the built scores");
    assert_eq!(read(&db, top(json!({ "answer": "ids" }))).unwrap(), built_top);

    // EXPLAIN tells the slower route from the index
    let asked: Reads = serde_json::from_value(
        json!({ "reads": [{ "read": "query", "explain": true, "plan": top(json!({ "answer": "ids" })) }] }),
    )
    .unwrap();
    let explained: Json =
        serde_json::to_value(rmpv::decode::read_value(&mut &db.read(&asked).unwrap()[..]).unwrap()).unwrap();
    let stage = explained[0]["stages"].as_array().unwrap().iter().find(|s| s["stage"] == "ranked").unwrap().clone();
    assert_eq!(stage["index"], "taskRank");
    assert_eq!(stage["building"], true);
    assert!(stage["error"].as_f64().unwrap() >= tolerance);

    // A read that asked not to compute them is refused, and one whose index is built is answered regardless
    let refusing = |steps: Json| plan(steps, json!({ "answer": "ids" }));
    let asks = json!({ "step": "ranked", "index": "taskRank", "whileBuilding": "refuse" });
    assert_eq!(read(&db, refusing(json!([asks.clone()]))).unwrap_err().code, ErrorCode::IndexBuilding);
    assert_eq!(
        read(&db, refusing(json!([asks, { "step": "limit", "n": 3 }]))).unwrap_err().code,
        ErrorCode::IndexBuilding
    );

    // What it refuses it refuses the same: a set outside the graph, and a name nothing declares
    let outside = json!({ "start": { "from": "types", "types": ["Tag"] },
        "steps": [{ "step": "ranked", "index": "taskRank" }], "terminal": { "answer": "ids" } });
    assert_eq!(read(&db, outside).unwrap_err().code, ErrorCode::BatchInvalid);
    assert_eq!(
        read(&db, plan(json!([{ "step": "ranked", "index": "nothing" }]), json!({ "answer": "ids" })))
            .unwrap_err()
            .code,
        ErrorCode::UnknownIndex
    );
}

#[test]
fn check_holds_the_index_to_its_bound_its_residuals_and_its_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    let mut ops: Vec<Json> =
        (0..8).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((1..8).map(
        |i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": format!("Task-{}", i - 1) }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(db.check().unwrap().is_clean());
    let damage = |f: &dyn Fn(&mut ranks::Node)| {
        db.store()
            .unwrap()
            .write(
                |txn, d| {
                    let key = keys::rank_node("taskRank", "Task-3");
                    let bytes = d.main.get(txn, &key)?.unwrap().to_vec();
                    let mut node = ranks::Node::decode(&bytes)?;
                    f(&mut node);
                    d.main.put(txn, &key, &node.encode())?;
                    Ok(())
                },
                false,
            )
            .unwrap();
    };
    let subjects = || db.check().unwrap().violations.into_iter().map(|v| v.message).collect::<Vec<_>>();
    // An estimate moved: the residual no longer what the estimates make it, the score order no longer the node's
    damage(&|n| n.x *= 1.5);
    let said = subjects();
    assert!(said.iter().any(|m| m.contains("residual")), "{said:?}");
    assert!(said.iter().any(|m| m.contains("score order")), "{said:?}");
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    // A residual moved alone
    damage(&|n| n.r += 0.01);
    assert!(subjects().iter().any(|m| m.contains("residual")));
    db.rebuild(None).unwrap();
    // Totals the nodes don't add up to: a read would divide by them, and report a bound from them
    db.store()
        .unwrap()
        .write(
            |txn, d| {
                let key = keys::rank_totals("taskRank");
                let mut bytes = d.main.get(txn, &key)?.unwrap().to_vec();
                bytes[8..16].copy_from_slice(&1.0f64.to_le_bytes());
                d.main.put(txn, &key, &bytes)?;
                Ok(())
            },
            false,
        )
        .unwrap();
    assert!(subjects().iter().any(|m| m.contains("totals")));
    // Every state the invariant holds in is within the bound it reports, the bound's own proof; so `check()`'s
    // comparison with `rank` is what a wrong bound or a wrong system fails, which the mutation tests show
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn the_index_builds_over_existing_data_and_rebuilds_when_its_graph_changes() {
    let dir = tempfile::tempdir().unwrap();
    let without = {
        let mut s = serde_json::to_value(schema("out", true)).unwrap();
        s["ranks"] = json!({});
        Schema::from_json(&s).unwrap()
    };
    let db = open_with(&dir, without.clone());
    let mut ops: Vec<Json> =
        (0..2500).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((1..2500).map(|i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "weighs", "target": format!("Task-{}", i / 2), "weight": (i % 3 + 1) as f64 })));
    write(&db, ops).unwrap();
    let changes = db.set_schema(schema("out", true)).unwrap();
    assert!(
        changes.iter().any(|c| c.path == "ranks.taskRank" && c.effect == crate::schema::Effect::Builds),
        "{changes:?}"
    );
    assert_within_bound(&db, "out", true);
    // A kind's weights changing changes the graph: the index rebuilds
    let mut unweighted = serde_json::to_value(schema("out", true)).unwrap();
    unweighted["relations"]["weighs"]["weighted"] = json!(false);
    let changes = db.set_schema(Schema::from_json(&unweighted).unwrap()).unwrap();
    assert!(
        changes.iter().any(|c| c.path == "ranks.taskRank" && c.effect == crate::schema::Effect::Rebuilds),
        "{changes:?}"
    );
    db.wait_for_index_builds().unwrap();
    assert!(db.check().unwrap().is_clean());
    // Removed, its data goes
    db.set_schema(without).unwrap();
    let left = db
        .store()
        .unwrap()
        .read(|txn, d| {
            let mut n = 0;
            for prefix in [keys::rank_nodes("taskRank"), keys::rank_orders("taskRank"), keys::rank_queue("taskRank")] {
                let (low, high) = crate::format::key::prefix_range(&prefix);
                let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
                n += d.main.range(txn, &range)?.count();
            }
            Ok(n)
        })
        .unwrap();
    assert_eq!(left, 0);
}

const NODES: usize = 6;

#[derive(Debug, Clone)]
enum Op {
    Link(usize, usize, usize, u8),
    Unlink(usize, usize, usize),
    Trash(usize),
    Restore(usize),
    Destroy(usize),
    Create(usize),
}

fn op() -> impl Strategy<Value = Op> {
    let n = 0..NODES;
    prop_oneof![
        6 => (n.clone(), 0..3usize, n.clone(), 0..5u8).prop_map(|(s, k, t, w)| Op::Link(s, k, t, w)),
        2 => (n.clone(), 0..3usize, n.clone()).prop_map(|(s, k, t)| Op::Unlink(s, k, t)),
        1 => n.clone().prop_map(Op::Trash),
        1 => n.clone().prop_map(Op::Restore),
        1 => n.clone().prop_map(Op::Destroy),
        1 => n.prop_map(Op::Create),
    ]
}

fn json_of(op: &Op) -> Json {
    const KINDS: [&str; 3] = ["refs", "weighs", "rates"];
    let id = |n: usize| format!("Task-{n}");
    match *op {
        Op::Link(s, k, t, w) => {
            let mut op = json!({ "op": "link", "source": id(s), "kind": KINDS[k], "target": id(t) });
            if k > 0 {
                op["weight"] = json!(f64::from(w) / 2.0);
            }
            if k == 1 {
                // Two key values, so a pair is joined by parallel edges as often as one edge's weight is replaced
                op["info"] = json!({ "n": w % 2 });
            }
            op
        }
        Op::Unlink(s, k, t) => json!({ "op": "unlink", "source": id(s), "kind": KINDS[k], "target": id(t) }),
        Op::Trash(n) => json!({ "op": "trash", "id": id(n) }),
        Op::Restore(n) => json!({ "op": "restore", "id": id(n) }),
        Op::Destroy(n) => json!({ "op": "destroy", "id": id(n) }),
        Op::Create(n) => json!({ "op": "create", "type": "Task", "id": id(n) }),
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn any_history_keeps_the_index_within_its_bound_of_rank(
        ops in prop::collection::vec(op(), 1..30),
        direction in prop_oneof![Just("out"), Just("in"), Just("both")],
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = open_with(&dir, schema(direction, true));
        write(&db, (0..NODES).map(|n| json!({ "op": "create", "type": "Task", "id": format!("Task-{n}") })).collect()).unwrap();
        for op in &ops {
            // A refused write (a self link, an entity gone) changes nothing
            let _ = write(&db, vec![json_of(op)]);
            assert_within_bound(&db, direction, true);
        }
    }
}

/// `schema("out", true)` at a tolerance
fn at_tolerance(tolerance: f64) -> Schema {
    let mut at = serde_json::to_value(schema("out", true)).unwrap();
    at["ranks"]["taskRank"]["tolerance"] = json!(tolerance);
    Schema::from_json(&at).unwrap()
}

/// The ids a read of the ranked set answers, in its order, `limit` of them or all; what it answers as `hits` or `ids`
fn ranked_ids(db: &Database, limit: Option<usize>, answer: &str) -> Vec<String> {
    let mut steps = vec![json!({ "step": "ranked", "index": "taskRank" })];
    if let Some(n) = limit {
        steps.push(json!({ "step": "limit", "n": n }));
    }
    let start = json!({ "from": "types", "types": ["Task", "Note"] });
    let read = read(db, json!({ "start": start, "steps": steps, "terminal": { "answer": answer } })).unwrap();
    if answer == "hits" {
        read["hits"].as_array().unwrap().iter().map(|h| h["id"].as_str().unwrap().to_owned()).collect()
    } else {
        read["ids"].as_array().unwrap().iter().map(|id| id.as_str().unwrap().to_owned()).collect()
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, failure_persistence: None, ..ProptestConfig::default() })]

    /// The index's half of the promise of `ranks::order_key`, at tolerances that keep few bits and many: a top read answers
    /// no node above one scoring further than twice the tolerance above it, and leaves out no such node. Against PageRank
    /// is `top_reads_stay_within_the_order_promised_against_rank`, and up against the margin
    /// `a_top_read_takes_a_narrow_cell_by_place_with_the_margin_the_order_promises`
    #[test]
    fn a_top_read_holds_the_order_promised_at_any_tolerance(
        ops in prop::collection::vec(op(), 1..30),
        tolerance in prop_oneof![Just(1e-2), Just(1e-4), Just(1e-6)],
        take in 1usize..8,
        answer in prop_oneof![Just("hits"), Just("ids")],
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = open_with(&dir, at_tolerance(tolerance));
        write(&db, (0..NODES).map(|n| json!({ "op": "create", "type": "Task", "id": format!("Task-{n}") })).collect()).unwrap();
        for op in &ops {
            let _ = write(&db, vec![json_of(op)]);
        }
        db.wait_for_index_builds().unwrap();
        let (scores, _) = indexed(&db);
        let score = |id: &str| scores.iter().find(|(held, _)| held == id).unwrap().1;
        let promised = 2.0 * tolerance;
        let top = ranked_ids(&db, Some(take), answer);
        prop_assert_eq!(top.len(), take.min(scores.len()));
        for (at, id) in top.iter().enumerate() {
            for later in &top[at + 1..] {
                prop_assert!(score(later) - score(id) <= promised, "{later} above {id}: {scores:?}");
            }
            for (other, held) in scores.iter().filter(|(other, _)| !top.contains(other)) {
                prop_assert!(held - score(id) <= promised, "{other} left out for {id}: {scores:?}");
            }
        }
    }
}

/// `Task-0` and `Task-1` scored apart by most of a cell of the index's order, `Task-1` created after and scoring higher, among
/// enough nodes that the cell is no wider than the order promises
fn two_hubs_in_one_narrow_cell(dir: &tempfile::TempDir) -> Database {
    let db = open_with(dir, at_tolerance(1e-5));
    let task = |i: usize| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") });
    let refs = |from: usize, to: usize| json!({ "op": "link", "source": format!("Task-{from}"), "kind": "refs", "target": format!("Task-{to}") });
    let mut ops: Vec<Json> = (0..900).map(task).collect();
    ops.extend((2..11).map(|leaf| refs(leaf, 0)));
    ops.extend((11..20).map(|leaf| refs(leaf, 1)));
    // One leaf more for the second hub, a share of it, the rest of that leaf's spread over nodes that stay low
    ops.push(refs(20, 1));
    ops.extend((21..125).map(|dud| refs(20, dud)));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    db
}

#[test]
fn a_top_read_takes_a_narrow_cell_by_place_with_the_margin_the_order_promises() {
    let tolerance = 1e-5;
    let dir = tempfile::tempdir().unwrap();
    let db = two_hubs_in_one_narrow_cell(&dir);
    let (estimates, x0, x1) = db
        .snapshot(|s| {
            Ok((
                ranks::totals(s, "taskRank")?.estimates,
                ranks::node(s, "taskRank", "Task-0")?.unwrap().x,
                ranks::node(s, "taskRank", "Task-1")?.unwrap().x,
            ))
        })
        .unwrap();
    let precision = ranks::order_precision(tolerance);
    // What the case is of: the higher score placed second inside one cell the order keeps by place, and apart by enough
    // that a margin of half the promise would be false
    assert!(x1 > x0, "{x0} {x1}");
    assert!(ranks::same_cell(x0, x1, precision), "{x0} {x1}");
    assert!(!ranks::cell_wider_than(x0, precision, 2.0 * tolerance * estimates), "{x0} {estimates}");
    let gap = (x1 - x0) / estimates;
    assert!(gap > tolerance / 2.0, "the case is of a gap near the margin, not {gap}");
    assert!(gap <= 2.0 * tolerance, "{gap}");
    // A cell kept by place: taking one takes the lower, whether or not scores are shown, which the promise allows
    for answer in ["hits", "ids"] {
        assert_eq!(ranked_ids(&db, Some(1), answer), ["Task-0"], "{answer}");
    }
    // Showing scores sorts what was read, and ids keep their place
    assert_eq!(ranked_ids(&db, Some(2), "hits"), ["Task-1", "Task-0"]);
    assert_eq!(ranked_ids(&db, Some(2), "ids"), ["Task-0", "Task-1"]);
    // And against PageRank: truly in the other order, by less than the margin promised and more than half the tolerance
    let rank = task_rank("out", true);
    let (_, bound) = rank.indexed(&db);
    let truth = rank.computed(&db);
    let of = |id: &str| truth.iter().find(|(held, _)| held == id).unwrap().1;
    let inverted = of("Task-1") - of("Task-0");
    assert!(inverted > tolerance / 2.0, "{inverted}");
    assert!(inverted <= 2.0 * tolerance + bound, "{inverted} past {bound}");
}

#[test]
fn top_reads_stay_within_the_order_promised_against_rank() {
    use proptest::test_runner::{Config, TestRunner};
    const TASKS: usize = 300;
    // Reads of every kind over a graph of varied scores. Near its top cells are wide, and read exactly, so below 1e-2 this
    // holds only that those stay so; `a_top_read_takes_a_narrow_cell_by_place…` is what comes up against the margin
    let rank = task_rank("out", true);
    let strategy = (
        prop::collection::vec(op(), 0..12),
        prop_oneof![Just(1e-2), Just(1e-4), Just(1e-6)],
        1usize..20,
        prop_oneof![Just("hits"), Just("ids")],
    );
    TestRunner::new(Config { cases: 12, failure_persistence: None, ..Config::default() })
        .run(&strategy, |(ops, tolerance, take, answer)| {
            let dir = tempfile::tempdir().unwrap();
            let db = open_with(&dir, at_tolerance(tolerance));
            let mut graph: Vec<Json> =
                (0..TASKS).map(|n| json!({ "op": "create", "type": "Task", "id": format!("Task-{n}") })).collect();
            graph.extend(
                (0..TASKS)
                    .flat_map(|i| [(i + 1) % TASKS, i * i % TASKS].map(|t| (i, t)))
                    .filter(|(i, t)| i != t)
                    .map(|(i, t)| {
                        json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": format!("Task-{t}") })
                    }),
            );
            write(&db, graph).unwrap();
            for op in &ops {
                let _ = write(&db, vec![json_of(op)]);
            }
            db.wait_for_index_builds().unwrap();
            let (_, bound) = rank.indexed(&db);
            let truth = rank.computed(&db);
            let of = |id: &str| truth.iter().find(|(held, _)| held == id).unwrap().1;
            let promised = 2.0 * tolerance + bound;
            let top = ranked_ids(&db, Some(take), answer);
            prop_assert_eq!(top.len(), take.min(truth.len()));
            for (at, id) in top.iter().enumerate() {
                for later in &top[at + 1..] {
                    prop_assert!(of(later) - of(id) <= promised, "{later} above {id} at {tolerance}");
                }
                for (other, held) in truth.iter().filter(|(other, _)| !top.contains(other)) {
                    prop_assert!(held - of(id) <= promised, "{other} left out for {id} at {tolerance}");
                }
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_cell_spans_the_same_share_of_a_score_on_every_platform() {
    // log2(1 / (256 × 1e-6)) is 11.93, and past a tolerance of about 2.4e-4 the bits are held at 4
    for (tolerance, bits) in [(1e-9, 22), (1e-6, 12), (1e-4, 6), (2.5e-4, 4), (1e-2, 4), (1e-300, 52)] {
        assert_eq!(ranks::order_precision(tolerance), bits, "at {tolerance}");
    }
    // Exactly a power of two, where a platform's log2 rounding one way or the other would change the bits
    assert_eq!(ranks::order_precision(1.0 / (256.0 * 1024.0)), 10);
}

/// `Task-0` and `Task-1` scored apart by less than a cell of the index's order, `Task-1` created after and scoring higher,
/// in a graph of few enough nodes that the cell is wider than the order promises
fn two_hubs_in_one_wide_cell(dir: &tempfile::TempDir) -> Database {
    let db = open_with(dir, at_tolerance(1e-5));
    let task = |i: usize| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") });
    let refs = |from: usize, to: usize| json!({ "op": "link", "source": format!("Task-{from}"), "kind": "refs", "target": format!("Task-{to}") });
    let mut ops: Vec<Json> = (0..106).map(task).collect();
    ops.extend((2..22).map(|leaf| refs(leaf, 0)));
    ops.extend((22..42).map(|leaf| refs(leaf, 1)));
    // One leaf more for the second hub, a sixty-fourth of it, its rest spread over nodes that stay low
    ops.push(refs(42, 1));
    ops.extend((43..106).map(|dud| refs(42, dud)));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    db
}

#[test]
fn a_top_read_reads_through_a_cell_wider_than_the_order_promises() {
    let dir = tempfile::tempdir().unwrap();
    let db = two_hubs_in_one_wide_cell(&dir);
    // What the case is of: the higher score placed second by the order, inside one cell, wider than promised
    let schema = db.schema().unwrap();
    let (estimates, x0, x1) = db
        .store()
        .unwrap()
        .read(|txn, d| {
            let snapshot = crate::engine::unit::Snapshot { txn, db: d, schema: &schema };
            Ok((
                ranks::totals(&snapshot, "taskRank")?.estimates,
                ranks::node(&snapshot, "taskRank", "Task-0")?.unwrap().x,
                ranks::node(&snapshot, "taskRank", "Task-1")?.unwrap().x,
            ))
        })
        .unwrap();
    let precision = ranks::order_precision(1e-5);
    assert!(x1 > x0, "{x0} {x1}");
    assert!(ranks::same_cell(x0, x1, precision), "{x0} {x1}");
    assert!(ranks::cell_wider_than(x0, precision, 2.0 * 1e-5 * estimates), "{x0} {estimates}");
    // Taking one straddles the cell, and the second hub is the answer whether or not scores are shown
    for answer in ["hits", "ids"] {
        assert_eq!(ranked_ids(&db, Some(1), answer), ["Task-1"], "{answer}");
        assert_eq!(ranked_ids(&db, Some(2), answer), ["Task-1", "Task-0"], "{answer}");
    }
}

#[test]
fn a_top_read_through_narrow_cells_reads_only_its_limit_and_scores_alike_either_way() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, at_tolerance(1e-6));
    // A ring: scores alike, each too small a share of their sum for a cell to be wider than the order promises
    let mut ops: Vec<Json> =
        (0..300).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((0..300).map(|i| {
        json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": format!("Task-{}", (i + 1) % 300) })
    }));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let plan = |answer: &str, explain: bool| Reads {
        reads: vec![
            serde_json::from_value::<ReadOp>(json!({ "read": "query", "explain": explain, "plan": {
                "start": { "from": "types", "types": ["Task", "Note"] },
                "steps": [{ "step": "ranked", "index": "taskRank" }, { "step": "limit", "n": 3 }],
                "terminal": { "answer": answer } } }))
            .unwrap(),
        ],
        track: None,
    };
    let answer = |reads: &Reads| {
        let bytes = db.read(reads).unwrap();
        serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()[0].clone()
    };
    let explained = answer(&plan("ids", true));
    assert_eq!(explained["visited"], json!(3), "three of the order's keys and no more: {explained}");
    // Scores shown are the nodes' own, the same as ranking the whole set
    let (scores, _) = indexed(&db);
    let hits = answer(&plan("hits", false));
    let hits = hits["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 3);
    for hit in hits {
        let held = scores.iter().find(|(id, _)| id == hit["id"].as_str().unwrap()).unwrap().1;
        assert_eq!(hit["score"].as_f64().unwrap(), held, "{hit}");
    }
}

#[test]
fn check_holds_the_order_to_the_bits_the_tolerance_calls_for() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, at_tolerance(1e-6));
    let mut ops: Vec<Json> =
        (0..8).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((1..8).map(
        |i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": format!("Task-{}", i - 1) }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(db.check().unwrap().is_clean());
    // One node's entry written with more bits than this index's tolerance keeps
    let kept = |x: f64, bits: u32| f64::from_bits(x.to_bits() & !((1u64 << (52 - bits)) - 1));
    db.store()
        .unwrap()
        .write(
            |txn, d| {
                let bytes = d.main.get(txn, &keys::rank_node("taskRank", "Task-3"))?.unwrap().to_vec();
                let node = ranks::Node::decode(&bytes)?;
                let precision = ranks::order_precision(1e-6);
                let held = keys::rank_order("taskRank", kept(node.x, precision), node.type_at, node.seq);
                let finer = keys::rank_order("taskRank", kept(node.x, precision + 8), node.type_at, node.seq);
                assert_ne!(held, finer, "{}", node.x);
                assert!(d.main.delete(txn, &held)?);
                d.main.put(txn, &finer, b"Task-3")?;
                Ok(())
            },
            false,
        )
        .unwrap();
    let said = db.check().unwrap().violations.into_iter().map(|v| v.message).collect::<Vec<_>>();
    assert!(said.iter().any(|m| m.contains("score order")), "{said:?}");
}

#[test]
fn a_watch_over_the_top_is_delivered_only_what_changes_its_answer() {
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, at_tolerance(1e-6));
    let mut ops: Vec<Json> =
        (0..200).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((1..200).map(
        |i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": format!("Task-{}", i / 2) }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let top = Reads {
        reads: vec![
            serde_json::from_value::<ReadOp>(json!({ "read": "query", "plan": {
                "start": { "from": "types", "types": ["Task", "Note"] },
                "steps": [{ "step": "ranked", "index": "taskRank" }, { "step": "limit", "n": 3 }],
                "terminal": { "answer": "ids" } } }))
            .unwrap(),
        ],
        track: None,
    };
    let heard: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let into = Arc::clone(&heard);
    db.watch(top, Box::new(move |answer| into.lock().unwrap().push(answer.as_ref().unwrap().to_vec()))).unwrap();
    // Far from the top, each drained: the three at the top stay
    for op in ["link", "unlink", "link", "unlink"] {
        write(&db, vec![json!({ "op": op, "source": "Task-198", "kind": "refs", "target": "Task-199" })]).unwrap();
        db.wait_for_index_builds().unwrap();
    }
    let heard = heard.lock().unwrap();
    assert_eq!(heard.len(), 1, "only the first read's answer: {heard:?}");
}

#[test]
fn a_watch_over_the_top_hears_the_drain_as_well_as_the_commit() {
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    // A chain long enough that one link at its end leaves pushes for the drain
    let mut ops: Vec<Json> =
        (0..3000).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((1..3000).map(
        |i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": format!("Task-{}", i - 1) }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let top = || Reads {
        reads: vec![
            serde_json::from_value::<ReadOp>(json!({ "read": "query", "plan": {
                "start": { "from": "types", "types": ["Task", "Note"] },
                "steps": [{ "step": "ranked", "index": "taskRank" }, { "step": "limit", "n": 3 }],
                "terminal": { "answer": "hits" } } }))
            .unwrap(),
        ],
        track: None,
    };
    let heard: Arc<Mutex<Vec<Json>>> = Arc::default();
    let into = Arc::clone(&heard);
    db.watch(
        top(),
        Box::new(move |answer| {
            let bytes = answer.as_ref().unwrap();
            let answer: Json = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
            into.lock().unwrap().push(answer[0]["hits"].clone());
        }),
    )
    .unwrap();
    // The chain's head points at its tail: every score moves, more than one commit's pushes
    write(&db, vec![json!({ "op": "link", "source": "Task-0", "kind": "refs", "target": "Task-2999" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    let now = {
        let bytes = db.read(&top()).unwrap();
        serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()[0]["hits"].clone()
    };
    let last = heard.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last, now, "the last delivery is the drained index's top, not the commit's");
    assert!(heard.lock().unwrap().len() >= 3, "the first read, the commit and at least one drain chunk");
}

#[test]
fn a_commit_pushes_its_budget_and_leaves_the_rest_to_the_drain() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    write(&db, (0..1000).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect())
        .unwrap();
    db.wait_for_index_builds().unwrap();
    let queued = |t: &crate::engine::unit::Txn<'_, '_>| {
        let mut n = 0;
        crate::engine::unit::Read::visit(t, &keys::rank_queue("taskRank"), |_, _| {
            n += 1;
            true
        })
        .unwrap();
        n
    };
    db.admin_unit_with_checkpoint(
        |t| {
            // Every node waiting again, none with an arc: each push takes one off the queue and adds none
            ranks::clear(t, "taskRank")?;
            assert_eq!(ranks::join_chunk(t, "taskRank", None, 2000)?, None);
            assert_eq!(queued(t), 1000);
            let waiting = ranks::push_at_commit(t)?;
            assert_eq!(waiting, ["taskRank"]);
            assert_eq!(queued(t), 1000 - ranks::COMMIT_PUSHES);
            Ok(())
        },
        false,
    )
    .unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(db.check().unwrap().is_clean(), "the drain finished what the commit left: {:?}", db.check().unwrap());
}

/// The karate club's friendships, each one way and then the other, a node trashed and restored, and two edges
/// reweighted: the index's bits after it, its pushes drained after each commit. The same history makes the same bits on
/// every platform, which CI's Linux, macOS and Windows runs must each find
#[test]
fn the_same_history_makes_the_same_bits_on_every_platform() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("both", true));
    write(&db, (0..34).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect())
        .unwrap();
    db.wait_for_index_builds().unwrap();
    let edges = crate::engine::algorithms::tests::KARATE_EDGES;
    let weights = crate::engine::algorithms::tests::KARATE_WEIGHTS;
    for (&(a, b), w) in edges.iter().zip(weights) {
        write(&db, vec![json!({ "op": "link", "source": format!("Task-{a}"), "kind": "rates", "target": format!("Task-{b}"), "weight": w })]).unwrap();
        db.wait_for_index_builds().unwrap();
    }
    for op in [
        json!({ "op": "trash", "id": "Task-5" }),
        json!({ "op": "link", "source": "Task-0", "kind": "rates", "target": "Task-1", "weight": 1.5 }),
        json!({ "op": "restore", "id": "Task-5" }),
        json!({ "op": "link", "source": "Task-32", "kind": "rates", "target": "Task-33", "weight": 9.0 }),
    ] {
        write(&db, vec![op]).unwrap();
        db.wait_for_index_builds().unwrap();
    }
    assert_within_bound(&db, "both", true);
    let bits: Vec<u64> = db
        .snapshot(|s| {
            (0..34).map(|i| Ok(ranks::node(s, "taskRank", &format!("Task-{i}"))?.unwrap().x.to_bits())).collect()
        })
        .unwrap();
    assert_eq!(bits, KARATE_HISTORY_BITS);
}

/// `the_same_history_makes_the_same_bits_on_every_platform`'s estimates, as this code found them on an Apple M1 Pro on
/// 2026-11-01
const KARATE_HISTORY_BITS: [u64; 34] = [
    4613673579929887182,
    4610821908511756052,
    4611997313769011390,
    4608420309499248142,
    4604513055730107302,
    4607900079070249754,
    4607548093700926650,
    4606344388543433671,
    4607739974525420031,
    4599412107091475773,
    4604565980822168575,
    4599726176834817916,
    4600744756592898767,
    4607823236196018639,
    4601446465956955386,
    4603085726175660378,
    4603338583678159216,
    4599653484847848568,
    4599428786596349597,
    4601721342185235004,
    4600423382346096114,
    4600698258575240285,
    4601460709888571015,
    4608865384609143664,
    4603221709648260067,
    4606851536766191080,
    4602772274935013647,
    4606406259891058125,
    4602458881641904151,
    4606674512065789340,
    4605137304703717078,
    4609023951797359374,
    4613413487413652885,
    4615005999353646894,
];

#[test]
fn a_savepoint_rolled_back_takes_the_index_changes_it_made_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    write(&db, (0..6).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect()).unwrap();
    let mut unit = db.begin().unwrap();
    unit.write(&super::batch(vec![json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-0" })]))
        .unwrap();
    let depth = unit.savepoint().unwrap();
    unit.write(&super::batch(vec![
        json!({ "op": "link", "source": "Task-2", "kind": "weighs", "target": "Task-0", "weight": 4 }),
        json!({ "op": "create", "type": "Task", "id": "Task-9" }),
        json!({ "op": "link", "source": "Task-9", "kind": "refs", "target": "Task-1" }),
        json!({ "op": "trash", "id": "Task-3" }),
    ]))
    .unwrap();
    unit.rollback_to(depth).unwrap();
    unit.commit(false).unwrap();
    assert_within_bound(&db, "out", true);
    let (found, _) = indexed(&db);
    assert_eq!(found.len(), 6, "Task-9's creation and Task-3's trash rolled back with the savepoint: {found:?}");
}

/// The child of `a_drain_killed_part_way_resumes_when_the_database_opens_again`: a long chain linked so its drain has
/// much to do, a line said once that commit is durable, then nothing until it is killed
#[test]
fn child_links_a_long_chain_and_waits_to_be_killed() {
    let Some(path) = std::env::var_os("ELMDB_RANK_CHILD") else { return };
    let db = Database::open(path, OpenOptions { schema: Some(schema("out", true)), syncs: true, ..Default::default() })
        .unwrap();
    let mut ops: Vec<Json> =
        (0..6_000).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((1..6_000).map(
        |i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": format!("Task-{}", i - 1) }),
    ));
    db.write(&super::batch(ops), true).unwrap();
    db.wait_for_index_builds().unwrap();
    db.write(
        &super::batch(vec![json!({ "op": "link", "source": "Task-0", "kind": "refs", "target": "Task-5999" })]),
        true,
    )
    .unwrap();
    println!("linked");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[test]
fn a_drain_killed_part_way_resumes_when_the_database_opens_again() {
    use std::io::BufRead;
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "engine::tests::rank_index::child_links_a_long_chain_and_waits_to_be_killed", "--nocapture"])
        .env("ELMDB_RANK_CHILD", dir.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(lines.any(|line| line.unwrap().contains("linked")), "the child linked the chain");
    // Killed while its drain still has pushes waiting, most likely part way through a chunk
    child.kill().unwrap();
    child.wait().unwrap();
    // The bound the index's residuals make, against its tolerance of 1e-9
    let bound = |db: &Database| db.snapshot(|s| Ok(ranks::totals(s, "taskRank")?.error(0.85))).unwrap();
    // Read-only, which starts no drain: the kill left the bound past the tolerance
    let looked = Database::open(dir.path(), OpenOptions { read_only: true, ..Default::default() }).unwrap();
    assert!(bound(&looked) > 1e-9, "the child was killed before its drain finished: {}", bound(&looked));
    looked.close().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions { schema: Some(schema("out", true)), syncs: false, ..Default::default() },
    )
    .unwrap();
    db.wait_for_index_builds().unwrap();
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert!(bound(&db) <= 1e-9, "the drain resumed on open and finished: {}", bound(&db));
    assert!(db.snapshot(|s| ranks::is_built(s, "taskRank")).unwrap());
}

const UNIT_NODES: usize = 5;

#[derive(Debug, Clone)]
enum UnitOp {
    Link(usize, usize, usize, u8),
    Unlink(usize, usize, usize),
    Reweigh(usize, usize, u8),
    Trash(usize),
    Restore(usize),
    Destroy(usize),
    Create(usize),
}

fn unit_op() -> impl Strategy<Value = UnitOp> {
    let n = 0..UNIT_NODES + 1;
    prop_oneof![
        6 => (n.clone(), 0..4usize, n.clone(), 0..5u8).prop_map(|(s, k, t, w)| UnitOp::Link(s, k, t, w)),
        2 => (n.clone(), 0..4usize, n.clone()).prop_map(|(s, k, t)| UnitOp::Unlink(s, k, t)),
        2 => (n.clone(), n.clone(), 0..5u8).prop_map(|(s, t, w)| UnitOp::Reweigh(s, t, w)),
        1 => n.clone().prop_map(UnitOp::Trash),
        1 => n.clone().prop_map(UnitOp::Restore),
        1 => n.clone().prop_map(UnitOp::Destroy),
        1 => n.prop_map(UnitOp::Create),
    ]
}

/// Task-0 to Task-4, and Tag-5: an entity of a type the index doesn't hold
fn unit_id(n: usize) -> String {
    if n < UNIT_NODES { format!("Task-{n}") } else { "Tag-5".to_owned() }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, failure_persistence: None, ..ProptestConfig::default() })]

    /// Inside one unit, where no drain runs, every write's own changes keep the index whole: each residual what the
    /// estimates make it, the order, the queue and the totals what the nodes make them, the bound true
    #[test]
    fn every_write_keeps_the_index_whole_with_no_drain_to_tidy_after_it(
        ops in prop::collection::vec(unit_op(), 1..30),
        direction in prop_oneof![Just("out"), Just("in"), Just("both")],
    ) {
        const KINDS: [&str; 4] = ["refs", "weighs", "rates", "other"];
        let dir = tempfile::tempdir().unwrap();
        let db = open_with(&dir, schema(direction, true));
        let mut creates: Vec<Json> = (0..UNIT_NODES).map(|n| json!({ "op": "create", "type": "Task", "id": unit_id(n) })).collect();
        creates.push(json!({ "op": "create", "type": "Tag", "id": unit_id(UNIT_NODES) }));
        write(&db, creates).unwrap();
        db.wait_for_index_builds().unwrap();
        let mut unit = db.begin().unwrap();
        for op in &ops {
            let json = match *op {
                UnitOp::Link(s, k, t, w) => {
                    let mut op = json!({ "op": "link", "source": unit_id(s), "kind": KINDS[k], "target": unit_id(t) });
                    if k == 1 || k == 2 {
                        op["weight"] = json!(f64::from(w) / 2.0);
                    }
                    if k == 1 {
                        op["info"] = json!({ "n": w % 2 });
                    }
                    op
                }
                UnitOp::Unlink(s, k, t) => json!({ "op": "unlink", "source": unit_id(s), "kind": KINDS[k], "target": unit_id(t) }),
                UnitOp::Reweigh(s, t, w) => json!({ "op": "link", "source": unit_id(s), "kind": "rates", "target": unit_id(t), "weight": f64::from(w) }),
                UnitOp::Trash(n) => json!({ "op": "trash", "id": unit_id(n) }),
                UnitOp::Restore(n) => json!({ "op": "restore", "id": unit_id(n) }),
                UnitOp::Destroy(n) => json!({ "op": "destroy", "id": unit_id(n) }),
                UnitOp::Create(n) => json!({ "op": "create", "type": if n < UNIT_NODES { "Task" } else { "Tag" }, "id": unit_id(n) }),
            };
            // A savepoint around each write, so a refused one leaves the unit as it was
            let depth = unit.savepoint().unwrap();
            if unit.write(&super::batch(vec![json])).is_err() {
                unit.rollback_to(depth).unwrap();
            } else {
                unit.release(depth).unwrap();
            }
            let violations = unit.read_with(|t| ranks::violations(t)).unwrap();
            prop_assert!(violations.is_empty(), "after {:?}: {:?}", op, violations.iter().map(|v| &v.message).collect::<Vec<_>>());
        }
        unit.abort();
    }
}

/// Every node of the index, by id: its estimate and residual
fn nodes_of(t: &impl crate::engine::unit::Read) -> Vec<(String, f64, f64)> {
    let mut out = Vec::new();
    crate::engine::unit::Read::visit(t, &keys::rank_nodes("taskRank"), |key, bytes| {
        let id = match crate::format::key::decode(key).unwrap().into_iter().nth(2) {
            Some(crate::format::key::Part::String(id)) => id,
            other => panic!("{other:?}"),
        };
        let n = ranks::Node::decode(bytes).unwrap();
        out.push((id, n.x, n.r));
        true
    })
    .unwrap();
    out
}

#[test]
fn pushes_sweep_the_queue_from_where_they_stopped_and_wrap_to_its_start() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    write(&db, (0..4).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect()).unwrap();
    db.wait_for_index_builds().unwrap();
    db.admin_unit_with_checkpoint(
        |t| {
            // Every node waiting again, none with an arc, so a push takes one off the queue and adds none
            ranks::clear(t, "taskRank")?;
            assert_eq!(ranks::join_chunk(t, "taskRank", None, 10)?, None);
            let pushed = |t: &crate::engine::unit::Txn<'_, '_>| -> Vec<String> {
                nodes_of(t).into_iter().filter(|(_, x, _)| *x > 0.0).map(|(id, ..)| id).collect()
            };
            ranks::push(t, "taskRank", 1)?;
            assert_eq!(pushed(t), ["Task-0"]);
            ranks::push(t, "taskRank", 2)?;
            assert_eq!(pushed(t), ["Task-0", "Task-1", "Task-2"], "the next push starts after the last");
            // The sweep's place survives into a drain, which goes on from it and wraps
            let mut left = nodes_of(t);
            left.retain(|(_, x, _)| *x == 0.0);
            assert_eq!(left.len(), 1);
            ranks::drain(t, "taskRank", 1)?;
            assert_eq!(pushed(t).len(), 4, "the drain pushed Task-3, after Task-2");
            Ok(())
        },
        false,
    )
    .unwrap();
}

#[test]
fn a_build_goes_on_across_chunks_and_types_from_its_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    let mut ops: Vec<Json> =
        (0..5).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((0..4).map(|i| json!({ "op": "create", "type": "Note", "id": format!("Note-{i}") })));
    ops.push(json!({ "op": "create", "type": "Tag", "id": "Tag-0" }));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    db.admin_unit_with_checkpoint(
        |t| {
            ranks::clear(t, "taskRank")?;
            let mut after = None;
            let mut cursors = Vec::new();
            while let Some(last) = ranks::join_chunk(t, "taskRank", after, 3)? {
                assert_eq!(ranks::totals(t, "taskRank")?.stage, ranks::Stage::Joining, "joining until the last chunk");
                cursors.push(last);
                after = Some(last);
            }
            // Three Tasks, then two Tasks and a Note, then three Notes: each cursor later than the one before, the Tag
            // never a node
            assert_eq!(cursors.len(), 3, "{cursors:?}");
            assert!(cursors.windows(2).all(|pair| pair[0] < pair[1]), "{cursors:?}");
            assert_eq!(cursors[1].0, 1, "the second chunk ended in the second type: {cursors:?}");
            let ids: Vec<String> = nodes_of(t).into_iter().map(|(id, ..)| id).collect();
            assert_eq!(ids.len(), 9, "{ids:?}");
            assert!(!ids.contains(&"Tag-0".to_owned()));
            assert_eq!(ranks::totals(t, "taskRank")?.stage, ranks::Stage::Pushing);
            Ok(())
        },
        false,
    )
    .unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn check_reports_each_way_an_index_can_disagree_with_its_nodes() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    let mut ops: Vec<Json> =
        (0..6).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((1..6).map(
        |i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "weighs", "target": "Task-0", "weight": i }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let said = |damage: &dyn Fn(&mut heed::RwTxn<'_>, crate::store::Db) -> crate::Result<()>| {
        db.store().unwrap().write(|txn, d| damage(txn, d), false).unwrap();
        let said: Vec<String> = db.check().unwrap().violations.into_iter().map(|v| v.message).collect();
        db.rebuild(None).unwrap();
        assert!(db.check().unwrap().is_clean());
        said.join("\\n")
    };
    let node = |f: fn(&mut ranks::Node)| {
        move |txn: &mut heed::RwTxn<'_>, d: crate::store::Db| {
            let key = keys::rank_node("taskRank", "Task-3");
            let mut n = ranks::Node::decode(d.main.get(txn, &key)?.unwrap())?;
            f(&mut n);
            d.main.put(txn, &key, &n.encode())?;
            Ok(())
        }
    };
    assert!(said(&node(|n| n.w += 1.0)).contains("out-weight"));
    assert!(said(&node(|n| n.x = f64::NAN)).contains("isn't finite"));
    assert!(said(&node(|n| n.r = f64::INFINITY)).contains("isn't finite"));
    assert!(said(&node(|n| n.w = f64::NAN)).contains("isn't finite"));
    // A node's queue entry gone, and one queued that waits for nothing
    let queued = said(&|txn, d| {
        let n = ranks::Node::decode(d.main.get(txn, &keys::rank_node("taskRank", "Task-3"))?.unwrap())?;
        d.main.put(txn, &keys::rank_queued("taskRank", n.type_at, n.seq), b"Task-3")?;
        Ok(())
    });
    assert!(queued.contains("queue holds"), "{queued}");
    // A node more than the types' live entities: one no longer live, still a node
    let extra = said(&|txn, d| {
        let n = ranks::Node::decode(d.main.get(txn, &keys::rank_node("taskRank", "Task-3"))?.unwrap())?;
        d.main.put(txn, &keys::rank_node("taskRank", "Task-9"), &ranks::Node { seq: n.seq + 100, ..n }.encode())?;
        Ok(())
    });
    assert!(extra.contains("live entities"), "{extra}");
    let order = said(&|txn, d| {
        let (low, high) = crate::format::key::prefix_range(&keys::rank_orders("taskRank"));
        let range = (std::ops::Bound::Included(low.as_slice()), std::ops::Bound::Excluded(high.as_slice()));
        let first = d.main.range(txn, &range)?.next().unwrap()?.0.to_vec();
        d.main.delete(txn, &first)?;
        Ok(())
    });
    assert!(order.contains("score order"), "{order}");
    let counted = said(&|txn, d| {
        let key = keys::rank_totals("taskRank");
        let mut bytes = d.main.get(txn, &key)?.unwrap().to_vec();
        bytes[0..8].copy_from_slice(&99u64.to_le_bytes());
        d.main.put(txn, &key, &bytes)?;
        Ok(())
    });
    assert!(counted.contains("totals"), "{counted}");
    // Each of the totals alone: the residuals, which the bound is counted from
    let residuals = said(&|txn, d| {
        let key = keys::rank_totals("taskRank");
        let mut bytes = d.main.get(txn, &key)?.unwrap().to_vec();
        bytes[16..24].copy_from_slice(&1.0f64.to_le_bytes());
        d.main.put(txn, &key, &bytes)?;
        Ok(())
    });
    assert!(residuals.contains("totals"), "{residuals}");
}

#[test]
fn a_build_across_types_joins_a_later_type_whose_entities_came_first() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    // Notes before Tasks, so every Note's place is below the Tasks' that end a chunk
    let mut ops: Vec<Json> =
        (0..4).map(|i| json!({ "op": "create", "type": "Note", "id": format!("Note-{i}") })).collect();
    ops.extend((0..5).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    db.admin_unit_with_checkpoint(
        |t| {
            ranks::clear(t, "taskRank")?;
            let mut after = None;
            while let Some(last) = ranks::join_chunk(t, "taskRank", after, 2)? {
                after = Some(last);
            }
            assert_eq!(nodes_of(t).len(), 9, "every Task and every Note");
            Ok(())
        },
        false,
    )
    .unwrap();
}

#[test]
fn an_out_weight_left_by_cancellation_is_counted_again_from_the_arcs() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    write(&db, (0..3).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect()).unwrap();
    // Out-weight 1e20 + 1, which is 1e20 in a double: removing the heavy arc leaves 0 by subtraction, 1 by its arcs
    write(
        &db,
        vec![
            json!({ "op": "link", "source": "Task-0", "kind": "weighs", "target": "Task-1", "weight": 1e20 }),
            json!({ "op": "link", "source": "Task-0", "kind": "weighs", "target": "Task-2", "weight": 1.0 }),
        ],
    )
    .unwrap();
    write(&db, vec![json!({ "op": "unlink", "source": "Task-0", "kind": "weighs", "target": "Task-1" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    let w = db.snapshot(|s| Ok(ranks::node(s, "taskRank", "Task-0")?.unwrap().w)).unwrap();
    assert_eq!(w, 1.0);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
}

#[test]
fn an_arc_of_a_node_whose_arcs_weighed_nothing_carries_all_it_holds_once_weighed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    write(&db, (0..2).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect()).unwrap();
    let rate = |w: f64| json!({ "op": "link", "source": "Task-0", "kind": "rates", "target": "Task-1", "weight": w });
    write(&db, vec![rate(0.0)]).unwrap();
    db.wait_for_index_builds().unwrap();
    assert_eq!(db.snapshot(|s| Ok(ranks::node(s, "taskRank", "Task-0")?.unwrap().w)).unwrap(), 0.0);
    write(&db, vec![rate(2.0)]).unwrap();
    db.wait_for_index_builds().unwrap();
    assert_eq!(db.snapshot(|s| Ok(ranks::node(s, "taskRank", "Task-0")?.unwrap().w)).unwrap(), 2.0);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    // And back to nothing, the arc kept; built again, so the drain pushes the node whose arcs weigh nothing
    write(&db, vec![rate(0.0)]).unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    db.rebuild(None).unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    // What check() counts the far end's residual from, the zero-weight arc carrying nothing
    db.store()
        .unwrap()
        .write(
            |txn, d| {
                let key = keys::rank_node("taskRank", "Task-1");
                let mut n = ranks::Node::decode(d.main.get(txn, &key)?.unwrap())?;
                n.r += 0.5;
                d.main.put(txn, &key, &n.encode())?;
                Ok(())
            },
            false,
        )
        .unwrap();
    let said: Vec<String> = db.check().unwrap().violations.into_iter().map(|v| v.message).collect();
    assert!(said.iter().any(|m| m.contains("Task-1 holds residual")), "{said:?}");
}

#[test]
fn a_reported_error_adds_the_rounding_of_every_score_and_keeps_one_figure() {
    // 0.9e-6 over 40 nodes: each score kept to 1e-8, whose rounding adds up to 2e-7, so 1.1e-6, kept as 2e-6
    let (bound, step) = ranks::reported(0.9e-6, 40);
    assert!((bound - 2e-6).abs() < 1e-18 && (step - 1e-8).abs() < 1e-22, "{bound} {step}");
    assert_eq!(ranks::reported(0.0, 40), (0.0, 0.0));
    assert_eq!(ranks::reported(1e-6, 0), (0.0, 0.0));
}

#[test]
fn draining_in_chunks_makes_the_bits_one_drain_makes() {
    let bits = |chunk: usize| {
        let dir = tempfile::tempdir().unwrap();
        let db = open_with(&dir, schema("both", true));
        let mut ops: Vec<Json> =
            (0..34).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
        let edges = crate::engine::algorithms::tests::KARATE_EDGES;
        let weights = crate::engine::algorithms::tests::KARATE_WEIGHTS;
        ops.extend(edges.iter().zip(weights).map(|(&(a, b), w)| json!({ "op": "link", "source": format!("Task-{a}"), "kind": "rates", "target": format!("Task-{b}"), "weight": w })));
        write(&db, ops).unwrap();
        db.wait_for_index_builds().unwrap();
        db.admin_unit_with_checkpoint(
            |t| {
                // Joined again and drained from nothing, `chunk` pushes at a time, each drain taking up the sweep
                ranks::clear(t, "taskRank")?;
                assert_eq!(ranks::join_chunk(t, "taskRank", None, 100)?, None);
                while ranks::drain(t, "taskRank", chunk)? {}
                Ok(())
            },
            false,
        )
        .unwrap();
        db.snapshot(|s| {
            (0..34).map(|i| Ok(ranks::node(s, "taskRank", &format!("Task-{i}"))?.unwrap().x.to_bits())).collect()
        })
        .unwrap()
    };
    let whole: Vec<u64> = bits(usize::MAX);
    for chunk in [1, 7, 33] {
        assert_eq!(bits(chunk), whole, "drained {chunk} pushes at a time");
    }
}

#[test]
fn the_totals_are_counted_afresh_once_as_many_changes_as_nodes_have_added_to_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("both", true));
    let mut ops: Vec<Json> =
        (0..34).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend(crate::engine::algorithms::tests::KARATE_EDGES.iter().map(
        |&(a, b)| json!({ "op": "link", "source": format!("Task-{a}"), "kind": "refs", "target": format!("Task-{b}") }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    // The estimates' sum off by less than `check()` tells, and `moved` changes made since the last count
    let settled = |moved: u64| {
        let found = std::cell::Cell::new((f64::NAN, u64::MAX));
        db.admin_unit_with_checkpoint(
            |t| {
                let key = keys::rank_totals("taskRank");
                let mut bytes = t.get(&key)?.unwrap();
                let estimates = f64::from_le_bytes(bytes[8..16].try_into().unwrap()) + 1e-12;
                bytes[8..16].copy_from_slice(&estimates.to_le_bytes());
                bytes[24..32].copy_from_slice(&moved.to_le_bytes());
                t.put(&key, &bytes)?;
                ranks::settle(t, "taskRank")?;
                let mut counted = 0.0;
                t.visit(&keys::rank_nodes("taskRank"), |_, bytes| {
                    counted += ranks::Node::decode(bytes).unwrap().x;
                    true
                })?;
                let totals = ranks::totals(t, "taskRank")?;
                found.set((totals.estimates - counted, totals.moved));
                Ok(())
            },
            false,
        )
        .unwrap();
        found.get()
    };
    let (off, moved) = settled(33);
    assert!(off > 0.0 && moved == 33, "fewer changes than nodes counted the totals afresh: {off} {moved}");
    assert_eq!(settled(34), (0.0, 0), "as many changes as nodes left the totals uncounted");
}

/// A ring of `nodes` tasks each linking the `degree` after it, built at a tolerance whose drains stay near a change, its
/// sequence starting from `from`
fn near_schema() -> Schema {
    let mut near = serde_json::to_value(schema("out", true)).unwrap();
    near["ranks"]["taskRank"]["tolerance"] = json!(1e-6);
    Schema::from_json(&near).unwrap()
}

fn near_links(dir: &tempfile::TempDir, nodes: usize, degree: usize, from: u64) -> Database {
    let db = open_with(dir, near_schema());
    db.store()
        .unwrap()
        .write(
            |txn, d| {
                d.main.put(txn, &keys::sequence("seq"), &crate::engine::values::encode(&from.into()))?;
                Ok(())
            },
            false,
        )
        .unwrap();
    let mut ops: Vec<Json> =
        (0..nodes).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    for i in 0..nodes {
        for step in 1..=degree {
            ops.push(json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs",
                "target": format!("Task-{}", (i + step) % nodes) }));
        }
    }
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    db
}

/// How many nodes are queued, and each node's bits
fn queued_and_bits(t: &impl crate::engine::unit::Read) -> crate::Result<(u64, Vec<(u64, u64)>)> {
    let mut queued = 0;
    t.visit(&keys::rank_queue("taskRank"), |_, _| {
        queued += 1;
        true
    })?;
    let mut nodes = Vec::new();
    t.visit(&keys::rank_nodes("taskRank"), |_, bytes| {
        let n = ranks::Node::decode(bytes).unwrap();
        nodes.push((n.x.to_bits(), n.r.to_bits()));
        true
    })?;
    Ok((queued, nodes))
}

#[test]
fn a_drain_reading_nodes_one_at_a_time_makes_the_pushes_the_commits_would() {
    // After `limit` pushes of one sweep or the other: a push no sweep makes would spend one of them and show here. The
    // history ends with a node leaving and coming back at the place the sweep stopped, so that place waits again. The
    // sum held past any budget, where it says, so pushes end only when no node waits
    let after = |nodes: usize, degree: usize, limit: usize, by_drain: bool, exhaust: bool| {
        let dir = tempfile::tempdir().unwrap();
        let db = near_links(&dir, nodes, degree, 0);
        let found = std::cell::RefCell::new(Vec::new());
        db.admin_unit_with_checkpoint(
            |t| {
                let swept = ranks::totals(t, "taskRank")?.swept;
                let mut at_swept = None;
                t.visit(&keys::rank_nodes("taskRank"), |key, bytes| {
                    let n = ranks::Node::decode(bytes).unwrap();
                    if Some((n.type_at, n.seq)) == swept {
                        let parts = crate::format::key::decode(key).unwrap();
                        if let Some(crate::format::key::Part::String(id)) = parts.into_iter().nth(2) {
                            at_swept = Some(id);
                        }
                    }
                    true
                })?;
                let at_swept = at_swept.unwrap_or_else(|| "Task-7".to_owned());
                ranks::liveness(t, "Task-200", false)?;
                ranks::liveness(t, &at_swept, false)?;
                ranks::liveness(t, &at_swept, true)?;
                if exhaust {
                    let key = keys::rank_totals("taskRank");
                    let mut bytes = t.get(&key)?.unwrap();
                    let residuals = f64::from_le_bytes(bytes[16..24].try_into().unwrap()) + 1e6;
                    bytes[16..24].copy_from_slice(&residuals.to_le_bytes());
                    t.put(&key, &bytes)?;
                }
                let (queued, before) = queued_and_bits(t)?;
                assert!(queued > 0 && queued <= nodes as u64 / 4, "{queued} queued reads every node");
                if by_drain {
                    ranks::drain(t, "taskRank", limit)?;
                } else {
                    ranks::push(t, "taskRank", limit)?;
                }
                let (_, bits) = queued_and_bits(t)?;
                let moved = bits.iter().zip(&before).filter(|(a, b)| a != b).count();
                let totals = ranks::totals(t, "taskRank")?;
                found.borrow_mut().push((bits, totals.swept, moved));
                Ok(())
            },
            false,
        )
        .unwrap();
        found.into_inner().remove(0)
    };
    // Five links each reach a quarter of the nodes within a few pushes, and a whole drain reads them all
    for limit in [3, 21, 1000] {
        let (drained, swept, moved) = after(400, 5, limit, true, false);
        let (committed, swept_by_commit, _) = after(400, 5, limit, false, false);
        assert_eq!(drained, committed, "five links, after {limit} pushes");
        assert_eq!(swept, swept_by_commit, "five links, after {limit} pushes");
        if limit == 1000 {
            assert!(moved > 100, "{moved} nodes moved by a whole drain, which never read them all");
        } else {
            assert!(moved > 0 && moved <= 100, "{moved} nodes moved by {limit} pushes");
        }
    }
    // One link each, around a ring long enough that fronts of residual die out within a quarter of its nodes: pushed until
    // none waits, read one at a time throughout, the sweep wrapping to its start as each front ends
    for limit in [89, 377, 1000] {
        let (drained, swept, moved) = after(1600, 1, limit, true, true);
        let (committed, swept_by_commit, _) = after(1600, 1, limit, false, true);
        assert_eq!(drained, committed, "one link, after {limit} pushes");
        assert_eq!(swept, swept_by_commit, "one link, after {limit} pushes");
        assert!(moved > 0 && moved <= 400, "{moved} nodes moved by {limit} pushes");
    }
}

#[test]
fn a_node_a_push_takes_under_the_threshold_no_longer_waits_in_a_drain_reading_one_at_a_time() {
    // Around a ring of single links, a node holding negative residual and the one after it holding a little more than the
    // threshold: the first's push takes the second under it, so no sweep should push the second
    let after = |by_drain: bool| {
        let dir = tempfile::tempdir().unwrap();
        let db = near_links(&dir, 400, 1, 0);
        let threshold = 1e-6 * 0.15 * 0.15 / (2.0 + 1e-6);
        let found = std::cell::RefCell::new(Vec::new());
        db.admin_unit_with_checkpoint(
            |t| {
                for (id, r) in [("Task-10", -2.0 * threshold), ("Task-11", 1.5 * threshold)] {
                    let mut n = ranks::node(t, "taskRank", id)?.unwrap();
                    n.r = r;
                    t.put(&keys::rank_node("taskRank", id), &n.encode())?;
                    t.put(&keys::rank_queued("taskRank", n.type_at, n.seq), id.as_bytes())?;
                }
                // The sweep from the first place, and the sum past any budget, so pushes end only when no node waits
                let key = keys::rank_totals("taskRank");
                let mut bytes = t.get(&key)?.unwrap();
                bytes.truncate(41);
                let residuals = f64::from_le_bytes(bytes[16..24].try_into().unwrap()) + 1e6;
                bytes[16..24].copy_from_slice(&residuals.to_le_bytes());
                t.put(&key, &bytes)?;
                let (queued, _) = queued_and_bits(t)?;
                assert!((2..=100).contains(&queued), "{queued} queued reads every node");
                if by_drain {
                    ranks::drain(t, "taskRank", 90)?;
                } else {
                    ranks::push(t, "taskRank", 90)?;
                }
                let second = ranks::node(t, "taskRank", "Task-11")?.unwrap();
                assert!(second.r != 0.0 && second.r.abs() <= threshold, "the second was pushed: {}", second.r);
                let (_, bits) = queued_and_bits(t)?;
                found.borrow_mut().push((bits, ranks::totals(t, "taskRank")?.swept));
                Ok(())
            },
            false,
        )
        .unwrap();
        found.into_inner().remove(0)
    };
    assert_eq!(after(true), after(false));
}

#[test]
fn a_drain_costs_what_it_reaches_however_high_the_stores_sequence() {
    // A node's place holds the store's sequence as it was made, 2⁴⁰ here, which nothing a drain keeps is sized by
    let dir = tempfile::tempdir().unwrap();
    let db = near_links(&dir, 400, 5, 1 << 40);
    db.admin_unit_with_checkpoint(
        |t| {
            ranks::liveness(t, "Task-200", false)?;
            let (queued, _) = queued_and_bits(t)?;
            assert!(queued > 0 && queued <= 100, "{queued} queued reads every node");
            while ranks::drain(t, "taskRank", usize::MAX)? {}
            Ok(())
        },
        false,
    )
    .unwrap();
}

#[test]
fn a_drain_ends_when_its_held_sum_is_past_the_budget_and_nothing_waits() {
    // The held sum off by more than any push can lower: every waiting node is pushed, and then none is left to push
    let inflated = |db: &Database| {
        let done = std::cell::Cell::new(true);
        db.admin_unit_with_checkpoint(
            |t| {
                ranks::liveness(t, "Task-20", false)?;
                let key = keys::rank_totals("taskRank");
                let mut bytes = t.get(&key)?.unwrap();
                let residuals = f64::from_le_bytes(bytes[16..24].try_into().unwrap()) + 1e6;
                bytes[16..24].copy_from_slice(&residuals.to_le_bytes());
                t.put(&key, &bytes)?;
                done.set(ranks::drain(t, "taskRank", usize::MAX)?);
                let (queued, _) = queued_and_bits(t)?;
                assert_eq!(queued, 0, "a node still waits");
                // Nothing queued to read, and the sum still off
                assert!(!ranks::drain(t, "taskRank", usize::MAX)?, "owed a push with no node queued");
                Ok(())
            },
            false,
        )
        .unwrap();
        done.get()
    };
    // Few waiting, read one at a time
    let dir = tempfile::tempdir().unwrap();
    assert!(!inflated(&near_links(&dir, 400, 5, 0)), "owed a push with none to make, reading one at a time");
    // A small graph, where a quarter of the nodes is soon passed and all are read
    let dir = tempfile::tempdir().unwrap();
    assert!(!inflated(&near_links(&dir, 40, 5, 0)), "owed a push with none to make, reading every node");
}

#[test]
fn a_commit_past_the_changes_a_count_waits_for_is_left_to_the_builder() {
    // A loose tolerance, so a commit owes no pushes and only the count can call for the builder
    let dir = tempfile::tempdir().unwrap();
    let mut loose = serde_json::to_value(schema("out", true)).unwrap();
    loose["ranks"]["taskRank"]["tolerance"] = json!(0.5);
    let db = open_with(&dir, Schema::from_json(&loose).unwrap());
    let ops: Vec<Json> =
        (0..40).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let left_to_builder = |moved: u64| {
        let found = std::cell::Cell::new(false);
        db.admin_unit_with_checkpoint(
            |t| {
                let key = keys::rank_totals("taskRank");
                let mut bytes = t.get(&key)?.unwrap();
                bytes[24..32].copy_from_slice(&moved.to_le_bytes());
                t.put(&key, &bytes)?;
                found.set(!ranks::push_at_commit(t)?.is_empty());
                Ok(())
            },
            false,
        )
        .unwrap();
        found.get()
    };
    assert!(!left_to_builder(0), "nothing moved, and nothing owed");
    assert!(!left_to_builder(39), "fewer changes than nodes");
    assert!(left_to_builder(40), "as many changes as nodes");
    // A node's leaving and coming back are changes the count takes in
    db.admin_unit_with_checkpoint(
        |t| {
            let key = keys::rank_totals("taskRank");
            let mut bytes = t.get(&key)?.unwrap();
            bytes[24..32].copy_from_slice(&0u64.to_le_bytes());
            t.put(&key, &bytes)?;
            ranks::liveness(t, "Task-3", false)?;
            ranks::liveness(t, "Task-3", true)?;
            assert!(ranks::totals(t, "taskRank")?.moved >= 2);
            Ok(())
        },
        false,
    )
    .unwrap();
}

#[test]
fn a_commit_to_an_index_with_no_nodes_is_left_to_nobody() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    write(&db, vec![json!({ "op": "create", "type": "Tag", "id": "Tag-1" })]).unwrap();
    db.admin_unit_with_checkpoint(
        |t| {
            assert_eq!(ranks::totals(t, "taskRank")?.nodes, 0);
            assert!(ranks::push_at_commit(t)?.is_empty(), "nothing to count, and no drain owed");
            Ok(())
        },
        false,
    )
    .unwrap();
}

#[test]
fn a_tight_tolerance_counts_the_totals_before_their_rounding_reaches_its_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut tight = serde_json::to_value(schema("out", true)).unwrap();
    tight["ranks"]["taskRank"]["tolerance"] = json!(1e-13);
    let db = open_with(&dir, Schema::from_json(&tight).unwrap());
    let ops: Vec<Json> =
        (0..40).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let settled = |moved: u64| {
        let found = std::cell::Cell::new(u64::MAX);
        db.admin_unit_with_checkpoint(
            |t| {
                let key = keys::rank_totals("taskRank");
                let mut bytes = t.get(&key)?.unwrap();
                bytes[24..32].copy_from_slice(&moved.to_le_bytes());
                t.put(&key, &bytes)?;
                ranks::settle(t, "taskRank")?;
                found.set(ranks::totals(t, "taskRank")?.moved);
                Ok(())
            },
            false,
        )
        .unwrap();
        found.get()
    };
    // At 1e-13 and a damping of 0.85, a tenth of the budget is 3.4 changes' worst rounding
    assert_eq!(settled(3), 3);
    assert_eq!(settled(4), 0);
}

#[test]
fn a_drains_changes_count_toward_the_totals_being_counted_afresh() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    let n = 200u64;
    let mut ops: Vec<Json> =
        (0..n).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((0..n - 1).map(
        |i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": format!("Task-{}", i + 1) }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    // From `from` changes, what `limit` pushes of a drain leave the count at
    let drained = |from: u64, id: &str, limit: usize| {
        let found = std::cell::Cell::new(0);
        db.admin_unit_with_checkpoint(
            |t| {
                ranks::liveness(t, id, false)?;
                let key = keys::rank_totals("taskRank");
                let mut bytes = t.get(&key)?.unwrap();
                bytes[24..32].copy_from_slice(&from.to_le_bytes());
                t.put(&key, &bytes)?;
                ranks::drain(t, "taskRank", limit)?;
                found.set(ranks::totals(t, "taskRank")?.moved);
                Ok(())
            },
            false,
        )
        .unwrap();
        found.get()
    };
    // Each push changes its node and its one far end, but for the chain's end, which has none
    let moved = drained(5, "Task-100", 3);
    assert!((5 + 5..=5 + 6).contains(&moved), "three pushes took the count from 5 to {moved}");
}

/// The karate club, built, with `window` written between a drain's computing and its writing: the drain a further link
/// leaves. What it answers: the index's estimates and its count of changes
fn with_a_commit_during_a_drain(
    window: Vec<Json>,
) -> (std::sync::Arc<Database>, tempfile::TempDir, (Vec<Option<u64>>, u64)) {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open_with(&dir, schema("both", true)));
    let mut ops: Vec<Json> =
        (0..34).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    let edges = crate::engine::algorithms::tests::KARATE_EDGES;
    let weights = crate::engine::algorithms::tests::KARATE_WEIGHTS;
    ops.extend(edges.iter().zip(weights).map(|(&(a, b), w)| json!({ "op": "link", "source": format!("Task-{a}"), "kind": "rates", "target": format!("Task-{b}"), "weight": w })));
    ops.push(json!({ "op": "create", "type": "Tag", "id": "Tag-0" }));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(window.is_empty()));
    if !window.is_empty() {
        let (inner, fired) = (std::sync::Arc::clone(&db), std::sync::Arc::clone(&fired));
        db.store().unwrap().when_rank_planned(move || {
            write(&inner, window).unwrap();
            fired.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    write(&db, vec![json!({ "op": "link", "source": "Task-9", "kind": "refs", "target": "Task-20" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(fired.load(std::sync::atomic::Ordering::SeqCst), "no drain ran for the link");
    let state = db
        .snapshot(|s| {
            let estimates = (0..34)
                .map(|i| Ok(ranks::node(s, "taskRank", &format!("Task-{i}"))?.map(|n| n.x.to_bits())))
                .collect::<crate::Result<Vec<_>>>()?;
            Ok((estimates, ranks::totals(s, "taskRank")?.moved))
        })
        .unwrap();
    (db, dir, state)
}

#[test]
fn a_commit_that_leaves_the_index_alone_doesnt_disturb_a_drain_being_computed() {
    let (_db, _dir, alone) = with_a_commit_during_a_drain(Vec::new());
    let (db, _dir2, beside) = with_a_commit_during_a_drain(vec![
        json!({ "op": "create", "type": "Tag", "id": "Tag-1" }),
        json!({ "op": "link", "source": "Tag-0", "kind": "other", "target": "Tag-1" }),
    ]);
    assert_eq!(alone, beside, "the commit took pushes, or the drain was computed again");
    let plans = *db.store().unwrap().rank_plans.lock();
    assert!(
        plans.applied > 0 && plans.redone == 0,
        "the drain computed outside the lock wasn't what was written: {plans:?}"
    );
    assert_within_bound(&db, "both", true);
}

#[test]
fn a_rebuild_while_a_drain_is_being_computed_has_the_drain_done_again_from_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open_with(&dir, schema("both", true)));
    let mut ops: Vec<Json> =
        (0..34).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend(crate::engine::algorithms::tests::KARATE_EDGES.iter().map(
        |&(a, b)| json!({ "op": "link", "source": format!("Task-{a}"), "kind": "refs", "target": format!("Task-{b}") }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let inner = std::sync::Arc::clone(&db);
    db.store().unwrap().when_rank_planned(move || inner.rebuild(None).unwrap());
    write(&db, vec![json!({ "op": "link", "source": "Task-9", "kind": "refs", "target": "Task-20" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    assert_within_bound(&db, "both", true);
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
}

#[test]
fn a_commit_that_leaves_the_index_alone_writes_nothing_to_it_and_still_hands_it_on() {
    let dir = tempfile::tempdir().unwrap();
    let db = near_links(&dir, 400, 5, 0);
    write(&db, vec![json!({ "op": "create", "type": "Tag", "id": "Tag-0" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    // Past the budget: the sum as a commit it didn't come from finds it
    db.admin_unit_with_checkpoint(
        |t| {
            let key = keys::rank_totals("taskRank");
            let mut bytes = t.get(&key)?.unwrap();
            let residuals = f64::from_le_bytes(bytes[16..24].try_into().unwrap()) + 1.0;
            bytes[16..24].copy_from_slice(&residuals.to_le_bytes());
            bytes[32..40].copy_from_slice(&0u64.to_le_bytes());
            t.put(&key, &bytes)?;
            let before = t.get(&key)?;
            let (_, nodes) = queued_and_bits(t)?;
            assert_eq!(ranks::push_at_commit(t)?, ["taskRank"], "pushes owed go to the builder");
            assert_eq!(t.get(&key)?, before, "and nothing is written");
            assert_eq!(queued_and_bits(t)?.1, nodes);
            // Within it, and no count due: nothing to hand on
            let mut within = before.clone().unwrap();
            let residuals = f64::from_le_bytes(within[16..24].try_into().unwrap()) - 1.0;
            within[16..24].copy_from_slice(&residuals.to_le_bytes());
            within[24..32].copy_from_slice(&0u64.to_le_bytes());
            t.put(&key, &within)?;
            assert!(ranks::push_at_commit(t)?.is_empty());
            Ok(())
        },
        false,
    )
    .unwrap();
}

#[test]
fn a_commit_that_changes_an_index_already_owing_pushes_leaves_them_to_the_drain() {
    let dir = tempfile::tempdir().unwrap();
    let db = near_links(&dir, 400, 5, 0);
    // Owing pushes as the commit finds it, with no builder told
    db.store()
        .unwrap()
        .write(
            |txn, d| {
                let key = keys::rank_totals("taskRank");
                let mut bytes = d.main.get(txn, &key)?.unwrap().to_vec();
                let residuals = f64::from_le_bytes(bytes[16..24].try_into().unwrap()) + 1.0;
                bytes[16..24].copy_from_slice(&residuals.to_le_bytes());
                d.main.put(txn, &key, &bytes)?;
                Ok(())
            },
            false,
        )
        .unwrap();
    db.admin_unit_with_checkpoint(
        |t| {
            ranks::liveness(t, "Task-200", false)?;
            let swept = ranks::totals(&*t, "taskRank")?.swept;
            let (_, nodes) = queued_and_bits(t)?;
            assert_eq!(ranks::push_at_commit(t)?, ["taskRank"], "owed, so handed to the builder");
            assert_eq!(ranks::totals(&*t, "taskRank")?.swept, swept, "it made pushes");
            assert_eq!(queued_and_bits(t)?.1, nodes);
            Ok(())
        },
        false,
    )
    .unwrap();
}

#[test]
fn a_commit_that_changes_the_index_makes_its_pushes_and_a_run_owing_nothing_commits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = near_links(&dir, 400, 5, 0);
    db.admin_unit_with_checkpoint(
        |t| {
            ranks::liveness(t, "Task-200", false)?;
            let queued = |t: &crate::engine::unit::Txn<'_, '_>| queued_and_bits(t).map(|(queued, _)| queued);
            let swept = ranks::totals(&*t, "taskRank")?.swept;
            let before = queued(t)?;
            ranks::push_at_commit(t)?;
            assert!(ranks::totals(&*t, "taskRank")?.swept != swept || queued(t)? != before, "it made no pushes");
            Ok(())
        },
        false,
    )
    .unwrap();
    db.wait_for_index_builds().unwrap();
    let store = db.store().unwrap();
    let sequence = store.sequence();
    db.snapshot(|s| {
        assert!(!ranks::owes_work(s, "taskRank")?, "drained, and counted if due");
        Ok(())
    })
    .unwrap();
    // A run queued now finds nothing owed: a commit would raise the sequence
    db.builders().unwrap().unwrap().queue(vec![crate::engine::Work::Rank { name: "taskRank".to_owned() }]);
    db.wait_for_index_builds().unwrap();
    assert_eq!(store.sequence(), sequence, "a run owing nothing committed");
}

#[test]
fn a_drain_is_stale_after_any_write_of_the_index_another_drains_and_a_count_included() {
    // What another process's builder does in the window: its own drain, then a count of the totals. Here in one
    // transaction, on the plan one snapshot read
    let dir = tempfile::tempdir().unwrap();
    let db = near_links(&dir, 400, 5, 0);
    db.admin_unit_with_checkpoint(
        |t| {
            ranks::liveness(t, "Task-200", false)?;
            let planned = ranks::plan(&*t, t.schema, "taskRank", usize::MAX, None)?.unwrap();
            assert!(matches!(planned.apply(t)?, ranks::Applied::Written { .. }), "nothing wrote it since it read");
            Ok(())
        },
        false,
    )
    .unwrap();
    let plan_then = |between: &dyn Fn(&mut crate::engine::unit::Txn<'_, '_>) -> crate::Result<()>| {
        let applied = std::cell::Cell::new(None);
        // A sequence of its own for each write: what makes it a later commit
        let planned = std::cell::RefCell::new(None);
        db.admin_unit_with_checkpoint(
            |t| {
                ranks::liveness(t, "Task-100", false)?;
                ranks::liveness(t, "Task-100", true)?;
                Ok(())
            },
            false,
        )
        .unwrap();
        // The builder's own drain of that done, so only `between` writes the index after the plan reads it
        db.wait_for_index_builds().unwrap();
        db.admin_unit_with_checkpoint(
            |t| {
                *planned.borrow_mut() = ranks::plan(&*t, t.schema, "taskRank", 5, None)?;
                Ok(())
            },
            false,
        )
        .unwrap();
        db.admin_unit_with_checkpoint(|t| between(t), false).unwrap();
        db.admin_unit_with_checkpoint(
            |t| {
                // A plan found stale writes nothing, so the unit it was asked in commits empty
                let totals = t.get(&keys::rank_totals("taskRank"))?;
                let answer = planned.borrow().as_ref().unwrap().apply(t)?;
                if answer == ranks::Applied::Stale {
                    assert_eq!(t.get(&keys::rank_totals("taskRank"))?, totals);
                }
                applied.set(Some(answer));
                Ok(())
            },
            false,
        )
        .unwrap();
        applied.get().unwrap()
    };
    assert_eq!(plan_then(&|t| ranks::drain(t, "taskRank", 5).map(|_| ())), ranks::Applied::Stale, "another drain");
    let count = |t: &mut crate::engine::unit::Txn<'_, '_>| {
        let key = keys::rank_totals("taskRank");
        let mut bytes = t.get(&key)?.unwrap();
        bytes[24..32].copy_from_slice(&u64::MAX.to_le_bytes());
        t.put(&key, &bytes)?;
        ranks::settle(t, "taskRank")
    };
    assert_eq!(plan_then(&count), ranks::Applied::Stale, "a count of the totals");
    assert!(matches!(plan_then(&|_| Ok(())), ranks::Applied::Written { .. }), "nothing wrote it");
}

#[test]
fn a_rebuild_leaves_the_commit_that_wrote_the_index_later_than_any_before_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("both", true));
    let mut ops: Vec<Json> =
        (0..34).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend(crate::engine::algorithms::tests::KARATE_EDGES.iter().map(
        |&(a, b)| json!({ "op": "link", "source": format!("Task-{a}"), "kind": "refs", "target": format!("Task-{b}") }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let written_at = || db.snapshot(|s| Ok(ranks::totals(s, "taskRank")?.written_at)).unwrap();
    let before = written_at();
    db.rebuild(None).unwrap();
    assert!(written_at() > before, "a drain that read {before} would take the rebuilt index for the one it read");
}

#[test]
fn a_mirror_keeps_what_a_reader_reads_until_an_edge_changes_after_the_readers_snapshot() {
    use crate::engine::mirror::{EdgeDelta, EdgeMirror, stored_sequence};
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("both", true));
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Task", "id": "Task-0" }),
            json!({ "op": "create", "type": "Task", "id": "Task-1" }),
            json!({ "op": "create", "type": "Task", "id": "Task-2" }),
            json!({ "op": "link", "source": "Task-0", "kind": "refs", "target": "Task-1" }),
            json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" }),
            json!({ "op": "link", "source": "Task-2", "kind": "refs", "target": "Task-0" }),
        ],
    )
    .unwrap();
    db.wait_for_index_builds().unwrap();
    db.snapshot(|s| {
        let at = stored_sequence(s)?;
        let far_ends = |mirror: &mut EdgeMirror, reader: u64, id: &str| {
            let mut far = Vec::new();
            mirror.visit_far_ends(s, reader, id, true, "refs", |name, _| far.push(name.to_owned())).map(|()| far)
        };
        let mut mirror = EdgeMirror::default();
        mirror.at(at);
        assert_eq!(far_ends(&mut mirror, at, "Task-0")?, ["Task-1"]);
        assert_eq!(mirror.held(), 1, "at the mirror's commit, a list read is kept");
        // A commit with no edge in it: the lists are the store's still, whatever a reader behind it reads
        mirror.commit(at, at + 1, Some(&[]));
        assert_eq!(far_ends(&mut mirror, at, "Task-1")?, ["Task-2"]);
        assert_eq!(mirror.held(), 2, "no edge changed since the reader's snapshot, so what it read is kept");
        // One filing an entry in a list held: answered as the commit left it, and nothing more kept
        let filed = EdgeDelta::Filed {
            kind: "refs".into(),
            source: "Task-0".into(),
            target: "Task-2".into(),
            seq: u64::MAX,
            weight: None,
        };
        mirror.commit(at + 1, at + 2, Some(&[filed]));
        assert_eq!(far_ends(&mut mirror, at, "Task-0")?, ["Task-1", "Task-2"], "a list held answers");
        assert_eq!(far_ends(&mut mirror, at, "Task-2")?, ["Task-0"], "a list not held is read from the reader");
        assert_eq!(mirror.held(), 2, "and not kept: an edge changed after the reader's snapshot");
        // One filing an entry in a list not held tells the same
        let mut mirror = EdgeMirror::default();
        mirror.at(at);
        let other = EdgeDelta::Filed {
            kind: "refs".into(),
            source: "Task-1".into(),
            target: "Task-0".into(),
            seq: u64::MAX,
            weight: None,
        };
        mirror.commit(at, at + 1, Some(&[other]));
        assert_eq!(far_ends(&mut mirror, at, "Task-1")?, ["Task-2"]);
        assert_eq!(mirror.held(), 0, "what it read misses the entry filed since, so isn't kept");
        // A reader ahead reads the store, keeps nothing, and leaves the mirror where it was
        let mut mirror = EdgeMirror::default();
        mirror.at(at - 1);
        assert_eq!(far_ends(&mut mirror, at, "Task-1")?, ["Task-2"]);
        assert_eq!((mirror.held(), mirror.sequence()), (0, Some(at - 1)));
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_drain_outside_the_write_lock_starts_a_mirror_and_never_resets_one() {
    use crate::engine::mirror::{EdgeMirror, stored_sequence};
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("both", true));
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-0" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    db.snapshot(|s| {
        let at = stored_sequence(s)?;
        let mirror: std::sync::Arc<crate::sync::PlainMutex<EdgeMirror>> = std::sync::Arc::default();
        ranks::plan(s, s.schema, "taskRank", usize::MAX, Some(&mirror))?;
        assert_eq!(mirror.lock().sequence(), Some(at), "an empty mirror starts at the reader");
        for held in [at + 5, at - 1] {
            mirror.lock().at(held);
            ranks::plan(s, s.schema, "taskRank", usize::MAX, Some(&mirror))?;
            assert_eq!(mirror.lock().sequence(), Some(held), "past it or behind it, a mirror is left as it is");
        }
        Ok(())
    })
    .unwrap();
}

#[test]
fn a_drain_under_the_write_lock_resets_a_mirror_another_process_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let db = near_links(&dir, 40, 5, 0);
    let store = db.store().unwrap();
    let mirror = store.mirror();
    write(&db, vec![json!({ "op": "link", "source": "Task-0", "kind": "refs", "target": "Task-20" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    let held = mirror.lock().sequence().unwrap();
    mirror.lock().at(held - 3);
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
    write(&db, vec![json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-20" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(mirror.lock().sequence(), Some(store.sequence()), "the next commits brought it back to the store's");
}

#[test]
fn a_commit_that_writes_the_index_has_a_drain_being_computed_done_again_from_it() {
    let link = |a: u32, b: u32| json!({ "op": "link", "source": format!("Task-{a}"), "kind": "refs", "target": format!("Task-{b}") });
    let windows = [
        // Out of a hub, whose estimate the drain moved most
        vec![link(33, 9)],
        vec![json!({ "op": "unlink", "source": "Task-0", "kind": "rates", "target": "Task-1" })],
        vec![json!({ "op": "trash", "id": "Task-33" })],
        vec![json!({ "op": "destroy", "id": "Task-0" })],
        vec![json!({ "op": "create", "type": "Task", "id": "Task-40" }), link(33, 40)],
        // A node the first drain's link reached, gone and an arc left to fill in
        vec![json!({ "op": "trash", "id": "Task-20" }), json!({ "op": "restore", "id": "Task-20" }), link(20, 9)],
    ];
    for window in windows {
        let (db, _dir, _) = with_a_commit_during_a_drain(window.clone());
        let plans = *db.store().unwrap().rank_plans.lock();
        assert!(plans.retried > 0, "{window:?} wrote the index under a drain it left standing: {plans:?}");
        assert_within_bound(&db, "both", true);
        let report = db.check().unwrap();
        assert!(report.is_clean(), "after {window:?}: {report:?}");
    }
}

#[test]
fn any_commit_landing_as_a_drain_is_computed_leaves_the_index_within_its_bound() {
    use proptest::test_runner::{Config, TestRunner};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    // Rounds of an op landing between a drain's computing and its writing, and an op leaving that drain to compute
    let rounds = prop::collection::vec((op(), op()), 1..4);
    let run = |db: &Arc<Database>, direction: &str, rounds: &[(Op, Op)], fired: &Arc<AtomicU64>| {
        for (round, (window, trigger)) in rounds.iter().enumerate() {
            let (inner, window, count) = (Arc::clone(db), json_of(window), Arc::clone(fired));
            db.store().unwrap().when_rank_planned(move || {
                // A refused write (a self link, an entity gone) changes nothing
                let _ = write(&inner, vec![window]);
                count.fetch_add(1, Ordering::SeqCst);
            });
            // A refused write changes nothing, so a link of its own in each round, for a drain to compute
            let _ = write(db, vec![json_of(trigger)]);
            let link = json!({ "op": "link", "source": format!("Task-{}", 10 + round), "kind": "rates",
                "target": format!("Task-{}", 20 + round), "weight": 3 });
            write(db, vec![link]).unwrap();
            db.wait_for_index_builds().unwrap();
            // A hook not fired holds the database: let it go
            db.store().unwrap().when_rank_planned(|| {});
            assert_within_bound(db, direction, true);
        }
    };
    let config = |cases| Config { cases, failure_persistence: None, ..Config::default() };
    // Over the karate club a drain reads every node
    let (fired, plans) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    TestRunner::new(config(8))
        .run(&rounds, |rounds| {
            let dir = tempfile::tempdir().unwrap();
            let db = Arc::new(open_with(&dir, schema("both", true)));
            let mut ops: Vec<Json> =
                (0..34).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
            ops.extend(crate::engine::algorithms::tests::KARATE_EDGES.iter().map(|&(a, b)| json!({ "op": "link", "source": format!("Task-{a}"), "kind": "refs", "target": format!("Task-{b}") })));
            write(&db, ops).unwrap();
            db.wait_for_index_builds().unwrap();
            run(&db, "both", &rounds, &fired);
            plans.fetch_add(db.store().unwrap().rank_plans.lock().every_node, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    assert!(fired.load(Ordering::SeqCst) > 0, "no op landed in a drain's window");
    assert!(plans.load(Ordering::SeqCst) > 0, "no drain read every node");
    // Over a long ring of single links, where a drain's fronts of residual die out within a quarter of the nodes, one reads
    // a node at a time throughout
    let (fired, plans) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    TestRunner::new(config(3))
        .run(&rounds, |rounds| {
            let dir = tempfile::tempdir().unwrap();
            let db = Arc::new(near_links(&dir, 1600, 1, 0));
            run(&db, "out", &rounds, &fired);
            plans.fetch_add(db.store().unwrap().rank_plans.lock().one_at_a_time, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
    assert!(fired.load(Ordering::SeqCst) > 0, "no op landed in a drain's window");
    assert!(plans.load(Ordering::SeqCst) > 0, "no drain read a node at a time");
}

#[test]
fn a_commit_landing_as_a_drain_takes_the_lock_has_it_retried_and_not_computed_under_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open_with(&dir, schema("both", true)));
    let mut ops: Vec<Json> =
        (0..34).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend(crate::engine::algorithms::tests::KARATE_EDGES.iter().map(
        |&(a, b)| json!({ "op": "link", "source": format!("Task-{a}"), "kind": "rates", "target": format!("Task-{b}"), "weight": 1 }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let inner = std::sync::Arc::clone(&db);
    db.store().unwrap().when_rank_checked(move || {
        write(&inner, vec![json!({ "op": "link", "source": "Task-33", "kind": "refs", "target": "Task-9" })]).unwrap();
    });
    write(&db, vec![json!({ "op": "link", "source": "Task-9", "kind": "refs", "target": "Task-20" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    let plans = *db.store().unwrap().rank_plans.lock();
    assert_eq!((plans.retried, plans.redone), (1, 0), "{plans:?}");
    assert!(plans.applied > 0, "{plans:?}");
    assert_within_bound(&db, "both", true);
}

#[test]
fn a_drain_stale_each_time_it_is_computed_is_computed_under_the_lock_in_the_end() {
    // A write of the index in every window: retried off the lock as many times as it takes to give up, and written all the
    // same
    fn arm(db: &std::sync::Arc<Database>, left: usize) {
        if left == 0 {
            return;
        }
        let inner = std::sync::Arc::clone(db);
        db.store().unwrap().when_rank_planned(move || {
            let (a, b) = (left, left + 3);
            write(
                &inner,
                vec![json!({ "op": "link", "source": format!("Task-{a}"), "kind": "refs", "target": format!("Task-{b}") })],
            )
            .unwrap();
            arm(&inner, left - 1);
        });
    }
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open_with(&dir, schema("both", true)));
    let mut ops: Vec<Json> =
        (0..34).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend(crate::engine::algorithms::tests::KARATE_EDGES.iter().map(
        |&(a, b)| json!({ "op": "link", "source": format!("Task-{a}"), "kind": "rates", "target": format!("Task-{b}"), "weight": 1 }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    arm(&db, 3);
    write(&db, vec![json!({ "op": "link", "source": "Task-9", "kind": "refs", "target": "Task-20" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    let store = db.store().unwrap();
    let plans = *store.rank_plans.lock();
    assert_eq!((plans.retried, plans.redone), (2, 1), "{plans:?}");
    store.when_rank_planned(|| {});
    assert_within_bound(&db, "both", true);
}

#[test]
fn a_drain_stale_once_is_written_from_its_retry_outside_the_lock() {
    let window = vec![json!({ "op": "link", "source": "Task-33", "kind": "refs", "target": "Task-9" })];
    let (db, _dir, _) = with_a_commit_during_a_drain(window);
    let plans = *db.store().unwrap().rank_plans.lock();
    assert_eq!((plans.retried, plans.redone), (1, 0), "{plans:?}");
    assert!(plans.applied > 0, "{plans:?}");
    assert_within_bound(&db, "both", true);
}

#[test]
fn commits_racing_drains_leave_the_index_within_its_bound() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open_with(&dir, schema("both", true)));
    let ops: Vec<Json> =
        (0..60).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let writers: Vec<_> = (0..3u64)
        .map(|w| {
            let db = std::sync::Arc::clone(&db);
            std::thread::spawn(move || {
                // A fixed walk over the pairs, each writer its own
                let mut at = w * 7 + 1;
                for i in 0..120u64 {
                    at = (at * 31 + 11) % 60;
                    let (source, target) = (format!("Task-{at}"), format!("Task-{}", (at + w + 1) % 60));
                    let op = if i % 5 == 4 { "unlink" } else { "link" };
                    write(&db, vec![json!({ "op": op, "source": source, "kind": "refs", "target": target })]).unwrap();
                    if i % 7 == 0 {
                        // Long enough for the builder's pause, so drains come between the writes
                        std::thread::sleep(std::time::Duration::from_millis(8));
                    }
                }
            })
        })
        .collect();
    for writer in writers {
        writer.join().unwrap();
    }
    assert_within_bound(&db, "both", true);
}

#[test]
fn a_drain_pushes_what_the_commits_pushes_would_after_a_node_leaves() {
    // A node leaving takes residual from its neighbours, so some that waited no longer do; the drain's sweep and the
    // commit's, through the store, push the same nodes in the same order, and only nodes still waiting: the same
    // estimates, ending at the same place
    let bits = |by_drain: bool| {
        let dir = tempfile::tempdir().unwrap();
        let db = open_with(&dir, schema("both", true));
        let mut ops: Vec<Json> =
            (0..34).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
        let edges = crate::engine::algorithms::tests::KARATE_EDGES;
        let weights = crate::engine::algorithms::tests::KARATE_WEIGHTS;
        ops.extend(edges.iter().zip(weights).map(|(&(a, b), w)| json!({ "op": "link", "source": format!("Task-{a}"), "kind": "rates", "target": format!("Task-{b}"), "weight": w })));
        write(&db, ops).unwrap();
        db.wait_for_index_builds().unwrap();
        // The hubs leave one at a time and the first comes back, each change settled before the next
        let mut bits = Vec::new();
        for (id, live) in [("Task-33", false), ("Task-0", false), ("Task-32", false), ("Task-33", true)] {
            db.admin_unit_with_checkpoint(
                |t| {
                    ranks::liveness(t, id, live)?;
                    if by_drain {
                        while ranks::drain(t, "taskRank", usize::MAX)? {}
                    } else {
                        while ranks::push(t, "taskRank", ranks::COMMIT_PUSHES)? {}
                    }
                    Ok(())
                },
                false,
            )
            .unwrap();
            bits.push(
                db.snapshot(|s| {
                    let estimates = (0..34)
                        .map(|i| Ok(ranks::node(s, "taskRank", &format!("Task-{i}"))?.map(|n| n.x.to_bits())))
                        .collect::<crate::Result<Vec<_>>>()?;
                    let totals = ranks::totals(s, "taskRank")?;
                    Ok((estimates, totals.swept))
                })
                .unwrap(),
            );
        }
        bits
    };
    let drained = bits(true);
    assert_eq!(drained, bits(false));
}

#[test]
fn the_top_is_read_from_the_index_only_for_its_own_types_in_its_order_and_scores_alike_either_way() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    let mut ops: Vec<Json> =
        (0..6).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((0..3).map(|i| json!({ "op": "create", "type": "Note", "id": format!("Note-{i}") })));
    ops.extend(
        (1..6).map(|i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": "Note-0" })),
    );
    ops.push(json!({ "op": "link", "source": "Note-0", "kind": "refs", "target": "Task-0" }));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let hits = |start: Json, scope: &str, limit: Option<u32>| {
        let mut steps = vec![json!({ "step": "ranked", "index": "taskRank" })];
        if let Some(n) = limit {
            steps.push(json!({ "step": "limit", "n": n }));
        }
        let answer =
            read(&db, json!({ "scope": scope, "start": start, "steps": steps, "terminal": { "answer": "hits" } }))
                .unwrap();
        answer["hits"].as_array().unwrap().iter().map(|h| (h["id"].clone(), h["score"].clone())).collect::<Vec<_>>()
    };
    let own = json!({ "from": "types", "types": ["Task", "Note"] });
    let all = hits(own.clone(), "live", None);
    // From the index's top: the same entities and scores as ranking the whole set
    assert_eq!(hits(own, "live", Some(4)), all[..4]);
    // Its types in another order tie otherwise: ranked over the set, not read from the index's top
    let reordered = hits(json!({ "from": "types", "types": ["Note", "Task"] }), "live", Some(9));
    let mut expected = all.clone();
    expected.sort_by(|a, b| b.1.as_f64().unwrap().total_cmp(&a.1.as_f64().unwrap()));
    let ties_note_first =
        reordered.iter().position(|h| h.0 == json!("Note-1")) < reordered.iter().position(|h| h.0 == json!("Task-1"));
    assert!(ties_note_first, "{reordered:?}");
    // A set of ids, one named twice, answers each once
    let named = hits(json!({ "from": "ids", "ids": ["Task-1", "Task-0", "Task-1"] }), "live", None);
    assert_eq!(named.len(), 2, "{named:?}");
}

#[test]
fn two_rank_indexes_keep_apart_and_index_status_names_each() {
    let dir = tempfile::tempdir().unwrap();
    let mut two = serde_json::to_value(schema("out", true)).unwrap();
    two["ranks"]["noteRank"] = json!({ "types": ["Note"], "relations": ["refs"], "direction": "in" });
    let db = open_with(&dir, Schema::from_json(&two).unwrap());
    let mut ops: Vec<Json> =
        (0..4).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((0..4).map(|i| json!({ "op": "create", "type": "Note", "id": format!("Note-{i}") })));
    ops.extend(
        (1..4).map(|i| json!({ "op": "link", "source": format!("Note-{i}"), "kind": "refs", "target": "Note-0" })),
    );
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let (task, note) = db.snapshot(|s| Ok((ranks::totals(s, "taskRank")?, ranks::totals(s, "noteRank")?))).unwrap();
    assert_eq!((task.nodes, note.nodes), (8, 4));
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    let status = db.index_status().unwrap();
    for name in ["taskRank", "noteRank"] {
        assert!(
            status.iter().any(|s| s.entity_type.is_none() && s.index == name && s.built && s.failure.is_none()),
            "{status:?}"
        );
    }
}

#[test]
fn a_rank_index_over_held_entities_is_left_to_the_builder_and_one_over_none_is_built_at_open() {
    let taskrank = |db: &Database| {
        db.index_status().unwrap().into_iter().find(|s| s.index == "taskRank").map(|s| s.built).unwrap()
    };
    // Over none: nothing to join, so the open that declares it builds it, and a read finds it built at once
    let empty = tempfile::tempdir().unwrap();
    assert!(taskrank(&open_with(&empty, schema("out", true))), "built by the open");
    // Over 20,000 tasks: the open leaves the join to the builder, which a unit begun at once holds off
    let dir = tempfile::tempdir().unwrap();
    {
        let mut plain = serde_json::to_value(schema("out", true)).unwrap();
        plain.as_object_mut().unwrap().remove("ranks");
        let db = open_with(&dir, Schema::from_json(&plain).unwrap());
        write(&db, (0..20_000).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect())
            .unwrap();
        db.close().unwrap();
    }
    let db = open_with(&dir, schema("out", true));
    let unit = db.begin().unwrap();
    assert!(!taskrank(&db), "the open joined 20,000 nodes inside its own commit");
    drop(unit);
    db.wait_for_index_builds().unwrap();
    assert!(taskrank(&db));
}

#[test]
fn an_index_within_its_tolerance_is_built_though_nodes_still_wait() {
    // A loose tolerance, so pushes stop with the residuals' sum within the budget and nodes still over the threshold
    let dir = tempfile::tempdir().unwrap();
    let mut loose = serde_json::to_value(schema("out", true)).unwrap();
    loose["ranks"]["taskRank"]["tolerance"] = json!(0.05);
    let db = open_with(&dir, Schema::from_json(&loose).unwrap());
    let mut ops: Vec<Json> =
        (0..40).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    // A ring with chords, around which residual keeps flowing rather than settling in one pass
    for i in 0..40 {
        for step in [1, 7] {
            ops.push(json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs",
                "target": format!("Task-{}", (i + step) % 40) }));
        }
    }
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let (totals, queued) = db
        .snapshot(|s| {
            let mut n = 0;
            crate::engine::unit::Read::visit(s, &keys::rank_queue("taskRank"), |_, _| {
                n += 1;
                true
            })?;
            Ok((ranks::totals(s, "taskRank")?, n))
        })
        .unwrap();
    assert!(queued > 0, "nodes still wait");
    assert!(totals.error(0.85) <= 0.05, "{}", totals.error(0.85));
    let status = db.index_status().unwrap();
    assert!(status.iter().any(|s| s.index == "taskRank" && s.built), "{status:?}");
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
}

/// The builder's edge mirror: the graph a drain read, kept by the commits after it, and emptied by any it can't follow
#[test]
fn the_drain_reads_the_graph_through_a_mirror_the_commits_keep_and_check_compares() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    let mut ops: Vec<Json> =
        (0..30).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((1..30).map(
        |i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": format!("Task-{}", i - 1) }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    let mirror = db.store().unwrap().mirror();
    let state = || {
        let m = mirror.lock();
        (m.sequence(), m.held())
    };
    // A change and its drain: the mirror is at the last commit and holds what the drain read
    write(&db, vec![json!({ "op": "link", "source": "Task-0", "kind": "refs", "target": "Task-29" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    let (sequence, held) = state();
    assert_eq!(sequence, Some(db.sequence().unwrap()), "kept to the last commit");
    assert!(held > 0);
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    // A unit's savepoint rolled back: its entries never reach the mirror, which still follows
    let mut unit = db.begin().unwrap();
    unit.write(&super::batch(vec![json!({ "op": "link", "source": "Task-5", "kind": "refs", "target": "Task-20" })]))
        .unwrap();
    let depth = unit.savepoint().unwrap();
    unit.write(&super::batch(vec![json!({ "op": "link", "source": "Task-6", "kind": "refs", "target": "Task-21" })]))
        .unwrap();
    unit.rollback_to(depth).unwrap();
    unit.commit(false).unwrap();
    db.wait_for_index_builds().unwrap();
    assert_eq!(state().0, Some(db.sequence().unwrap()));
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    // check() names a list that strays from the store
    mirror.lock().lose_an_entry();
    let said: Vec<String> = db.check().unwrap().violations.into_iter().map(|v| v.message).collect();
    assert!(said.iter().any(|m| m.contains("edge mirror")), "{said:?}");
    // A write outside the engine's units, and an admin unit, each empty it
    db.store().unwrap().write(|_, _| Ok(()), false).unwrap();
    assert_eq!(state(), (None, 0));
    write(&db, vec![json!({ "op": "unlink", "source": "Task-0", "kind": "refs", "target": "Task-29" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(state().1 > 0, "a drain fills it again");
    db.rebuild(None).unwrap();
    assert_eq!(state(), (None, 0), "emptied by the admin unit, whose own drain reads the store");
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
}

/// The child of `another_processs_commit_between_two_drains_empties_the_mirror`: one edge linked at an entity whose
/// list the parent's mirror holds, its drain finished, and the database closed
#[test]
fn child_links_one_edge_the_parents_mirror_holds() {
    let Some(path) = std::env::var_os("ELMDB_MIRROR_CHILD") else { return };
    let db =
        Database::open(path, OpenOptions { schema: Some(schema("out", true)), syncs: false, ..Default::default() })
            .unwrap();
    db.write(
        &super::batch(vec![json!({ "op": "link", "source": "Task-5", "kind": "refs", "target": "Task-25" })]),
        false,
    )
    .unwrap();
    db.wait_for_index_builds().unwrap();
    db.close().unwrap();
    println!("linked");
}

#[test]
fn another_processs_commit_between_two_drains_empties_the_mirror() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    let mut ops: Vec<Json> =
        (0..30).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend((1..30).map(
        |i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": format!("Task-{}", i - 1) }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    write(&db, vec![json!({ "op": "link", "source": "Task-0", "kind": "refs", "target": "Task-29" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(db.store().unwrap().mirror().lock().held() > 0, "the drain filled the mirror");
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "engine::tests::rank_index::child_links_one_edge_the_parents_mirror_holds", "--nocapture"])
        .env("ELMDB_MIRROR_CHILD", dir.path())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("linked"), "{}", String::from_utf8_lossy(&out.stderr));
    // This process's next commit reads the child's sequence as its base, which isn't the mirror's
    write(&db, vec![json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-28" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(db.check().unwrap().is_clean(), "{:?}", db.check().unwrap());
    assert_within_bound(&db, "out", true);
}

#[test]
fn a_read_only_open_drains_nothing() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open_with(&dir, schema("out", true));
        write(&db, (0..3).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect())
            .unwrap();
        db.wait_for_index_builds().unwrap();
        // Unbuilt again: an open able to write would build it
        db.admin_unit_with_checkpoint(|t| ranks::clear(t, "taskRank"), false).unwrap();
        db.close().unwrap();
    }
    let db = Database::open(dir.path(), OpenOptions { read_only: true, ..Default::default() }).unwrap();
    // No build started, so none failed for want of a write, and the index is as the writer left it
    db.wait_for_index_builds().unwrap();
    let status = db.index_status().unwrap();
    assert!(status.iter().any(|s| s.index == "taskRank" && !s.built && s.failure.is_none()), "{status:?}");
}

#[test]
fn a_tracked_read_hears_what_the_builder_writes_into_it_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("out", true));
    let mut ops: Vec<Json> =
        (0..200).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.push(json!({ "op": "create", "type": "Tag", "id": "Tag-0" }));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();

    let tracked = |plan: Json, token: u32| -> Reads {
        serde_json::from_value(json!({ "reads": [{ "read": "query", "plan": plan }], "track": token })).unwrap()
    };
    let top = json!({ "start": { "from": "types", "types": ["Task", "Note"] },
        "steps": [{ "step": "ranked", "index": "taskRank" }, { "step": "limit", "n": 5 }], "terminal": { "answer": "ids" } });
    let tags = json!({ "start": { "from": "types", "types": ["Tag"] }, "terminal": { "answer": "ids" } });
    db.read(&tracked(top, 7)).unwrap();
    db.read(&tracked(tags, 8)).unwrap();

    let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(u64, Vec<u32>)>::new()));
    let hearing = std::sync::Arc::clone(&heard);
    db.hear_builder_commits(Some(std::sync::Arc::new(move |sequence, touched| {
        hearing.lock().unwrap().push((sequence, touched));
    })))
    .unwrap();
    // One at a time, so the first isn't left unheard
    let second = db.hear_builder_commits(Some(std::sync::Arc::new(|_, _| {}))).unwrap_err();
    assert_eq!(second.code, ErrorCode::AlreadyListening);

    let links: Vec<Json> = (1..200)
        .map(|i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": "Task-0" }))
        .collect();
    write(&db, links).unwrap();
    db.wait_for_index_builds().unwrap();

    let commits = heard.lock().unwrap().clone();
    assert!(!commits.is_empty(), "the drain's commits told nobody");
    assert!(commits.iter().all(|(_, touched)| touched == &vec![7]), "{commits:?}");
    assert!(commits.windows(2).all(|pair| pair[0].0 < pair[1].0), "{commits:?}");
    assert!(commits.iter().all(|(sequence, _)| *sequence <= db.sequence().unwrap()), "{commits:?}");

    // A commit that wrote into no tracked read tells nobody
    heard.lock().unwrap().clear();
    db.untrack(7);
    let unlinks: Vec<Json> = (2..200)
        .map(|i| json!({ "op": "unlink", "source": format!("Task-{i}"), "kind": "refs", "target": "Task-0" }))
        .collect();
    write(&db, unlinks).unwrap();
    let written = db.sequence().unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(db.sequence().unwrap() > written, "the builder committed nothing, so this looked at nothing");
    assert!(heard.lock().unwrap().is_empty(), "{:?}", heard.lock().unwrap());

    // Stops at the caller's word
    db.read(&tracked(json!({ "start": { "from": "types", "types": ["Task", "Note"] },
        "steps": [{ "step": "ranked", "index": "taskRank" }, { "step": "limit", "n": 5 }], "terminal": { "answer": "ids" } }), 7)).unwrap();
    db.hear_builder_commits(None).unwrap();
    write(&db, vec![json!({ "op": "unlink", "source": "Task-1", "kind": "refs", "target": "Task-0" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(heard.lock().unwrap().is_empty());

    // Closing drops whoever is listening
    let listener: crate::engine::watch::BuilderCommitListener = std::sync::Arc::new(|_, _| {});
    db.hear_builder_commits(Some(std::sync::Arc::clone(&listener))).unwrap();
    assert_eq!(std::sync::Arc::strong_count(&listener), 2);
    db.close().unwrap();
    assert_eq!(std::sync::Arc::strong_count(&listener), 1);
}

/// What a process leaves when it exits straight after a commit: the commit's pushes owed, its builder gone
#[test]
#[ignore = "run by leaves_an_owed_index, as a process of its own"]
fn child_exits_owing_a_rank_index() {
    let Ok(path) = std::env::var("ELMDB_OWING_DB") else { return };
    let db = Database::open(
        std::path::Path::new(&path),
        OpenOptions { schema: Some(near_schema()), syncs: false, ..Default::default() },
    )
    .unwrap();
    let ops = (0..400)
        .flat_map(|i| {
            (6..12).map(move |step| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": format!("Task-{}", (i + step) % 400) }))
        })
        .collect();
    write(&db, ops).unwrap();
    std::process::exit(0);
}

fn run_owing_child(dir: &tempfile::TempDir) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "engine::tests::rank_index::child_exits_owing_a_rank_index",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ELMDB_OWING_DB", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the owing process failed");
}

fn owes(db: &Database) -> bool {
    db.snapshot(|s| ranks::owes_work(s, "taskRank")).unwrap()
}

#[test]
fn being_woken_finishes_what_a_process_left_owed_without_a_commit_here() {
    let dir = tempfile::tempdir().unwrap();
    let db = near_links(&dir, 400, 5, 0);
    let (told, heard) = std::sync::mpsc::channel();
    db.hear_commits_elsewhere(std::sync::Arc::new(move |sequence| told.send(sequence).unwrap())).unwrap();
    run_owing_child(&dir);
    heard.recv_timeout(std::time::Duration::from_secs(10)).expect("woken");
    db.wait_for_index_builds().unwrap();
    assert!(!owes(&db), "drained without a commit here");
}

#[test]
fn a_commit_here_finishes_what_a_process_that_exited_left_owed() {
    let dir = tempfile::tempdir().unwrap();
    let db = near_links(&dir, 400, 5, 0);
    // Its own work done first, so a builder still draining it can't take the other process's work as well
    db.wait_for_index_builds().unwrap();
    run_owing_child(&dir);
    assert!(owes(&db), "the process left the index owed");
    // A commit that pushes nothing of its own: only finding the other process's commit queues the work
    let mut unit = db.begin().unwrap();
    unit.commit(false).unwrap();
    db.wait_for_index_builds().unwrap();
    assert!(!owes(&db), "drained after a commit here, with no wake heard");
}

#[test]
fn a_close_refused_for_an_open_unit_goes_on_hearing_other_processes() {
    let dir = tempfile::tempdir().unwrap();
    let db = near_links(&dir, 400, 5, 0);
    let (told, heard) = std::sync::mpsc::channel();
    db.hear_commits_elsewhere(std::sync::Arc::new(move |sequence| told.send(sequence).unwrap())).unwrap();
    let unit = db.begin().unwrap();
    assert_eq!(db.close().unwrap_err().code, crate::error::ErrorCode::UnitOpen);
    drop(unit);
    run_owing_child(&dir);
    heard.recv_timeout(std::time::Duration::from_secs(10)).expect("still woken");
}
