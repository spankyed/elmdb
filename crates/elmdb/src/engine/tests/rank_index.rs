//! Rank indexes (`docs/roadmap/rank-index.md`): scores kept current by every commit, within the bound they report of
//! the PageRank `rank` computes over the same graph; read by `ranked`, the top of them read alone; refused while
//! building; held to that bound and to their own invariant by `check()`.

use proptest::prelude::*;
use serde_json::{Value as Json, json};

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
fn the_totals_are_counted_afresh_once_as_many_pushes_as_nodes_have_added_to_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_with(&dir, schema("both", true));
    let mut ops: Vec<Json> =
        (0..34).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect();
    ops.extend(crate::engine::algorithms::tests::KARATE_EDGES.iter().map(
        |&(a, b)| json!({ "op": "link", "source": format!("Task-{a}"), "kind": "refs", "target": format!("Task-{b}") }),
    ));
    write(&db, ops).unwrap();
    db.wait_for_index_builds().unwrap();
    // The estimates' sum off by less than `check()` tells, and `pushes` pushes made since the last count
    let settled = |pushes: u64| {
        let found = std::cell::Cell::new((f64::NAN, u64::MAX));
        db.admin_unit_with_checkpoint(
            |t| {
                let key = keys::rank_totals("taskRank");
                let mut bytes = t.get(&key)?.unwrap();
                let estimates = f64::from_le_bytes(bytes[8..16].try_into().unwrap()) + 1e-12;
                bytes[8..16].copy_from_slice(&estimates.to_le_bytes());
                bytes[24..32].copy_from_slice(&pushes.to_le_bytes());
                t.put(&key, &bytes)?;
                ranks::settle(t, "taskRank")?;
                let mut counted = 0.0;
                t.visit(&keys::rank_nodes("taskRank"), |_, bytes| {
                    counted += ranks::Node::decode(bytes).unwrap().x;
                    true
                })?;
                let totals = ranks::totals(t, "taskRank")?;
                found.set((totals.estimates - counted, totals.pushes));
                Ok(())
            },
            false,
        )
        .unwrap();
        found.get()
    };
    let (off, pushes) = settled(33);
    assert!(off > 0.0 && pushes == 33, "fewer pushes than nodes counted the totals afresh: {off} {pushes}");
    assert_eq!(settled(34), (0.0, 0), "as many pushes as nodes left the totals uncounted");
}

#[test]
fn a_drains_pushes_count_toward_the_totals_being_counted_afresh() {
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
    // The estimates' sum off by less than `check()` tells, with more pushes owed before a count than a commit makes
    let left = n - ranks::COMMIT_PUSHES as u64 - 1;
    db.store()
        .unwrap()
        .write(
            |txn, d| {
                let key = keys::rank_totals("taskRank");
                let mut bytes = d.main.get(txn, &key)?.unwrap().to_vec();
                let estimates = f64::from_le_bytes(bytes[8..16].try_into().unwrap()) + 1e-12;
                bytes[8..16].copy_from_slice(&estimates.to_le_bytes());
                bytes[24..32].copy_from_slice(&left.to_le_bytes());
                d.main.put(txn, &key, &bytes)?;
                Ok(())
            },
            false,
        )
        .unwrap();
    // A link far along the chain: the commit's pushes leave some to the drain, whose pushes complete the count
    write(&db, vec![json!({ "op": "link", "source": "Task-0", "kind": "refs", "target": "Task-150" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    let (totals, counted) = db
        .snapshot(|s| {
            let mut counted = 0.0;
            s.visit(&keys::rank_nodes("taskRank"), |_, bytes| {
                counted += ranks::Node::decode(bytes).unwrap().x;
                true
            })?;
            Ok((ranks::totals(s, "taskRank")?, counted))
        })
        .unwrap();
    assert_eq!(totals.estimates, counted, "the drain's pushes left the totals uncounted ({} since)", totals.pushes);
    // From a count, the next drain's pushes are what is counted since
    write(&db, vec![json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-120" })]).unwrap();
    db.wait_for_index_builds().unwrap();
    let since = db.snapshot(|s| Ok(ranks::totals(s, "taskRank")?.pushes)).unwrap();
    assert!(since > 0 && since < n, "{since} pushes counted since the last count");
}

#[test]
fn a_drain_pushes_what_the_commits_pushes_would_after_a_node_leaves() {
    // A node leaving takes residual from its neighbours, so some that waited no longer do; the drain's sweep and the
    // commit's, through the store, push the same nodes in the same order, and only nodes still waiting
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
                    (0..34)
                        .map(|i| Ok(ranks::node(s, "taskRank", &format!("Task-{i}"))?.map(|n| n.x.to_bits())))
                        .collect::<crate::Result<Vec<_>>>()
                })
                .unwrap(),
            );
        }
        bits
    };
    let drained: Vec<Vec<Option<u64>>> = bits(true);
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
