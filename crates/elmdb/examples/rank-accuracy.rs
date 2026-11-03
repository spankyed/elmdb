//! How close a rank index's top is to exact PageRank at a tolerance, and what it costs:
//! `rank-accuracy GRAPH DOCUMENTS TOLERANCE [SEED] [--accuracy]`, GRAPH `hubs`, `shuffled` (its hubs anywhere among the
//! documents, and SEED choosing them and the changes) or `spread`, built with `--release`. A history of changes is made,
//! each drained and timed, from an ordinary document and from the highest-scoring one, which `--accuracy` leaves out with
//! the reads and the churn; then the top ten, hundred and thousand are compared with `rank` run far past the tolerance,
//! and with the top a change before, as a watch sees it.
#[path = "../tests/support/mod.rs"]
#[allow(dead_code)]
mod support;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use elmdb::Database;
use elmdb::engine::batch::Batch;
use elmdb::engine::plan::Reads;
use serde_json::{Value as Json, json};

const SIZES: [usize; 3] = [10, 100, 1000];

fn read(db: &Database, plan: Json) -> Json {
    let reads: Reads = serde_json::from_value(json!({ "reads": [{ "read": "query", "plan": plan }] })).unwrap();
    let bytes = db.read(&reads).unwrap();
    serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap()[0].clone()
}

/// Ids and scores from a `hits` answer, in its order
fn hits(answer: &Json) -> Vec<(String, f64)> {
    let hits = answer["hits"].as_array().unwrap();
    hits.iter().map(|h| (h["id"].as_str().unwrap().to_owned(), h["score"].as_f64().unwrap())).collect()
}

fn top(db: &Database, n: usize) -> Vec<(String, f64)> {
    hits(&read(
        db,
        json!({ "start": { "from": "types", "types": ["Doc"] },
            "steps": [{ "step": "ranked", "index": "docRank" }, { "step": "limit", "n": n }],
            "terminal": { "answer": "hits" } }),
    ))
}

/// `rank` over the whole set, far past any tolerance here: the truth, highest first
fn exact(db: &Database) -> Vec<(String, f64)> {
    hits(&read(
        db,
        json!({ "start": { "from": "types", "types": ["Doc"] },
            "steps": [{ "step": "rank", "kinds": ["refs"], "iterations": 100000, "tolerance": 1e-13 }],
            "terminal": { "answer": "hits" } }),
    ))
}

fn change(op: &str, source: usize, target: usize) -> Batch {
    Batch {
        ops: vec![
            serde_json::from_value(json!({ "op": op, "source": format!("Doc-{source:08}"), "kind": "refs",
                "target": format!("Doc-{target:08}") }))
            .unwrap(),
        ],
        now: Some(1.0),
    }
}

/// A fixed sequence of numbers below `below`
fn sequence(seed: u64, below: usize) -> impl FnMut() -> usize {
    let mut state = seed;
    move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) % below as u64) as usize
    }
}

/// Median, 90th percentile and largest, in milliseconds
fn spread_of(mut ms: Vec<f64>) -> String {
    ms.sort_by(f64::total_cmp);
    let at = |q: f64| ms[((ms.len() - 1) as f64 * q) as usize];
    format!("{:.1}/{:.1}/{:.1}ms", at(0.5), at(0.9), at(1.0))
}

fn main() {
    let mut args = std::env::args().skip(1);
    let graph = args.next().expect("a graph: hubs or spread");
    let rows: usize = args.next().expect("a document count").parse().unwrap();
    let tolerance: f64 = args.next().expect("a tolerance").parse().unwrap();
    let rest: Vec<String> = args.collect();
    let accuracy_only = rest.iter().any(|a| a == "--accuracy");
    let seed: u64 = rest.iter().find(|a| !a.starts_with("--")).map_or(0, |s| s.parse().expect("a seed"));
    let f = match graph.as_str() {
        "hubs" => support::Fixture::ranked_hub_graph_at(rows, 5, tolerance),
        "shuffled" => support::Fixture::ranked_shuffled_hub_graph_at(rows, 5, tolerance, seed),
        "spread" => support::Fixture::ranked_graph_at(rows, 5, tolerance),
        other => panic!("no graph {other}"),
    };
    let db = &f.db;
    let timed = |batch: &[Batch]| {
        let started = Instant::now();
        for b in batch {
            db.write(b, false).unwrap();
        }
        db.wait_for_index_builds().unwrap();
        started.elapsed().as_secs_f64() * 1e3
    };
    let truth = exact(db);
    let hub: usize = truth[0].0["Doc-".len()..].parse().unwrap();
    let share = truth[0].1 / truth.iter().map(|t| t.1).sum::<f64>();
    let mut far = sequence(1 + 2 * seed, rows);
    let mut ordinary = sequence(2 + 2 * seed, rows / 2);
    println!("{graph} n={rows} tol={tolerance:e} seed={seed} top_share={share:.4}");

    if !accuracy_only {
        // An ordinary document, in the later half, linking one anywhere and unlinking it
        let mut typical = Vec::new();
        for _ in 0..20 {
            let (source, target) = (rows / 2 + ordinary(), far());
            if source == target {
                continue;
            }
            typical.push(timed(&[change("link", source, target)]));
            typical.push(timed(&[change("unlink", source, target)]));
        }
        // The highest-scoring document
        let mut from_hub = Vec::new();
        for _ in 0..10 {
            let target = far();
            if target == hub {
                continue;
            }
            from_hub.push(timed(&[change("link", hub, target)]));
            from_hub.push(timed(&[change("unlink", hub, target)]));
        }
        // Ten ordinary changes sharing a drain
        let mut burst = Vec::new();
        for _ in 0..10 {
            let pairs: Vec<(usize, usize)> = (0..10).map(|_| (rows / 2 + ordinary(), far())).collect();
            let batch = |op: &str| -> Vec<Batch> {
                pairs.iter().filter(|(s, t)| s != t).map(|&(s, t)| change(op, s, t)).collect()
            };
            burst.push(timed(&batch("link")));
            burst.push(timed(&batch("unlink")));
        }
        println!(
            "{graph} n={rows} tol={tolerance:e} top_share={share:.4} ordinary={} hub={} burst={}",
            spread_of(typical),
            spread_of(from_hub),
            spread_of(burst)
        );

        // Reads, with no drain owed
        for (answer, n) in SIZES.iter().flat_map(|&n| [("hits", n), ("ids", n)]) {
            let reads: Reads = serde_json::from_value(json!({ "reads": [{ "read": "query", "plan": {
            "start": { "from": "types", "types": ["Doc"] },
            "steps": [{ "step": "ranked", "index": "docRank" }, { "step": "limit", "n": n }],
            "terminal": { "answer": answer } } }] }))
            .unwrap();
            let started = Instant::now();
            let runs = 1000;
            for _ in 0..runs {
                std::hint::black_box(db.read(&reads).unwrap());
            }
            println!("  read {answer}{n} {:.1}us", started.elapsed().as_secs_f64() * 1e6 / runs as f64);
        }
    }

    // The top after a history of ordinary changes left linked, each drained: it holds what the last left within budget
    for _ in 0..30 {
        let (source, target) = (rows / 2 + ordinary(), far());
        if source != target {
            db.write(&change("link", source, target), false).unwrap();
            db.wait_for_index_builds().unwrap();
        }
    }
    let truth = exact(db);
    let at: HashMap<&str, f64> = truth.iter().map(|(id, s)| (id.as_str(), *s)).collect();
    let bound = read(
        db,
        json!({ "start": { "from": "types", "types": ["Doc"] },
            "steps": [{ "step": "ranked", "index": "docRank" }, { "step": "limit", "n": 1 }],
            "terminal": { "answer": "hits" } }),
    )["rank"]["error"]
        .as_f64()
        .unwrap();
    println!("  bound reported {bound:e}");
    for n in SIZES {
        let held = top(db, n);
        let true_top: HashSet<&str> = truth[..n].iter().map(|t| t.0.as_str()).collect();
        let overlap = held.iter().filter(|h| true_top.contains(h.0.as_str())).count();
        let same_place = held.iter().zip(&truth).filter(|(h, t)| h.0 == t.0).count();
        // What the answer's nodes truly score against what the true top does: near ties cost this nothing
        let kept: f64 = held.iter().map(|h| at[h.0.as_str()]).sum();
        let best: f64 = truth[..n].iter().map(|t| t.1).sum();
        let worst_error = held.iter().map(|h| (h.1 - at[h.0.as_str()]).abs() / at[h.0.as_str()]).fold(0.0, f64::max);
        println!(
            "  top{n}: in_true_top={overlap}/{n} same_place={same_place}/{n} score_lost={:.2e} worst_score_error={worst_error:.2e}",
            1.0 - kept / best
        );
    }

    if accuracy_only {
        std::hint::black_box(&f.directory);
        return;
    }
    // What a watch over the top sees: how much of it moves with each ordinary change
    for n in [10, 100] {
        let (mut left, mut moved, mut changes) = (0usize, 0usize, 0usize);
        let mut before = top(db, n);
        for _ in 0..20 {
            let (source, target) = (rows / 2 + ordinary(), far());
            if source == target {
                continue;
            }
            db.write(&change("link", source, target), false).unwrap();
            db.wait_for_index_builds().unwrap();
            let now = top(db, n);
            let was: HashSet<&str> = before.iter().map(|h| h.0.as_str()).collect();
            left += now.iter().filter(|h| !was.contains(h.0.as_str())).count();
            moved += now.iter().zip(&before).filter(|(a, b)| a.0 != b.0).count();
            changes += 1;
            before = now;
        }
        println!(
            "  churn top{n}: entered_per_change={:.2} moved_place_per_change={:.2}",
            left as f64 / changes as f64,
            moved as f64 / changes as f64
        );
    }
    std::hint::black_box(&f.directory);
}
