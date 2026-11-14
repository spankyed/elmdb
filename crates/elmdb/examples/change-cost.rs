//! Where the time of a unit goes that changes one field of a thousand entities, by the field: one no index reads, a
//! string one value index files, a string two value indexes file, an enum of two values, and a number. Each unit's
//! entities are spread over the type as a program's changes are, and one reading changes a thousand that are next to
//! each other instead, so what the spread costs shows apart from what an index does. Prints microseconds of CPU and of
//! wall time per changed entity, the median of five units, no entity in two of them, and the bytes of each index.
//! Then what a create costs in three types as large, of a title and a string each: the string in no index, in one,
//! and in two.
//!
//! `cargo run --release --example change-cost -- 1000000`: the entities to load, 100,000 when none is given.

use std::time::Instant;

use elmdb::{Database, OpenOptions};
use serde_json::{Value, json};

const WORDS: [&str; 12] =
    ["amber", "birch", "cedar", "dune", "ember", "fjord", "glade", "heath", "isle", "juniper", "kelp", "larch"];
const UNIT: usize = 1000;
const UNITS: usize = 5;
const TITLE: &str = "a note of ordinary size, as long as a line of a page someone wrote and left for later reading";

/// An id whose order among the keys is its number's
fn id(i: usize) -> String {
    format!("Note-{i:08}")
}

/// A string no other entity holds, and with `turn` 1 another, elsewhere in the strings' order
fn string_of(i: usize, turn: usize) -> String {
    format!("{} {i}", WORDS[(i + 5 * turn) % WORDS.len()])
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

/// Nothing, on a platform without `getrusage`: the wall time is the reading there
#[cfg(not(unix))]
fn cpu_micros() -> f64 {
    0.0
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

/// The entities of the unit numbered `unit`: spread over the `loaded`, or next to each other
fn entities(unit: usize, loaded: usize, spread: bool) -> Vec<usize> {
    (0..UNIT).map(|k| if spread { ((unit * UNIT + k) * 7919) % loaded } else { unit * UNIT + k }).collect()
}

/// Five units of a thousand ops each, those of the unit numbered `unit` being what `ops` gives
fn reading(db: &Database, label: &str, first: usize, ops: impl Fn(usize) -> Vec<Value>) {
    let (mut cpu, mut wall) = (Vec::new(), Vec::new());
    for unit in first..first + UNITS {
        let batch = serde_json::from_value(json!({ "ops": ops(unit), "now": 2.0 })).unwrap();
        let (used, began) = (cpu_micros(), Instant::now());
        db.write(&batch, false).unwrap();
        wall.push(began.elapsed().as_secs_f64() * 1e6 / UNIT as f64);
        cpu.push((cpu_micros() - used) / UNIT as f64);
    }
    println!("{label}: cpu={:.1}us wall={:.1}us", median(cpu), median(wall));
}

/// The ops setting `field` of a unit's entities to what `value` gives
fn sets(unit: usize, loaded: usize, spread: bool, field: &str, value: impl Fn(usize) -> Value) -> Vec<Value> {
    entities(unit, loaded, spread)
        .into_iter()
        .map(|i| json!({ "op": "set", "id": id(i), "fields": { field: value(i) }, "touch": false }))
        .collect()
}

/// The ops creating the entities numbered from `from` to `to` of a type of a title and a string
fn creates(entity_type: &str, from: usize, to: usize) -> Vec<Value> {
    (from..to)
        .map(|i| {
            let fields = json!({ "title": TITLE, "s": string_of(i, 0) });
            json!({ "op": "create", "type": entity_type, "id": format!("{entity_type}-{i:08}"), "fields": fields })
        })
        .collect()
}

fn main() {
    let loaded: usize = std::env::args().nth(1).map_or(100_000, |n| n.parse().unwrap());
    assert!(loaded >= 40 * UNIT && !loaded.is_multiple_of(7919), "the units' entities would not be distinct");
    let dir = tempfile::tempdir().unwrap();
    let string = json!({ "type": "string" });
    let schema = elmdb::schema::Schema::from_json(&json!({
        "$schema": elmdb::schema::SCHEMA_LANGUAGE,
        "types": { "Note": {
            "fields": {
                "title": string, "plain": string, "name": string, "alias": string, "n": { "type": "number" },
                "status": { "type": "enum", "values": ["open", "done"] },
            },
            "indexes": {
                "name": { "kind": "value", "fields": ["name"] },
                "alias": { "kind": "value", "fields": ["alias"] },
                "aliasAgain": { "kind": "value", "fields": ["alias"] },
                "n": { "kind": "value", "fields": ["n"] },
                "status": { "kind": "value", "fields": ["status"] },
            },
        },
        "Bare": { "fields": { "title": string, "s": string } },
        "Once": { "fields": { "title": string, "s": string }, "indexes": { "s": { "kind": "value", "fields": ["s"] } } },
        "Twice": { "fields": { "title": string, "s": string }, "indexes": {
            "s": { "kind": "value", "fields": ["s"] }, "sAgain": { "kind": "value", "fields": ["s"] },
        } } },
        "relations": {}, "roles": {}
    }))
    .unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() }).unwrap();
    for from in (0..loaded).step_by(10_000) {
        let ops: Vec<Value> = (from..(from + 10_000).min(loaded))
            .map(|i| {
                let fields = json!({
                    "title": TITLE, "plain": string_of(i, 0), "name": string_of(i, 0), "alias": string_of(i, 0),
                    "n": (i * 7919) % loaded, "status": if i % 2 == 0 { "open" } else { "done" },
                });
                json!({ "op": "create", "type": "Note", "id": id(i), "fields": fields })
            })
            .collect();
        db.write(&serde_json::from_value(json!({ "ops": ops, "now": 1.0 })).unwrap(), false).unwrap();
    }
    db.wait_for_index_builds().unwrap();
    let stats = db.stats().unwrap();
    println!("loaded={loaded} fileBytes={}", stats.file_bytes);
    for (index, bytes) in &stats.index_bytes {
        println!("indexBytes {index}={bytes}");
    }
    let other = |i: usize| json!(string_of(i, 1));
    // Units no reading counts, so the first that counts doesn't pay for what the load left cold
    reading(&db, "warm", 0, |unit| sets(unit, loaded, true, "plain", other));
    reading(&db, "noIndexSpread", 5, |unit| sets(unit, loaded, true, "plain", other));
    reading(&db, "noIndexAdjacent", 10, |unit| sets(unit, loaded, false, "plain", other));
    reading(&db, "oneStringIndex", 15, |unit| sets(unit, loaded, true, "name", other));
    reading(&db, "twoStringIndexes", 20, |unit| sets(unit, loaded, true, "alias", other));
    let flipped = |i: usize| json!(if i.is_multiple_of(2) { "done" } else { "open" });
    reading(&db, "enumIndex", 25, |unit| sets(unit, loaded, true, "status", flipped));
    reading(&db, "numberIndex", 30, |unit| sets(unit, loaded, true, "n", |i| json!(((i + 1) * 7919) % loaded)));
    // The first again, last: what the readings between them did to the file shows in how far the two differ
    reading(&db, "noIndexSpreadAgain", 35, |unit| sets(unit, loaded, true, "plain", other));
    // A create: its entity goes after the last of its type, and its string's place where the string sorts
    for entity_type in ["Bare", "Once", "Twice"] {
        for from in (0..loaded).step_by(10_000) {
            let ops = creates(entity_type, from, (from + 10_000).min(loaded));
            db.write(&serde_json::from_value(json!({ "ops": ops, "now": 1.0 })).unwrap(), false).unwrap();
        }
    }
    db.wait_for_index_builds().unwrap();
    for (entity_type, label) in [("Bare", "createNoIndex"), ("Once", "createOneIndex"), ("Twice", "createTwoIndexes")] {
        reading(&db, label, 0, |unit| creates(entity_type, loaded + unit * UNIT, loaded + (unit + 1) * UNIT));
    }
    db.close().unwrap();
}
