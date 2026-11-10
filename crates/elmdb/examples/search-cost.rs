//! What a search costs, by how many notes hold its word: `search-cost [NOTES [TIMES]]`, 200,000 unless told otherwise;
//! with TIMES, built with `--profile profiling`, it reads the first of these that many times and prints nothing. Each
//! note's title is in a text index: three words every note holds, two a twelfth hold, and its own number. Prints, for
//! a word every note holds and one a twelfth hold, the microseconds its best ten, its count and all of its ids take, a
//! median of seven, so two revisions can be compared in turn (`cargo xtask compare`).

use std::time::Instant;

use elmdb::engine::plan::{ReadOp, Reads};
use elmdb::{Database, OpenOptions};
use serde_json::{Value, json};

const WORDS: [&str; 12] =
    ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet", "kilo", "lima"];

fn read(db: &Database, word: &str, limit: Option<u64>, answer: &str) -> usize {
    let mut steps = vec![json!({ "step": "search", "query": word, "options": { "fuzzy": false } })];
    steps.extend(limit.map(|n| json!({ "step": "limit", "n": n })));
    let plan =
        json!({ "start": { "from": "types", "types": ["Note"] }, "steps": steps, "terminal": { "answer": answer } });
    let op: ReadOp = serde_json::from_value(json!({ "read": "query", "plan": plan })).unwrap();
    db.read(&Reads { reads: vec![op], track: None }).unwrap().len()
}

/// The median time of seven runs, in microseconds
fn micros(mut run: impl FnMut() -> usize) -> f64 {
    let mut samples: Vec<f64> = (0..7)
        .map(|_| {
            let started = Instant::now();
            std::hint::black_box(run());
            started.elapsed().as_secs_f64() * 1e6
        })
        .collect();
    samples.sort_by(f64::total_cmp);
    samples[3]
}

fn main() {
    let notes: usize = std::env::args().nth(1).map(|n| n.parse().expect("how many notes")).unwrap_or(200_000);
    let dir = tempfile::tempdir().unwrap();
    let schema = elmdb::schema::Schema::from_json(&json!({
        "$schema": elmdb::schema::SCHEMA_LANGUAGE,
        "types": { "Note": {
            "fields": { "title": { "type": "string" } },
            "indexes": { "text": { "kind": "text", "fields": { "title": {} } } }
        } }
    }))
    .unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap();
    for from in (0..notes).step_by(5000) {
        let ops: Vec<Value> = (from..notes.min(from + 5000))
            .map(|i| {
                let title = format!("the {} {} note number {i}", WORDS[i % 12], WORDS[(i * 7) % 12]);
                json!({ "op": "create", "type": "Note", "id": format!("Note-{i:07}"), "fields": { "title": title } })
            })
            .collect();
        db.write(&serde_json::from_value(json!({ "ops": ops, "now": 1.0 })).unwrap(), false).unwrap();
    }
    // For a native sampler: the best ten of the word every note holds, over and over
    if let Some(times) = std::env::args().nth(2).map(|n| n.parse::<usize>().expect("how many times")) {
        println!("PROFILE_READY pid={}", std::process::id());
        for _ in 0..times {
            std::hint::black_box(read(&db, "the", Some(10), "ids"));
        }
        return;
    }
    for (name, word) in [("every", "the"), ("twelfth", "alpha")] {
        println!("{name}BestTen={:.0}us", micros(|| read(&db, word, Some(10), "ids")));
        println!("{name}Count={:.0}us", micros(|| read(&db, word, None, "count")));
        println!("{name}Ids={:.0}us", micros(|| read(&db, word, None, "ids")));
    }
    db.close().unwrap();
}
