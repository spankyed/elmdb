//! Every entity of a type in a value index's order, repeated, for a native sampler: `profile-order FIELD [NOTES]
//! [ITERATIONS] [DIR]`, built with `--profile profiling`. FIELD is `name`, a string no other note holds, or `n`, a
//! number, as the binding's benchmark against SQLite has them (`crates/elmdb-node/bench/against-sqlite.mjs`). The
//! notes are loaded into DIR when it holds no database yet, so a second run reads the first's; the answer is checked
//! before PROFILE_READY, and only the reads follow, each one's milliseconds printed.
use std::io::Write;
use std::time::Instant;

use elmdb::{Database, OpenOptions};
use serde_json::{Value, json};

const WORDS: [&str; 12] =
    ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet", "kilo", "lima"];

fn id(i: usize) -> String {
    format!("Note-{i:07}")
}

/// The benchmark's name of a note, which no other holds
fn name_of(i: usize) -> String {
    format!("{} {} {i}", WORDS[i % 12], WORDS[(i * 7) % 12])
}

fn main() {
    let mut args = std::env::args().skip(1);
    let field = args.next().expect("usage: profile-order FIELD [NOTES] [ITERATIONS] [DIR]");
    let notes: usize = args.next().map_or(1_000_000, |n| n.parse().expect("a number of notes"));
    let iterations: usize = args.next().map_or(10, |n| n.parse().expect("a number of iterations"));
    let temporary = tempfile::tempdir().unwrap();
    let dir = args.next().map_or_else(|| temporary.path().to_owned(), std::path::PathBuf::from);
    let string = json!({ "type": "string" });
    let schema = elmdb::schema::Schema::from_json(&json!({
        "$schema": elmdb::schema::SCHEMA_LANGUAGE,
        "types": { "Note": {
            "fields": {
                "title": string, "name": string, "n": { "type": "number" },
                "status": { "type": "enum", "values": ["open", "done"] },
            },
            "indexes": {
                "status": { "kind": "value", "fields": ["status"] },
                "name": { "kind": "value", "fields": ["name"] },
                "n": { "kind": "value", "fields": ["n"] },
            },
        } },
        "relations": {}, "roles": {}
    }))
    .unwrap();
    let fresh = !dir.join("data.mdb").exists();
    std::fs::create_dir_all(&dir).unwrap();
    let db = Database::open(&dir, OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap();
    if fresh {
        let began = Instant::now();
        for from in (0..notes).step_by(10_000) {
            let ops: Vec<Value> = (from..(from + 10_000).min(notes))
                .map(|i| {
                    let fields = json!({
                        "title": format!("the {} {} note number {i}", WORDS[i % 12], WORDS[(i * 7) % 12]),
                        "name": name_of(i), "n": (i * 7919) % notes,
                        "status": if i % 2 == 1 { "open" } else { "done" },
                    });
                    json!({ "op": "create", "type": "Note", "id": id(i), "fields": fields })
                })
                .collect();
            db.write(&serde_json::from_value(json!({ "ops": ops, "now": 1.0 })).unwrap(), false).unwrap();
        }
        db.wait_for_index_builds().unwrap();
        println!("loaded={notes} in {:.1}s", began.elapsed().as_secs_f64());
    }
    let reads = serde_json::from_value(json!({ "reads": [{ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Note"] },
        "steps": [{ "step": "orderBy", "field": field }],
        "terminal": { "answer": "ids" },
    } }] }))
    .unwrap();
    // The answer, checked against the order a sort of the notes gives
    let answer: Vec<Value> = rmp_serde::from_slice(&db.read(&reads).unwrap()).unwrap();
    let ids: Vec<String> = serde_json::from_value(answer[0]["ids"].clone()).unwrap();
    let mut expected: Vec<usize> = (0..notes).collect();
    match field.as_str() {
        // The names are lowercase letters, digits and spaces, which the root collation orders as their bytes
        "name" => expected.sort_by_cached_key(|i| name_of(*i)),
        "n" => expected.sort_by_key(|i| ((i * 7919) % notes, *i)),
        _ => panic!("no field {field}: name or n"),
    }
    assert!(
        ids.iter().map(String::as_str).eq(expected
            .iter()
            .map(|i| id(*i))
            .collect::<Vec<_>>()
            .iter()
            .map(String::as_str))
    );
    println!("PROFILE_READY pid={} field={field} notes={notes} iterations={iterations}", std::process::id());
    std::io::stdout().flush().unwrap();
    let started = Instant::now();
    for _ in 0..iterations {
        let began = Instant::now();
        std::hint::black_box(db.read(std::hint::black_box(&reads)).unwrap());
        println!("read_ms={:.1}", began.elapsed().as_secs_f64() * 1e3);
    }
    println!("PROFILE_COMPLETE elapsed_seconds={:.3}", started.elapsed().as_secs_f64());
    db.close().unwrap();
}
