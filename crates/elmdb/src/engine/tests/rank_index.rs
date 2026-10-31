//! Rank indexes (`docs/roadmap/rank-index.md`): scores kept current by every commit, within the bound they report of
//! the PageRank `rank` computes over the same graph; read by `ranked`, the top of them read alone; refused while
//! building; held to that bound and to their own invariant by `check()`.

use proptest::prelude::*;
use serde_json::{Value as Json, json};

use crate::engine::plan::{ReadOp, Reads};
use crate::engine::{keys, ranks};
use crate::error::ErrorCode;
use crate::schema::Schema;
use crate::{Database, OpenOptions};

fn schema(direction: &str, weights: bool) -> Schema {
    Schema::from_json(&json!({
        "$schema": crate::schema::SCHEMA_LANGUAGE,
        "relations": {
            "refs": { "cardinality": "many-to-many" },
            "weighs": { "cardinality": "many-to-many", "weighted": true },
            "rates": { "cardinality": "many-to-many", "weighted": true, "unique": true }
        },
        "types": { "Task": { "fields": {} }, "Note": { "fields": {} } },
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

fn read(db: &Database, plan: Json) -> crate::Result<Json> {
    let reads = Reads {
        reads: vec![serde_json::from_value::<ReadOp>(json!({ "read": "query", "plan": plan })).unwrap()],
        track: None,
    };
    db.read(&reads)
        .map(|bytes| serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()[0].clone())
}

/// `id → score` from a `hits` answer, and the answer's `rank` record
fn scores(answer: &Json) -> (Vec<(String, f64)>, Json) {
    let hits = answer["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["id"].as_str().unwrap().to_owned(), h["score"].as_f64().unwrap()));
    (hits.collect(), answer["rank"].clone())
}

/// The index's scores and bound, after its pushes are done
fn indexed(db: &Database) -> (Vec<(String, f64)>, f64) {
    db.wait_for_index_builds().unwrap();
    let answer = read(
        db,
        json!({ "start": { "from": "types", "types": ["Task", "Note"] },
        "steps": [{ "step": "ranked", "index": "taskRank" }], "terminal": { "answer": "hits" } }),
    )
    .unwrap();
    let (mut found, rank) = scores(&answer);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    (found, rank["error"].as_f64().unwrap())
}

/// `rank` over the same graph, every entity a seed, run far past the index's tolerance
fn computed(db: &Database, direction: &str, weights: bool) -> Vec<(String, f64)> {
    let answer = read(
        db,
        json!({ "start": { "from": "types", "types": ["Task", "Note"] },
        "steps": [{ "step": "rank", "kinds": ["refs", "weighs", "rates"], "direction": direction, "weights": weights,
                   "iterations": 100000, "tolerance": 1e-14 }],
        "terminal": { "answer": "hits" } }),
    )
    .unwrap();
    let (mut found, _) = scores(&answer);
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// The index's scores against `rank`'s: the same entities, within the bound the index reports, and in `rank`'s order
/// wherever two differ by more than twice it
fn assert_within_bound(db: &Database, direction: &str, weights: bool) {
    let (found, error) = indexed(db);
    let truth = computed(db, direction, weights);
    assert_eq!(found.iter().map(|f| &f.0).collect::<Vec<_>>(), truth.iter().map(|t| &t.0).collect::<Vec<_>>());
    let distance: f64 = found.iter().zip(&truth).map(|(f, t)| (f.1 - t.1).abs()).sum();
    assert!(distance <= error + 1e-12, "{distance} past the bound {error}: {found:?} against {truth:?}");
    for (a, ta) in found.iter().zip(&truth) {
        for (b, tb) in found.iter().zip(&truth) {
            if ta.1 - tb.1 > 2.0 * error + 1e-12 {
                assert!(a.1 > b.1, "{} above {} by rank, not by the index", a.0, b.0);
            }
        }
    }
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
        // A parallel edge, a weight replaced, an edge removed
        write(&db, vec![link("Task-1", "weighs", "Task-2", Some(1.5)), link("Note-a", "rates", "Task-0", Some(2.0))])
            .unwrap();
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
        (0..20).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend(
        (1..20).map(|i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": "Task-0" })),
    );
    ops.extend(
        (2..20).map(|i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": "Task-1" })),
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
        json!({ "start": { "from": "ids", "ids": ["Task-0", "Task-404"] },
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
    // An index whose build hasn't finished has nothing to answer from
    db.store()
        .unwrap()
        .write(|txn, d| d.delete(txn, &keys::rank_totals("taskRank")).map(|_| ()).map_err(Into::into), false)
        .unwrap();
    let e = read(
        &db,
        json!({ "start": { "from": "types", "types": ["Task"] },
        "steps": [{ "step": "ranked", "index": "taskRank" }], "terminal": { "answer": "ids" } }),
    )
    .unwrap_err();
    assert_eq!(e.code, ErrorCode::IndexBuilding);
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
