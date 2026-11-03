//! How long a write waits while a rank index drains: `rank-write-stall [DOCUMENTS] [CHANGES]`, built with `--release`.
//! One edge of the benchmark graph is linked and unlinked, each change drained, while another thread makes a small write
//! every 2 ms: a field of one document, which leaves the index alone, or with `--edges`, an edge the index reads. Answers
//! the writes' latencies, and how fresh the scores stayed: what a reader sampling every 5 ms found the index's reported
//! error to be, and how long it stayed past what a built index reports.
#[path = "../tests/support/mod.rs"]
#[allow(dead_code)]
mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// What a built index at the default tolerance reports at most: its bound, plus up to half that for rounding its scores,
/// rounded up to one significant figure
const BUILT: f64 = 2e-6;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let bursts = args.iter().any(|a| a == "--bursts");
    let edges = bursts || args.iter().any(|a| a == "--edges");
    let mut numbers = args.iter().filter(|a| !a.starts_with("--"));
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
    let ranked = fixture.graph_read("ranked");
    let (mut waits, mut errors, longest): (Vec<Duration>, Vec<f64>, Duration) = std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            let (mut errors, mut longest, mut past_since) = (Vec::new(), Duration::ZERO, None);
            while !done.load(Ordering::SeqCst) {
                let bytes = fixture.db.read(&ranked).unwrap();
                let answer: serde_json::Value = rmp_serde::from_slice(&bytes).unwrap();
                // No figure is a bound too wide to report
                let error = answer[0]["rank"]["error"].as_f64().unwrap_or(f64::INFINITY);
                errors.push(error);
                if error > BUILT {
                    let since = *past_since.get_or_insert_with(Instant::now);
                    longest = longest.max(since.elapsed());
                } else {
                    past_since = None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            (errors, longest)
        });
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
                std::thread::sleep(Duration::from_millis(if bursts && i.is_multiple_of(20) { 300 } else { 2 }));
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
        let (errors, longest) = reader.join().unwrap();
        (waits, errors, longest)
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
    errors.sort_by(f64::total_cmp);
    let error = |q: f64| errors[((errors.len() - 1) as f64 * q) as usize];
    let past = errors.iter().filter(|e| **e > BUILT).count() as f64 / errors.len() as f64;
    println!(
        "reads={} error_p50={:e} error_p99={:e} error_max={:e} past_share={:.3} past_longest={:?}",
        errors.len(),
        error(0.5),
        error(0.99),
        error(1.0),
        past,
        longest
    );
    std::hint::black_box(&fixture.directory);
}
