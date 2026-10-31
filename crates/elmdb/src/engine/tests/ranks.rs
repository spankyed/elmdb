//! The graph algorithms as plan steps (`docs/roadmap/graph-algorithms.md`): `rank` and the `weakComponents` answer over the
//! set a plan built, case by case, and the graph each builds from the store against one built by hand from the same
//! random edges.

use proptest::prelude::*;
use serde_json::{Value as Json, json};

use super::{batch, open, write};
use crate::Database;
use crate::engine::algorithms::{self, Projection};
use crate::engine::plan::{ReadOp, Reads};
use crate::error::ErrorCode;

fn decode(bytes: &[u8]) -> Json {
    serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()
}

fn reads(ops: Vec<Json>, track: Option<u32>) -> Reads {
    Reads { reads: ops.into_iter().map(|op| serde_json::from_value::<ReadOp>(op).unwrap()).collect(), track }
}

fn try_query(db: &Database, plan: Json) -> crate::Result<Json> {
    db.read(&reads(vec![json!({ "read": "query", "plan": plan })], None)).map(|b| decode(&b)[0].clone())
}

fn query(db: &Database, plan: Json) -> Json {
    try_query(db, plan).unwrap()
}

/// Every task, ranked by these options, answered as hits
fn ranked(db: &Database, rank: Json) -> Json {
    let mut step = json!({ "step": "rank", "kinds": ["refs"] });
    step.as_object_mut().unwrap().extend(rank.as_object().unwrap().clone());
    query(
        db,
        json!({ "start": { "from": "types", "types": ["Task"] }, "steps": [step], "terminal": { "answer": "hits" } }),
    )
}

fn order(hits: &Json) -> Vec<&str> {
    hits["hits"].as_array().unwrap().iter().map(|h| h["id"].as_str().unwrap()).collect()
}

fn tasks(db: &Database, names: &[&str]) {
    write(db, names.iter().map(|n| json!({ "op": "create", "type": "Task", "id": n })).collect()).unwrap();
}

fn link(db: &Database, edges: &[(&str, &str, &str)]) {
    let ops = edges.iter().map(|(s, k, t)| json!({ "op": "link", "source": s, "kind": k, "target": t })).collect();
    write(db, ops).unwrap();
}

/// a → b → c, and d → c: c is where every walk ends up, and a is reached by nothing
fn chain(db: &Database) {
    tasks(db, &["Task-a", "Task-b", "Task-c", "Task-d"]);
    link(db, &[("Task-a", "refs", "Task-b"), ("Task-b", "refs", "Task-c"), ("Task-d", "refs", "Task-c")]);
}

#[test]
fn rank_orders_the_set_by_score_and_hits_answers_the_scores() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    chain(&db);
    let hits = ranked(&db, json!({}));
    assert_eq!(order(&hits)[0], "Task-c", "{hits}");
    let scores: Vec<f64> = hits["hits"].as_array().unwrap().iter().map(|h| h["score"].as_f64().unwrap()).collect();
    assert!(scores.windows(2).all(|pair| pair[0] >= pair[1]), "highest first: {scores:?}");
    assert!((scores.iter().sum::<f64>() - 1.0).abs() < 1e-9);
    assert_eq!(hits["rank"]["converged"], json!(true));
    // a and d score alike, nothing reaching either: the set's order breaks the tie
    let a = order(&hits).iter().position(|id| *id == "Task-a").unwrap();
    let d = order(&hits).iter().position(|id| *id == "Task-d").unwrap();
    assert!(a < d);
}

#[test]
fn seeds_personalize_the_rank_and_a_set_without_one_answers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    chain(&db);
    // From d the walker reaches c alone, so a and b score nothing
    let from_d = ranked(&db, json!({ "seeds": ["Task-d"] }));
    let zero: Vec<&str> = from_d["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["score"].as_f64() == Some(0.0))
        .map(|h| h["id"].as_str().unwrap())
        .collect();
    assert_eq!(zero, ["Task-a", "Task-b"]);
    assert_eq!(order(&ranked(&db, json!({ "seeds": ["Task-d", "Task-gone"] }))), order(&from_d), "unknown ignored");
    let none = ranked(&db, json!({ "seeds": ["Task-gone"] }));
    assert_eq!(none["hits"], json!([]));
    assert_eq!(none["rank"], json!({ "converged": true, "iterations": 0 }), "a rank that ran says so");
    // Followed both ways, from a the walk reaches d through c
    let both = ranked(&db, json!({ "seeds": ["Task-a"], "direction": "both" }));
    assert!(both["hits"].as_array().unwrap().iter().all(|h| h["score"].as_f64().unwrap() > 0.0), "{both}");
}

#[test]
fn a_search_after_a_rank_answers_its_own_scores_and_not_how_the_rank_settled() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(
        &db,
        vec![
            json!({ "op": "create", "type": "Note", "id": "Note-a", "row": { "a": "apple" } }),
            json!({ "op": "create", "type": "Note", "id": "Note-b", "row": { "a": "apple pie" } }),
            json!({ "op": "link", "source": "Note-a", "kind": "refs", "target": "Note-b" }),
        ],
    )
    .unwrap();
    let hits = |steps: Json| {
        query(
            &db,
            json!({ "start": { "from": "types", "types": ["Note"] }, "steps": steps, "terminal": { "answer": "hits" } }),
        )
    };
    let rank = json!({ "step": "rank", "kinds": ["refs"] });
    let search = json!({ "step": "search", "query": "apple", "index": "text" });
    assert!(hits(json!([rank.clone()])).get("rank").is_some());
    let searched = hits(json!([rank.clone(), search.clone()]));
    assert!(searched.get("rank").is_none(), "{searched}");
    assert_eq!(hits(json!([search, rank])).get("rank").unwrap()["converged"], json!(true));
}

#[test]
fn rank_follows_only_edges_inside_the_set_and_the_scope() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    chain(&db);
    // A rank of a and b alone: the edge b → c leaves the set, so b holds what it is given
    let pair = query(
        &db,
        json!({ "start": { "from": "ids", "ids": ["Task-a", "Task-b"] }, "steps": [{ "step": "rank", "kinds": ["refs"] }],
            "terminal": { "answer": "hits" } }),
    );
    assert_eq!(order(&pair), ["Task-b", "Task-a"]);
    write(&db, vec![json!({ "op": "trash", "id": "Task-c" })]).unwrap();
    let live = ranked(&db, json!({}));
    assert!(!order(&live).contains(&"Task-c"), "trashed, out of a live set: {live}");
}

#[test]
fn rank_refuses_what_it_cannot_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    chain(&db);
    let plan = |step: Json| json!({ "start": { "from": "types", "types": ["Task"] }, "steps": [step], "terminal": { "answer": "ids" } });
    let unknown = try_query(&db, plan(json!({ "step": "rank", "kinds": ["nope"] }))).unwrap_err();
    assert_eq!(unknown.code, ErrorCode::UnknownRelationKind);
    for bad in
        [json!({ "damping": 1.0 }), json!({ "damping": 0.0 }), json!({ "iterations": 0 }), json!({ "tolerance": 0.0 })]
    {
        let mut step = json!({ "step": "rank", "kinds": ["refs"] });
        step.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
        assert_eq!(try_query(&db, plan(step)).unwrap_err().code, ErrorCode::BatchInvalid, "{bad}");
    }
    let no_rank = json!({ "start": { "from": "types", "types": ["Task"] }, "terminal": { "answer": "hits" } });
    assert_eq!(try_query(&db, no_rank).unwrap_err().code, ErrorCode::BatchInvalid);
}

#[test]
fn a_rank_cut_short_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    chain(&db);
    let cut = ranked(&db, json!({ "iterations": 1 }));
    assert_eq!(cut["rank"], json!({ "converged": false, "iterations": 1 }));
}

#[test]
fn components_answer_each_connected_group_largest_first() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b", "Task-c", "Task-d", "Task-e"]);
    // {a, b}, {c, d, e} whichever way the edges point; a blocks e, a kind the answer isn't asked about
    link(
        &db,
        &[
            ("Task-b", "refs", "Task-a"),
            ("Task-c", "refs", "Task-e"),
            ("Task-d", "refs", "Task-e"),
            ("Task-a", "blocks", "Task-e"),
        ],
    );
    let all = |counts: bool| {
        query(
            &db,
            json!({ "start": { "from": "types", "types": ["Task"] },
                "terminal": { "answer": "weakComponents", "kinds": ["refs"], "counts": counts } }),
        )
    };
    assert_eq!(all(false)["weakComponents"], json!([["Task-c", "Task-d", "Task-e"], ["Task-a", "Task-b"]]));
    assert_eq!(all(true)["weakComponents"], json!([3, 2]));
    let both = query(
        &db,
        json!({ "start": { "from": "types", "types": ["Task"] }, "terminal": { "answer": "weakComponents", "kinds": ["refs", "blocks"] } }),
    );
    assert_eq!(both["weakComponents"], json!([["Task-a", "Task-b", "Task-c", "Task-d", "Task-e"]]));
    // Only edges between two of the set's entities join: without e, c and d are apart
    let some = query(
        &db,
        json!({ "start": { "from": "ids", "ids": ["Task-c", "Task-d"] }, "terminal": { "answer": "weakComponents", "kinds": ["refs"] } }),
    );
    assert_eq!(some["weakComponents"], json!([["Task-c"], ["Task-d"]]));
}

#[test]
fn explain_reports_the_graph_each_built() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    chain(&db);
    let explained = decode(
        &db.read(&reads(
            vec![json!({ "read": "query", "explain": true, "plan": {
                "start": { "from": "types", "types": ["Task"] },
                "steps": [{ "step": "rank", "kinds": ["refs"] }],
                "terminal": { "answer": "weakComponents", "kinds": ["refs"] }
            } })],
            None,
        ))
        .unwrap(),
    );
    let stages = explained[0]["stages"].as_array().unwrap();
    assert_eq!(stages[1]["stage"], json!("rank"));
    assert_eq!(
        (&stages[1]["nodes"], &stages[1]["edges"], &stages[1]["converged"]),
        (&json!(4), &json!(3), &json!(true))
    );
    assert!(stages[1]["iterations"].as_u64().unwrap() > 1);
    assert_eq!(
        (&stages[2]["stage"], &stages[2]["nodes"], &stages[2]["edges"]),
        (&json!("weakComponents"), &json!(4), &json!(3))
    );
    assert!(stages[2].get("iterations").is_none());
    assert!(stages[0].get("nodes").is_none(), "a stage that built no graph says nothing of one");
}

#[test]
fn a_tracked_rank_is_written_into_by_an_edge_between_two_of_its_entities() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    chain(&db);
    tasks(&db, &["Task-y", "Task-z"]);
    let rank = json!({ "read": "query", "plan": {
        "start": { "from": "ids", "ids": ["Task-a", "Task-b", "Task-c"] },
        "steps": [{ "step": "rank", "kinds": ["refs"] }],
        "terminal": { "answer": "ids" }
    } });
    db.read(&reads(vec![rank.clone()], Some(1))).unwrap();
    let touched = |edge: (&str, &str)| {
        db.write(&batch(vec![json!({ "op": "link", "source": edge.0, "kind": "refs", "target": edge.1 })]), false)
            .unwrap()
            .1
            .touched
    };
    assert!(touched(("Task-y", "Task-z")).is_empty(), "entities outside its set");
    assert_eq!(touched(("Task-c", "Task-a")), [1], "an edge inside its set");
    db.read(&reads(vec![rank.clone()], Some(1))).unwrap();
    let unlinked = db
        .write(&batch(vec![json!({ "op": "unlink", "source": "Task-a", "kind": "refs", "target": "Task-b" })]), false)
        .unwrap()
        .1
        .touched;
    assert_eq!(unlinked, [1], "an edge inside its set removed");
    db.read(&reads(vec![rank.clone()], Some(1))).unwrap();
    let other = db
        .write(&batch(vec![json!({ "op": "link", "source": "Task-a", "kind": "blocks", "target": "Task-b" })]), false)
        .unwrap()
        .1
        .touched;
    assert!(other.is_empty(), "a kind the rank doesn't follow");
}

#[test]
fn a_tracked_weak_components_read_is_written_into_by_an_edge_between_two_of_its_entities() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    chain(&db);
    tasks(&db, &["Task-y", "Task-z"]);
    let weak = json!({ "read": "query", "plan": {
        "start": { "from": "ids", "ids": ["Task-a", "Task-b", "Task-c"] },
        "terminal": { "answer": "weakComponents", "kinds": ["refs"] }
    } });
    db.read(&reads(vec![weak], Some(1))).unwrap();
    let touched = |edge: (&str, &str)| {
        db.write(&batch(vec![json!({ "op": "link", "source": edge.0, "kind": "refs", "target": edge.1 })]), false)
            .unwrap()
            .1
            .touched
    };
    assert!(touched(("Task-y", "Task-z")).is_empty(), "entities outside its set");
    assert_eq!(touched(("Task-c", "Task-a")), [1], "an edge inside its set");
}

#[test]
fn communities_answer_the_densely_linked_groups_the_same_each_time() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let names: Vec<String> = (0..8).map(|n| format!("Task-{n}")).collect();
    tasks(&db, &names.iter().map(String::as_str).collect::<Vec<_>>());
    // Two tight squares, 0-3 and 4-7, joined by one edge, 3 → 4
    let mut edges = Vec::new();
    for base in [0, 4] {
        for a in base..base + 4 {
            for b in a + 1..base + 4 {
                edges.push((names[a].clone(), names[b].clone()));
            }
        }
    }
    edges.push((names[3].clone(), names[4].clone()));
    let refs: Vec<(&str, &str, &str)> = edges.iter().map(|(s, t)| (s.as_str(), "refs", t.as_str())).collect();
    link(&db, &refs);
    let answer =
        |terminal: Json| query(&db, json!({ "start": { "from": "types", "types": ["Task"] }, "terminal": terminal }));
    let found = answer(json!({ "answer": "communities", "kinds": ["refs"] }));
    assert_eq!(
        found["communities"],
        json!([["Task-0", "Task-1", "Task-2", "Task-3"], ["Task-4", "Task-5", "Task-6", "Task-7"]])
    );
    assert_eq!(
        answer(json!({ "answer": "communities", "kinds": ["refs"], "seed": 7 }))["communities"],
        found["communities"]
    );
    // At resolution 0 every edge only adds, so the one component is one community
    let whole = answer(json!({ "answer": "communities", "kinds": ["refs"], "resolution": 0.0 }));
    assert_eq!(whole["communities"].as_array().unwrap().len(), 1);
    for bad in [json!({ "resolution": -1.0 }), json!({ "iterations": 0 })] {
        let mut terminal = json!({ "answer": "communities", "kinds": ["refs"] });
        terminal.as_object_mut().unwrap().extend(bad.as_object().unwrap().clone());
        let plan = json!({ "start": { "from": "types", "types": ["Task"] }, "terminal": terminal });
        assert_eq!(try_query(&db, plan).unwrap_err().code, ErrorCode::BatchInvalid, "{bad}");
    }
    let unknown = json!({ "start": { "from": "types", "types": ["Task"] }, "terminal": { "answer": "communities", "kinds": ["nope"] } });
    assert_eq!(try_query(&db, unknown).unwrap_err().code, ErrorCode::UnknownRelationKind);
}

const NODES: usize = 7;
const KINDS: [&str; 2] = ["refs", "blocks"];

fn name(n: usize) -> String {
    format!("Task-{n}")
}

/// A random graph over a few tasks and two kinds, each edge once; the engine refuses a self-link
fn graph() -> impl Strategy<Value = Vec<(usize, usize, usize)>> {
    prop::collection::btree_set((0..NODES, 0..2usize, 0..NODES).prop_filter("no self-link", |(s, _, t)| s != t), 0..24)
        .prop_map(|edges| edges.into_iter().collect())
}

/// The graph a rank over these kinds and direction should build, from the edge list rather than the store: node `n` is
/// `Task-n`, and its edges are the list's, out, in, or both, each kind in turn
fn by_hand(edges: &[(usize, usize, usize)], kinds: &[usize], direction: &str) -> Projection {
    let mut graph = Projection::new();
    for n in 0..NODES {
        let mut targets = Vec::new();
        for &k in kinds {
            let mut out: Vec<u32> =
                edges.iter().filter(|&&(s, kk, _)| s == n && kk == k).map(|&(_, _, t)| t as u32).collect();
            let mut into: Vec<u32> =
                edges.iter().filter(|&&(_, kk, t)| t == n && kk == k).map(|&(s, _, _)| s as u32).collect();
            out.sort_unstable();
            into.sort_unstable();
            match direction {
                "out" => targets.extend(out),
                "in" => targets.extend(into),
                _ => targets.extend(out.into_iter().chain(into)),
            }
        }
        graph.node(targets);
    }
    graph
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn the_engine_builds_the_graph_the_edges_describe(
        edges in graph(), kind in 0..3usize, direction in 0..3usize, seed in 0..NODES,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let names: Vec<String> = (0..NODES).map(name).collect();
        tasks(&db, &names.iter().map(String::as_str).collect::<Vec<_>>());
        let ops = edges
            .iter()
            .map(|&(s, k, t)| json!({ "op": "link", "source": name(s), "kind": KINDS[k], "target": name(t) }))
            .collect();
        write(&db, ops).unwrap();
        let kinds: Vec<usize> = if kind == 2 { vec![0, 1] } else { vec![kind] };
        let kind_names: Vec<&str> = kinds.iter().map(|&k| KINDS[k]).collect();
        let direction = ["out", "in", "both"][direction];

        let expected = algorithms::rank(&by_hand(&edges, &kinds, direction), &[seed as u32], 0.85, 100, 1e-6).unwrap();
        let hits = ranked(&db, json!({ "kinds": kind_names, "direction": direction, "seeds": [name(seed)] }));
        for hit in hits["hits"].as_array().unwrap() {
            let n: usize = hit["id"].as_str().unwrap().trim_start_matches("Task-").parse().unwrap();
            prop_assert_eq!(hit["score"].as_f64().unwrap(), expected.scores[n], "{}", hit);
        }

        let mut groups = algorithms::weak_components(&by_hand(&edges, &kinds, "out"));
        let answer = query(&db, json!({ "start": { "from": "types", "types": ["Task"] },
            "terminal": { "answer": "weakComponents", "kinds": kind_names } }));
        let named: Vec<Vec<String>> = groups.drain(..).map(|g| g.into_iter().map(|n| name(n as usize)).collect()).collect();
        prop_assert_eq!(&answer["weakComponents"], &json!(named));

        let communities = algorithms::communities(&by_hand(&edges, &kinds, "out"), 1.0, 0, 100);
        let answer = query(&db, json!({ "start": { "from": "types", "types": ["Task"] },
            "terminal": { "answer": "communities", "kinds": kind_names } }));
        let named: Vec<Vec<String>> = communities.into_iter().map(|g| g.into_iter().map(|n| name(n as usize)).collect()).collect();
        prop_assert_eq!(&answer["communities"], &json!(named));
    }
}
