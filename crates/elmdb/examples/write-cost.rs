//! What a commit costs, by its shape: one row at a time, a unit of 100 rows, and a unit too large for its change log
//! record to name each, and the first two again for a type keeping a change order. Prints each shape's microseconds of CPU per commit, which a busy machine inflates far less than
//! wall time, so two revisions can be compared in turn (`cargo xtask compare`).

use elmdb::{Database, OpenOptions};
use serde_json::{Value, json};

fn row(id: String) -> Value {
    let entity_type = id.split('-').next().unwrap().to_owned();
    json!({ "op": "create", "type": entity_type, "id": id, "fields": { "title": "a row of ordinary size", "n": 1 } })
}

/// CPU this process has used, user and system, in microseconds
#[cfg(unix)]
fn cpu_micros() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` fills the struct it's handed when it answers 0, which the assert holds it to
    let usage = unsafe {
        assert_eq!(libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()), 0);
        usage.assume_init()
    };
    let micros = |time: libc::timeval| time.tv_sec as f64 * 1e6 + time.tv_usec as f64;
    micros(usage.ru_utime) + micros(usage.ru_stime)
}

/// Wall time on a platform without `getrusage`, which a busy machine inflates: a reading only against itself
#[cfg(not(unix))]
fn cpu_micros() -> f64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1e6
}

fn commits(db: &Database, count: usize, per: usize, tag: &str) -> f64 {
    commits_of(db, "Task", count, per, tag)
}

fn commits_of(db: &Database, entity_type: &str, count: usize, per: usize, tag: &str) -> f64 {
    let start = cpu_micros();
    for commit in 0..count {
        let ops: Vec<Value> = (0..per).map(|i| row(format!("{entity_type}-{tag}-{commit}-{i}"))).collect();
        let batch = serde_json::from_value(json!({ "ops": ops, "now": 1.0 })).unwrap();
        db.write(&batch, false).unwrap();
    }
    (cpu_micros() - start) / count as f64
}

fn main() {
    let dir = tempfile::tempdir().unwrap();
    let schema = elmdb::schema::Schema::from_json(&json!({ "$schema": elmdb::schema::SCHEMA_LANGUAGE, "types": { "Task": { "fields": { "title": { "type": "string" }, "n": { "type": "number" } } }, "Note": { "changeOrder": true, "fields": { "title": { "type": "string" }, "n": { "type": "number" } } } }, "relations": {}, "roles": {} })).unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema.clone()), syncs: false, ..Default::default() })
            .unwrap();
    // Past the log's bound, so the runs below pay for trimming too
    commits(&db, 5000, 1, "warm");
    println!("oneRow={:.1}us", commits(&db, 20000, 1, "one"));
    println!("unit100={:.1}us", commits(&db, 1000, 100, "hundred"));
    println!("unit2000={:.1}us", commits(&db, 40, 2000, "large"));
    db.close().unwrap();
    // The first two again for a type keeping a change order, as a projection's sources do, in a database of their own so
    // they meet one the same size as the shapes above did
    let ordered = tempfile::tempdir().unwrap();
    let db = Database::open(ordered.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() })
        .unwrap();
    commits_of(&db, "Note", 5000, 1, "warm");
    println!("oneRowOrdered={:.1}us", commits_of(&db, "Note", 20000, 1, "one"));
    println!("unit100Ordered={:.1}us", commits_of(&db, "Note", 1000, 100, "hundred"));
    db.close().unwrap();
}
