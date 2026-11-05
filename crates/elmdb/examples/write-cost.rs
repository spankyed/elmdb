//! What a commit costs, by its shape: one row at a time, a unit of 100 rows, and a unit too large for its change log
//! record to name each. Prints each shape's microseconds per commit, so two revisions can be compared in turn
//! (`cargo xtask compare`).

use std::time::Instant;

use elmdb::{Database, OpenOptions};
use serde_json::{Value, json};

fn row(id: String) -> Value {
    json!({ "op": "create", "type": "Task", "id": id, "fields": { "title": "a row of ordinary size", "n": 1 } })
}

fn commits(db: &Database, count: usize, per: usize, tag: &str) -> f64 {
    let start = Instant::now();
    for commit in 0..count {
        let ops: Vec<Value> = (0..per).map(|i| row(format!("Task-{tag}-{commit}-{i}"))).collect();
        let batch = serde_json::from_value(json!({ "ops": ops, "now": 1.0 })).unwrap();
        db.write(&batch, false).unwrap();
    }
    start.elapsed().as_secs_f64() * 1e6 / count as f64
}

fn main() {
    let dir = tempfile::tempdir().unwrap();
    let schema = serde_json::from_value(json!({ "types": { "Task": { "required": [] } } })).unwrap();
    let db = Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap();
    // Past the log's bound, so the runs below pay for trimming too
    commits(&db, 5000, 1, "warm");
    println!("oneRow={:.1}us", commits(&db, 20000, 1, "one"));
    println!("unit100={:.1}us", commits(&db, 1000, 100, "hundred"));
    println!("unit2000={:.1}us", commits(&db, 40, 2000, "large"));
    db.close().unwrap();
}
