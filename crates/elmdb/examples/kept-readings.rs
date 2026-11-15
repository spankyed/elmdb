//! The readings the grouped mode's kept commit is held to (`docs/roadmap/synced-commit.md`, phase 3), through the public
//! API alone, so the same file runs on a revision from before it for the reading beside today's:
//!
//! - `cost`: a `durable` write, a commit of one process, and a second process's first write after a pause while
//!   another writes. Prints the median wall time of each, in microseconds.
//! - `beside`: one process rewriting values for `SECS` seconds, alone and then with a second process that wrote once
//!   and stayed open. Prints the data file's bytes after each.
//! - `rewrite`: `NOTES` notes loaded, then a unit rewriting a tenth of them each second for `SECS` seconds. Prints the
//!   data file's bytes after the load, the most it held while the units ran, and after.
//!
//! `cargo run --release -p elmdb --example kept-readings -- cost | beside [SECS] | rewrite [NOTES] [SECS]`. Each run is
//! one process's at a time on the machine (`docs/architecture/testing.md`), on a fresh temporary directory.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use elmdb::{Database, OpenOptions};
use serde_json::{Value, json};

const TITLE: &str = "a note of ordinary size, as long as a line of a page someone wrote and left for later reading";

fn schema() -> elmdb::schema::Schema {
    elmdb::schema::Schema::from_json(&json!({
        "$schema": elmdb::schema::SCHEMA_LANGUAGE,
        "types": { "Note": { "fields": { "title": { "type": "string" }, "n": { "type": "number" } } } },
        "relations": {}, "roles": {}
    }))
    .unwrap()
}

/// The grouped mode as a program opens it, its timers as they ship
fn open(dir: &Path) -> Database {
    Database::open(dir, OpenOptions { schema: Some(schema()), ..Default::default() }).unwrap()
}

fn write(db: &Database, ops: Vec<Value>, durable: bool) {
    db.write(&serde_json::from_value(json!({ "ops": ops, "now": 1.0 })).unwrap(), durable).unwrap();
}

fn set(id: usize, n: usize) -> Value {
    json!({ "op": "set", "id": format!("Note-{id:08}"), "fields": { "n": n } })
}

fn create(id: usize) -> Value {
    json!({ "op": "create", "type": "Note", "id": format!("Note-{id:08}"), "fields": { "title": TITLE, "n": id } })
}

fn load(db: &Database, notes: usize) {
    for from in (0..notes).step_by(10_000) {
        write(db, (from..(from + 10_000).min(notes)).map(create).collect(), false);
    }
    db.durable().unwrap().wait().unwrap();
}

fn median(mut samples: Vec<f64>) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn file_bytes(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("data.mdb")).map_or(0, |m| m.len())
}

/// This example again, as a second process doing `what` to the database in `dir` until its input ends
fn second(dir: &Path, what: &str) -> std::process::Child {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([what, dir.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut ready = String::new();
    std::io::BufReader::new(child.stdout.as_mut().unwrap()).read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), "ready", "the second process started");
    child
}

/// Ends a second process: its input closed, then its exit waited for
fn end(mut child: std::process::Child) {
    drop(child.stdin.take());
    child.wait().unwrap();
}

fn cost() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    load(&db, 1_000);
    let timed = |run: &dyn Fn(usize)| -> f64 {
        let samples = (0..200)
            .map(|i| {
                let began = Instant::now();
                run(i);
                began.elapsed().as_secs_f64() * 1e6
            })
            .collect();
        median(samples)
    };
    let durable = timed(&|i| write(&db, vec![set(i % 1_000, i)], true));
    let commit = timed(&|i| write(&db, vec![set(i % 1_000, i)], false));
    // Another process writing all the while; this one writes after a pause, so that the other has committed since
    let other = second(dir.path(), "writer");
    let beside = timed(&|i| {
        std::thread::sleep(Duration::from_millis(20));
        write(&db, vec![set(i % 1_000, i)], false);
    });
    end(other);
    db.close().unwrap();
    // The pause is the reading's, not the write's
    println!("durableWrite={durable:.0}us commit={commit:.1}us firstWriteBesideAnother={:.0}us", beside - 20_000.0);
}

fn beside(secs: u64) {
    let rewrite = |dir: &Path| {
        let db = open(dir);
        load(&db, 10_000);
        let began = Instant::now();
        let mut n = 0;
        while began.elapsed() < Duration::from_secs(secs) {
            write(&db, (0..100).map(|k| set((n * 100 + k) % 10_000, n)).collect(), false);
            n += 1;
        }
        db.close().unwrap();
        file_bytes(dir)
    };
    let alone = tempfile::tempdir().unwrap();
    let alone_bytes = rewrite(alone.path());
    let with = tempfile::tempdir().unwrap();
    open(with.path()).close().unwrap();
    let idle = second(with.path(), "idle");
    let with_bytes = rewrite(with.path());
    end(idle);
    println!("alone={alone_bytes} besideAnIdleProcess={with_bytes} secs={secs}");
}

fn rewrite(notes: usize, secs: u64) {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    load(&db, notes);
    let loaded = file_bytes(dir.path());
    let (began, mut most, mut second) = (Instant::now(), loaded, 0usize);
    while began.elapsed() < Duration::from_secs(secs) {
        let tenth = notes / 10;
        let from = (second * tenth) % notes;
        let mut unit = db.begin().unwrap();
        for chunk in (from..from + tenth).collect::<Vec<_>>().chunks(10_000) {
            let ops = chunk.iter().map(|id| set(id % notes, second)).collect::<Vec<_>>();
            unit.write(&serde_json::from_value(json!({ "ops": ops, "now": 1.0 })).unwrap()).unwrap();
        }
        unit.commit(false).unwrap();
        most = most.max(file_bytes(dir.path()));
        second += 1;
        let next = began + Duration::from_secs(second as u64);
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
    }
    db.close().unwrap();
    println!("notes={notes} loaded={loaded} most={most} after={} units={second}", file_bytes(dir.path()));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("cost") => cost(),
        Some("beside") => beside(args.get(1).map_or(60, |s| s.parse().unwrap())),
        Some("rewrite") => rewrite(
            args.get(1).map_or(1_000_000, |s| s.parse().unwrap()),
            args.get(2).map_or(60, |s| s.parse().unwrap()),
        ),
        // A second process: writing all the while, or open having written once, until its input ends
        Some(what @ ("writer" | "idle")) => {
            let db = open(Path::new(&args[1]));
            write(&db, vec![create(1_000_000 + std::process::id() as usize)], true);
            println!("ready");
            std::io::stdout().flush().unwrap();
            if what == "writer" {
                let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let stopping = std::sync::Arc::clone(&stop);
                std::thread::spawn(move || {
                    let _ = std::io::stdin().read_line(&mut String::new());
                    stopping.store(true, std::sync::atomic::Ordering::Relaxed);
                });
                let mut n = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    write(&db, vec![set(n % 1_000, n)], false);
                    n += 1;
                }
            } else {
                let _ = std::io::stdin().read_line(&mut String::new());
            }
            db.close().unwrap();
        }
        _ => eprintln!("usage: kept-readings cost | beside [SECS] | rewrite [NOTES] [SECS]"),
    }
}
