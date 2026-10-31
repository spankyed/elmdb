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
/// 2026-10-31
const KARATE_HISTORY_BITS: [u64; 34] = [
    4613673579926782254,
    4610821908507679268,
    4611997313766654983,
    4608420309496835819,
    4604513055727937687,
    4607900079068323918,
    4607548093699354791,
    4606344388540204292,
    4607739974523240813,
    4599412107090191155,
    4604565980820253274,
    4599726176833251323,
    4600744756591145578,
    4607823236193910293,
    4601446465954465685,
    4603085726173937314,
    4603338583676891793,
    4599653484846277290,
    4599428786595022239,
    4601721342182633847,
    4600423382344164836,
    4600698258572450612,
    4601460709885148115,
    4608865384604963893,
    4603221709645400933,
    4606851536761028675,
    4602772274932736575,
    4606406259886602508,
    4602458881637634411,
    4606674512061367489,
    4605137304699968546,
    4609023951793800059,
    4613413487410362999,
    4615005999349841167,
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
    let waiting = |db: &Database| {
        db.snapshot(|s| {
            let mut n = 0;
            crate::engine::unit::Read::visit(s, &keys::rank_queue("taskRank"), |_, _| {
                n += 1;
                true
            })?;
            Ok(n)
        })
        .unwrap()
    };
    // Read-only, which starts no drain: the kill left pushes waiting
    let looked = Database::open(dir.path(), OpenOptions { read_only: true, ..Default::default() }).unwrap();
    assert!(waiting(&looked) > 0, "the child was killed before its drain finished");
    looked.close().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions { schema: Some(schema("out", true)), syncs: false, ..Default::default() },
    )
    .unwrap();
    db.wait_for_index_builds().unwrap();
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(waiting(&db), 0, "the drain resumed on open and finished");
    assert!(db.snapshot(|s| ranks::is_built(s, "taskRank")).unwrap());
}
