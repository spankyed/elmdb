//! How long a write waits while a rank index drains: `rank-write-stall [DOCUMENTS] [CHANGES]`, built with `--release`.
//! One edge of the benchmark graph is linked and unlinked, each change drained, while another thread makes a small write
//! every 2 ms: a field of one document, which leaves the index alone, or with `--edges`, an edge the index reads. Answers
//! the writes' latencies.
#[path = "../tests/support/mod.rs"]
#[allow(dead_code)]
mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let edges = args.iter().any(|a| a == "--edges");
    let mut numbers = args.iter().filter(|a| *a != "--edges");
    let rows: usize = numbers.next().map(|s| s.parse().expect("a document count")).unwrap_or(100_000);
    let changes: usize = numbers.next().map(|s| s.parse().expect("a change count")).unwrap_or(20);
    let fixture = support::Fixture::ranked_graph(rows, 5);
    let batch = |op: serde_json::Value| elmdb::engine::batch::Batch {
        ops: vec![serde_json::from_value(op).unwrap()],
        now: Some(1.0),
    };
    let change = |op: &str| {
        batch(serde_json::json!({ "op": op, "source": "Doc-00000000", "kind": "refs",
            "target": format!("Doc-{:08}", rows / 2) }))
    };
    let (link, unlink) = (change("link"), change("unlink"));
    let done = AtomicBool::new(false);
    let mut waits: Vec<Duration> = std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let mut waits = Vec::new();
            let mut i = 0usize;
            while !done.load(Ordering::SeqCst) {
                let write = if edges {
                    let op = if i.is_multiple_of(2) { "link" } else { "unlink" };
                    batch(serde_json::json!({ "op": op, "source": "Doc-00000001", "kind": "refs",
                        "target": format!("Doc-{:08}", rows / 3) }))
                } else {
                    batch(serde_json::json!({ "op": "set", "id": "Doc-00000002", "fields": { "body": format!("written {i}") } }))
                };
                let started = Instant::now();
                fixture.db.write(&write, false).unwrap();
                waits.push(started.elapsed());
                i += 1;
                std::thread::sleep(Duration::from_millis(2));
            }
            waits
        });
        let started = Instant::now();
        for _ in 0..changes {
            fixture.db.write(&link, false).unwrap();
            // A writer of edges leaves the index always owing a drain, so nothing would see the builder idle
            if edges {
                std::thread::sleep(Duration::from_millis(500));
            } else {
                fixture.db.wait_for_index_builds().unwrap();
            }
            fixture.db.write(&unlink, false).unwrap();
            if edges {
                std::thread::sleep(Duration::from_millis(500));
            } else {
                fixture.db.wait_for_index_builds().unwrap();
            }
        }
        done.store(true, Ordering::SeqCst);
        let waits = writer.join().unwrap();
        fixture.db.wait_for_index_builds().unwrap();
        println!("changes took {:.3} s", started.elapsed().as_secs_f64());
        waits
    });
    waits.sort();
    let at = |q: f64| waits[((waits.len() - 1) as f64 * q) as usize];
    println!(
        "writes={} p50={:?} p90={:?} p99={:?} max={:?} total={:?}",
        waits.len(),
        at(0.5),
        at(0.9),
        at(0.99),
        at(1.0),
        waits.iter().sum::<Duration>()
    );
    std::hint::black_box(&fixture.directory);
}
