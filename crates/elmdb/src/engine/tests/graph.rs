//! Graph walks (`docs/architecture/engine.md`, Reads): a depth bound on `descendants` and `ancestors`, `both`, `pathTo`
//! and the `degrees` answer, case by case and against a plain breadth-first reference over random graphs.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use proptest::prelude::*;
use serde_json::{Value as Json, json};

use super::{batch, open, write};
use crate::Database;
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

/// The ids a plan from these ids answers, under a scope
fn ids_in(db: &Database, scope: &str, from: &[&str], steps: Json) -> Json {
    let plan = json!({ "scope": scope, "start": { "from": "ids", "ids": from }, "steps": steps, "terminal": { "answer": "ids" } });
    query(db, plan)["ids"].clone()
}

fn ids(db: &Database, from: &[&str], steps: Json) -> Json {
    ids_in(db, "live", from, steps)
}

fn degrees(db: &Database, from: &[&str], terminal: Json) -> Json {
    query(db, json!({ "start": { "from": "ids", "ids": from }, "terminal": terminal }))["degrees"].clone()
}

fn tasks(db: &Database, names: &[&str]) {
    write(db, names.iter().map(|n| json!({ "op": "create", "type": "Task", "id": n })).collect()).unwrap();
}

fn link(db: &Database, edges: &[(&str, &str, &str)]) {
    let ops = edges.iter().map(|(s, k, t)| json!({ "op": "link", "source": s, "kind": k, "target": t })).collect();
    write(db, ops).unwrap();
}

#[test]
fn descendants_and_ancestors_stop_at_their_depth() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b", "Task-c", "Task-d"]);
    link(
        &db,
        &[("Task-a", "parent_of", "Task-b"), ("Task-b", "parent_of", "Task-c"), ("Task-a", "parent_of", "Task-d")],
    );
    let down =
        |depth: Json| ids(&db, &["Task-a"], json!([{ "step": "descendants", "kinds": ["parent_of"], "depth": depth }]));
    assert_eq!(down(json!(0)), json!([]), "no edge walked");
    assert_eq!(down(json!(1)), json!(["Task-b", "Task-d"]));
    assert_eq!(down(json!(2)), json!(["Task-b", "Task-d", "Task-c"]));
    assert_eq!(down(Json::Null), json!(["Task-b", "Task-d", "Task-c"]), "no depth is any depth");
    let up =
        |depth: u32| ids(&db, &["Task-c"], json!([{ "step": "ancestors", "kinds": ["parent_of"], "depth": depth }]));
    assert_eq!(up(1), json!(["Task-b"]));
    assert_eq!(up(2), json!(["Task-b", "Task-a"]));
}

#[test]
fn both_follows_edges_out_and_in_each_far_end_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b", "Task-c", "Task-d", "Task-e"]);
    write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-n" })]).unwrap();
    link(
        &db,
        &[
            ("Task-a", "refs", "Task-b"),
            ("Task-c", "refs", "Task-a"),
            ("Task-b", "refs", "Task-a"),
            ("Task-a", "refs", "Note-n"),
            ("Task-a", "blocks", "Task-d"),
            ("Task-e", "refs", "Task-a"),
        ],
    );
    write(&db, vec![json!({ "op": "trash", "id": "Task-e" })]).unwrap();
    let both = |types: Json| json!([{ "step": "both", "kinds": ["refs"], "types": types }]);
    assert_eq!(ids(&db, &["Task-a"], both(Json::Null)), json!(["Task-b", "Note-n", "Task-c"]), "out first, then in");
    assert_eq!(ids(&db, &["Task-a"], both(json!(["Task"]))), json!(["Task-b", "Task-c"]));
    assert_eq!(
        ids_in(&db, "withTrashed", &["Task-a"], both(json!(["Task"]))),
        json!(["Task-b", "Task-c", "Task-e"]),
        "a trashed end is reached only with the trash"
    );
    assert_eq!(
        ids(&db, &["Task-b", "Task-c"], json!([{ "step": "both", "kinds": ["refs"] }])),
        json!(["Task-a"]),
        "an end two of the set reach is visited once"
    );
    assert_eq!(
        ids(
            &db,
            &["Task-a"],
            json!([{ "step": "both", "kinds": ["refs"] }, { "step": "reverse" }, { "step": "limit", "n": 2 }])
        ),
        json!(["Task-c", "Note-n"]),
        "a reverse after it reverses the whole walk"
    );
    let plan = |step: Json| json!({ "start": { "from": "ids", "ids": ["Task-a"] }, "steps": [step], "terminal": { "answer": "ids" } });
    let refused = try_query(&db, plan(json!({ "step": "both", "kinds": ["follows"] }))).unwrap_err();
    assert_eq!(refused.code, ErrorCode::UnknownRelationKind);
    let refused = try_query(&db, plan(json!({ "step": "descendants", "kinds": ["follows"], "depth": 1 }))).unwrap_err();
    assert_eq!(refused.code, ErrorCode::UnknownRelationKind);
}

#[test]
fn path_to_answers_a_shortest_path_from_the_set_or_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b", "Task-c", "Task-d", "Task-e", "Task-f", "Task-x"]);
    // a → b → c → d, a shortcut a → e → d, and f linking into a
    link(
        &db,
        &[
            ("Task-a", "refs", "Task-b"),
            ("Task-b", "refs", "Task-c"),
            ("Task-c", "refs", "Task-d"),
            ("Task-a", "refs", "Task-e"),
            ("Task-e", "refs", "Task-d"),
            ("Task-f", "refs", "Task-a"),
        ],
    );
    let path = |from: &[&str], to: &str, extra: Json| {
        let mut step = json!({ "step": "pathTo", "to": to, "kinds": ["refs"] });
        step.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        ids(&db, from, json!([step]))
    };
    assert_eq!(path(&["Task-a"], "Task-d", json!({})), json!(["Task-a", "Task-e", "Task-d"]), "the shortcut");
    assert_eq!(path(&["Task-a"], "Task-c", json!({})), json!(["Task-a", "Task-b", "Task-c"]));
    assert_eq!(path(&["Task-a"], "Task-a", json!({})), json!(["Task-a"]), "a start is its own path");
    assert_eq!(path(&["Task-a"], "Task-x", json!({})), json!([]), "nothing links to it");
    assert_eq!(path(&["Task-a"], "Task-gone", json!({})), json!([]), "no such entity");
    assert_eq!(path(&["Task-a"], "Task-f", json!({})), json!([]), "edges in aren't followed out");
    assert_eq!(path(&["Task-a"], "Task-f", json!({ "direction": "in" })), json!(["Task-a", "Task-f"]));
    assert_eq!(
        path(&["Task-d"], "Task-f", json!({ "direction": "both" })),
        json!(["Task-d", "Task-e", "Task-a", "Task-f"]),
        "an edge's way doesn't matter"
    );
    assert_eq!(path(&["Task-a"], "Task-d", json!({ "depth": 1 })), json!([]), "two edges away");
    assert_eq!(path(&["Task-a"], "Task-d", json!({ "depth": 2 })), json!(["Task-a", "Task-e", "Task-d"]));
    assert_eq!(
        path(&["Task-c", "Task-a"], "Task-d", json!({})),
        json!(["Task-c", "Task-d"]),
        "from whichever of the set is nearest"
    );
    // Two paths as short: the one through the set's earlier entity, then through the earlier edge
    assert_eq!(path(&["Task-b", "Task-e"], "Task-d", json!({})), json!(["Task-e", "Task-d"]));
    link(&db, &[("Task-b", "refs", "Task-d")]);
    assert_eq!(path(&["Task-b", "Task-e"], "Task-d", json!({})), json!(["Task-b", "Task-d"]));
    assert_eq!(
        path(&["Task-a"], "Task-d", json!({})),
        json!(["Task-a", "Task-b", "Task-d"]),
        "b's edge was placed first"
    );

    write(&db, vec![json!({ "op": "trash", "id": "Task-b" })]).unwrap();
    assert_eq!(
        path(&["Task-a"], "Task-d", json!({})),
        json!(["Task-a", "Task-e", "Task-d"]),
        "around a trashed entity"
    );
    assert_eq!(path(&["Task-a"], "Task-c", json!({})), json!([]), "only through it");
    assert_eq!(path(&["Task-a"], "Task-b", json!({})), json!([]), "to it");
    assert_eq!(
        ids_in(&db, "withTrashed", &["Task-a"], json!([{ "step": "pathTo", "to": "Task-b", "kinds": ["refs"] }])),
        json!(["Task-a", "Task-b"]),
        "with the trash"
    );

    let rows = query(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Task-a"] },
            "steps": [{ "step": "pathTo", "to": "Task-d", "kinds": ["refs"] }],
            "terminal": { "answer": "rows", "fields": ["id"] }
        }),
    );
    let order: Vec<&Json> = rows["rows"].as_array().unwrap().iter().map(|r| &r["id"]).collect();
    assert_eq!(order, [&json!("Task-a"), &json!("Task-e"), &json!("Task-d")], "rows come in path order");

    let refused = try_query(
        &db,
        json!({
            "start": { "from": "ids", "ids": ["Task-a"] },
            "steps": [{ "step": "pathTo", "to": "Task-d", "kinds": ["follows"] }],
            "terminal": { "answer": "ids" }
        }),
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::UnknownRelationKind);
}

#[test]
fn degrees_count_each_entity_s_edges_as_edges_would_answer_them() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b", "Task-c", "Task-d"]);
    write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-n" })]).unwrap();
    link(
        &db,
        &[
            ("Task-a", "refs", "Task-b"),
            ("Task-a", "refs", "Task-c"),
            ("Task-a", "refs", "Note-n"),
            ("Task-a", "blocks", "Task-b"),
            ("Task-b", "refs", "Task-a"),
        ],
    );
    let of = |direction: &str, kinds: Json, types: Json| {
        degrees(
            &db,
            &["Task-a", "Task-b", "Task-d"],
            json!({ "answer": "degrees", "kinds": kinds, "direction": direction, "types": types }),
        )
    };
    assert_eq!(of("out", json!(["refs"]), Json::Null), json!([["Task-a", 3], ["Task-b", 1], ["Task-d", 0]]));
    assert_eq!(of("in", json!(["refs"]), Json::Null), json!([["Task-a", 1], ["Task-b", 1], ["Task-d", 0]]));
    assert_eq!(of("both", json!(["refs"]), Json::Null), json!([["Task-a", 4], ["Task-b", 2], ["Task-d", 0]]));
    assert_eq!(
        of("out", json!(["refs", "blocks"]), json!(["Task"])),
        json!([["Task-a", 3], ["Task-b", 1], ["Task-d", 0]]),
        "every kind named, a Note's edge left out"
    );
    assert_eq!(
        degrees(&db, &["Task-a"], json!({ "answer": "degrees", "kinds": ["refs"] })),
        json!([["Task-a", 3]]),
        "out unless told otherwise"
    );
    write(&db, vec![json!({ "op": "trash", "id": "Task-c" })]).unwrap();
    assert_eq!(of("out", json!(["refs"]), json!(["Task"])), json!([["Task-a", 1], ["Task-b", 1], ["Task-d", 0]]));
    let refused = try_query(
        &db,
        json!({ "start": { "from": "ids", "ids": ["Task-a"] }, "terminal": { "answer": "degrees", "kinds": ["follows"] } }),
    )
    .unwrap_err();
    assert_eq!(refused.code, ErrorCode::UnknownRelationKind);
}

#[test]
fn explain_names_the_new_stages() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b"]);
    link(&db, &[("Task-a", "refs", "Task-b")]);
    let explained = decode(
        &db.read(&reads(
            vec![json!({ "read": "query", "explain": true, "plan": {
                "start": { "from": "ids", "ids": ["Task-a"] },
                "steps": [
                    { "step": "both", "kinds": ["refs"] },
                    { "step": "pathTo", "to": "Task-a", "kinds": ["refs"], "direction": "both" }
                ],
                "terminal": { "answer": "degrees", "kinds": ["refs"] }
            } })],
            None,
        ))
        .unwrap(),
    );
    let stages: Vec<&Json> = explained[0]["stages"].as_array().unwrap().iter().map(|s| &s["stage"]).collect();
    assert_eq!(stages, [&json!("ids"), &json!("both"), &json!("pathTo"), &json!("degrees")]);
    assert_eq!(explained[0]["answer"]["degrees"], json!([["Task-b", 0], ["Task-a", 1]]));
}

#[test]
fn a_tracked_path_is_written_into_by_an_edge_on_what_it_walked() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    tasks(&db, &["Task-a", "Task-b", "Task-c", "Task-d", "Task-y", "Task-z"]);
    link(&db, &[("Task-a", "refs", "Task-b"), ("Task-b", "refs", "Task-c"), ("Task-c", "refs", "Task-d")]);
    let path = json!({ "read": "query", "plan": {
        "start": { "from": "ids", "ids": ["Task-a"] },
        "steps": [{ "step": "pathTo", "to": "Task-d", "kinds": ["refs"] }],
        "terminal": { "answer": "ids" }
    } });
    db.read(&reads(vec![path], Some(1))).unwrap();
    let touched = |edge: (&str, &str)| {
        db.write(&batch(vec![json!({ "op": "link", "source": edge.0, "kind": "refs", "target": edge.1 })]), false)
            .unwrap()
            .1
            .touched
    };
    assert!(touched(("Task-y", "Task-z")).is_empty(), "entities it never walked");
    assert_eq!(touched(("Task-a", "Task-d")), [1], "a shorter path from its start");
}

/// A random graph over a few tasks and two kinds, each edge once; the engine refuses a self-link
fn graph() -> impl Strategy<Value = Vec<(usize, usize, usize)>> {
    prop::collection::btree_set((0..NODES, 0..2usize, 0..NODES).prop_filter("no self-link", |(s, _, t)| s != t), 0..24)
        .prop_map(|edges| edges.into_iter().collect())
}

const NODES: usize = 7;
const KINDS: [&str; 2] = ["refs", "blocks"];

fn name(n: usize) -> String {
    format!("Task-{n}")
}

/// Each task's distance from `from` over the kinds, out or both ways, as plainly as a breadth-first walk is written
fn distances(edges: &[(usize, usize, usize)], from: usize, kinds: &[usize], both: bool) -> BTreeMap<usize, u32> {
    let mut next: HashMap<usize, BTreeSet<usize>> = HashMap::new();
    for &(s, k, t) in edges {
        if kinds.contains(&k) {
            next.entry(s).or_default().insert(t);
            if both {
                next.entry(t).or_default().insert(s);
            }
        }
    }
    let mut seen = BTreeMap::from([(from, 0)]);
    let mut queue = VecDeque::from([from]);
    while let Some(at) = queue.pop_front() {
        for &far in next.get(&at).into_iter().flatten() {
            if !seen.contains_key(&far) {
                seen.insert(far, seen[&at] + 1);
                queue.push_back(far);
            }
        }
    }
    seen
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, failure_persistence: None, ..ProptestConfig::default() })]

    #[test]
    fn walks_agree_with_a_breadth_first_reference(edges in graph(), from in 0..NODES, kind in 0..3usize) {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        let names: Vec<String> = (0..NODES).map(name).collect();
        tasks(&db, &names.iter().map(String::as_str).collect::<Vec<_>>());
        let ops = edges
            .iter()
            .map(|&(s, k, t)| json!({ "op": "link", "source": name(s), "kind": KINDS[k], "target": name(t) }))
            .collect();
        write(&db, ops).unwrap();
        // One kind, or both of them
        let kinds: Vec<usize> = if kind == 2 { vec![0, 1] } else { vec![kind] };
        let kind_names: Vec<&str> = kinds.iter().map(|&k| KINDS[k]).collect();

        for both in [false, true] {
            let reference = distances(&edges, from, &kinds, both);
            let direction = if both { "both" } else { "out" };
            for to in 0..NODES {
                let path = ids(&db, &[&name(from)], json!([{ "step": "pathTo", "to": name(to), "kinds": kind_names, "direction": direction }]));
                let path: Vec<String> = serde_json::from_value(path).unwrap();
                match reference.get(&to) {
                    None => prop_assert!(path.is_empty(), "no path to {to}, answered {path:?}"),
                    Some(&distance) => {
                        prop_assert_eq!(path.len() as u32, distance + 1, "to {}: {:?}", to, path);
                        prop_assert_eq!(&path[0], &name(from));
                        prop_assert_eq!(path.last().unwrap(), &name(to));
                        for pair in path.windows(2) {
                            let linked = edges.iter().any(|&(s, k, t)| {
                                kinds.contains(&k)
                                    && ((name(s) == pair[0] && name(t) == pair[1])
                                        || (both && name(t) == pair[0] && name(s) == pair[1]))
                            });
                            prop_assert!(linked, "{} to {} isn't an edge", pair[0], pair[1]);
                        }
                    }
                }
            }
        }

        let reference = distances(&edges, from, &kinds, false);
        for depth in 0..NODES as u32 {
            let reached = ids(&db, &[&name(from)], json!([{ "step": "descendants", "kinds": kind_names, "depth": depth }]));
            let reached: BTreeSet<String> = serde_json::from_value(reached).unwrap();
            let expected: BTreeSet<String> =
                reference.iter().filter(|&(&n, &d)| n != from && d <= depth).map(|(&n, _)| name(n)).collect();
            prop_assert_eq!(reached, expected, "depth {}", depth);
        }

        let all: Vec<String> = (0..NODES).map(name).collect();
        let answer = degrees(&db, &all.iter().map(String::as_str).collect::<Vec<_>>(), json!({ "answer": "degrees", "kinds": kind_names, "direction": "both" }));
        for (n, row) in answer.as_array().unwrap().iter().enumerate() {
            let expected = edges.iter().filter(|&&(s, k, t)| kinds.contains(&k) && (s == n || t == n)).count();
            prop_assert_eq!(&row[0], &json!(name(n)));
            prop_assert_eq!(row[1].as_u64().unwrap() as usize, expected, "{}'s degree", name(n));
        }
    }
}
