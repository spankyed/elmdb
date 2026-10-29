//! A relation hop's walk: `reverse` straight after a hop from one entity over one kind reads its edges from the last,
//! so a `limit` after it reads only what it keeps, and answers what reversing the whole walk answers: the far ends in
//! place order backward, each once at its first edge's place, a trashed one left out. A tracked read of it is written
//! into by an edge from its last to where it stopped, and by none before.

use serde_json::{Value as Json, json};

use super::{batch, open, write};
use crate::Database;
use crate::engine::plan::{ReadOp, Reads};

const TARGETS: usize = 400;

/// Task-0 linked by `refs` and by `tags` (unique) to Task-1 to Task-400 in order, with Note-1 to Note-40 among them;
/// Task-7 linked by `refs` a second time, last; Task-13 and Task-200 trashed; Task-401 to Task-420 linking to Task-0
fn fixture() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let mut ops = vec![json!({ "op": "create", "type": "Task", "id": "Task-0" })];
    for i in 1..=TARGETS {
        ops.push(json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") }));
        if i % 10 == 0 {
            ops.push(json!({ "op": "create", "type": "Note", "id": format!("Note-{}", i / 10) }));
        }
    }
    for i in 401..=420 {
        ops.push(json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") }));
    }
    write(&db, ops).unwrap();
    let mut links = Vec::new();
    for i in 1..=TARGETS {
        for kind in ["refs", "tags"] {
            links.push(json!({ "op": "link", "source": "Task-0", "kind": kind, "target": format!("Task-{i}") }));
            if i % 10 == 0 {
                links.push(
                    json!({ "op": "link", "source": "Task-0", "kind": kind, "target": format!("Note-{}", i / 10) }),
                );
            }
        }
    }
    links.push(
        json!({ "op": "link", "source": "Task-0", "kind": "refs", "target": "Task-7", "info": { "again": true } }),
    );
    for i in 401..=420 {
        links.push(json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": "Task-0" }));
    }
    links.push(json!({ "op": "trash", "id": "Task-13" }));
    links.push(json!({ "op": "trash", "id": "Task-200" }));
    write(&db, links).unwrap();
    (dir, db)
}

fn read(db: &Database, op: Json, track: Option<u32>) -> Json {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track };
    let bytes = db.read(&reads).unwrap();
    let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    answers[0].take()
}

fn plan(ids: &[&str], steps: Json) -> Json {
    json!({ "start": { "from": "ids", "ids": ids }, "steps": steps, "terminal": { "answer": "ids" } })
}

fn ids(db: &Database, from: &[&str], steps: Json) -> Vec<Json> {
    read(db, json!({ "read": "query", "plan": plan(from, steps) }), None)["ids"].as_array().unwrap().clone()
}

/// What each stage kept, in a read of the plan asked to explain itself
fn kept(db: &Database, from: &[&str], steps: Json) -> Vec<u64> {
    let explained = read(db, json!({ "read": "query", "explain": true, "plan": plan(from, steps) }), None);
    explained["stages"].as_array().unwrap().iter().map(|s| s["kept"].as_u64().unwrap()).collect()
}

#[test]
fn a_reverse_after_a_hop_answers_the_walk_reversed_and_a_limit_after_it_reads_only_what_it_keeps() {
    let (_dir, db) = fixture();
    for (step, kind) in [("out", "refs"), ("out", "tags"), ("in", "refs")] {
        for types in [None, Some(json!(["Task"]))] {
            let mut hop = json!({ "step": step, "kinds": [kind] });
            if let Some(types) = &types {
                hop["types"] = types.clone();
            }
            let mut walked = ids(&db, &["Task-0"], json!([hop]));
            assert!(walked.len() >= 20, "{step} {kind} {types:?}: the case walks something");
            walked.reverse();
            assert_eq!(ids(&db, &["Task-0"], json!([hop, { "step": "reverse" }])), walked, "{step} {kind} {types:?}");
            for n in [0, 1, 5, walked.len(), walked.len() + 3] {
                let page = ids(&db, &["Task-0"], json!([hop, { "step": "reverse" }, { "step": "limit", "n": n }]));
                assert_eq!(page[..], walked[..n.min(walked.len())], "{step} {kind} {types:?}, limit {n}");
            }
            let stages = kept(&db, &["Task-0"], json!([hop, { "step": "reverse" }, { "step": "limit", "n": 5 }]));
            assert_eq!(
                stages[1],
                5,
                "{step} {kind} {types:?}: the hop yields what the limit keeps, not its {}",
                walked.len()
            );
        }
    }
}

#[test]
fn an_end_linked_twice_is_where_its_first_edge_is_and_a_trashed_one_is_left_out() {
    let (_dir, db) = fixture();
    let hop = json!({ "step": "out", "kinds": ["refs"], "types": ["Task"] });
    let last = ids(&db, &["Task-0"], json!([hop, { "step": "reverse" }, { "step": "limit", "n": 3 }]));
    assert_eq!(last, [json!("Task-400"), json!("Task-399"), json!("Task-398")], "Task-7's second edge is the last");
    let all = ids(&db, &["Task-0"], json!([hop, { "step": "reverse" }]));
    assert_eq!(all.iter().filter(|id| *id == "Task-7").count(), 1);
    assert_eq!(all[all.len() - 7], "Task-7", "at its first edge's place, after Task-8 and before Task-6");
    assert!(!all.contains(&json!("Task-13")) && !all.contains(&json!("Task-200")));
}

#[test]
fn a_reverse_after_a_hop_from_several_entities_or_kinds_reverses_the_whole_walk() {
    let (_dir, db) = fixture();
    for (from, kinds) in [(vec!["Task-0", "Task-401"], json!(["refs"])), (vec!["Task-0"], json!(["refs", "tags"]))] {
        let hop = json!({ "step": "out", "kinds": kinds });
        let mut walked = ids(&db, &from, json!([hop]));
        walked.reverse();
        let page = ids(&db, &from, json!([hop, { "step": "reverse" }, { "step": "limit", "n": 4 }]));
        assert_eq!(page[..], walked[..4], "{from:?} {kinds}");
    }
}

#[test]
fn a_tracked_reverse_after_a_hop_is_written_into_as_far_back_as_it_read_and_no_further() {
    let (_dir, db) = fixture();
    let last = json!({ "read": "query", "plan": plan(&["Task-0"],
        json!([{ "step": "out", "kinds": ["tags"] }, { "step": "reverse" }, { "step": "limit", "n": 3 }])) });
    // `tags` is unique, so the walk reads no edge's ends but through its edges
    let mut token = 0;
    // A fresh read of the last three for each write, so each asks of what that read reached
    let mut touches = |op: Json| {
        token += 1;
        read(&db, last.clone(), Some(token));
        let touched = db.write(&batch(vec![op]), false).unwrap().1.touched;
        db.untrack(token);
        !touched.is_empty()
    };
    let unlink = |target: &str| json!({ "op": "unlink", "source": "Task-0", "kind": "tags", "target": target });
    assert!(
        touches(json!({ "op": "link", "source": "Task-0", "kind": "tags", "target": "Task-401" })),
        "a new last edge"
    );
    assert!(touches(unlink("Task-401")), "the last edge, the first it kept");
    assert!(touches(unlink("Task-399")), "the edge it stopped at, the last of Note-40, Task-400 and Task-399 it kept");
    assert!(!touches(unlink("Task-2")), "an edge before where it stopped is no write into it");
}

#[test]
fn a_reverse_after_a_hop_in_a_unit_reads_the_unit_s_own_edges() {
    let (_dir, db) = fixture();
    let mut unit = db.begin().unwrap();
    unit.write(&batch(vec![json!({ "op": "link", "source": "Task-0", "kind": "refs", "target": "Task-401" })]))
        .unwrap();
    let op = json!({ "read": "query", "plan": plan(&["Task-0"],
        json!([{ "step": "out", "kinds": ["refs"] }, { "step": "reverse" }, { "step": "limit", "n": 2 }])) });
    let bytes = unit.read(&Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None }).unwrap();
    let answer = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    assert_eq!(answer[0]["ids"], json!(["Task-401", "Note-40"]));
}
