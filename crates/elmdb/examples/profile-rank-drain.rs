//! One edge of the rank index's benchmark graph linked and unlinked, each change drained, repeated, for a native sampler:
//! `profile-rank-drain [DOCUMENTS] [CHANGES]`, built with `--profile profiling`. The graph and its index are built before
//! PROFILE_READY; only the changes and their drains follow, the drains on the database's own thread.
#[path = "../tests/support/mod.rs"]
#[allow(dead_code)]
mod support;

use std::io::Write;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let rows: usize = args.next().map(|s| s.parse().expect("a document count")).unwrap_or(100_000);
    let changes: usize = args.next().map(|s| s.parse().expect("a change count")).unwrap_or(20);
    let fixture = support::Fixture::ranked_graph(rows, 5);
    let change = |op: &str| elmdb::engine::batch::Batch {
        ops: vec![
            serde_json::from_value(serde_json::json!({ "op": op, "source": "Doc-00000000", "kind": "refs",
                "target": format!("Doc-{:08}", rows / 2) }))
            .unwrap(),
        ],
        now: Some(1.0),
    };
    let (link, unlink) = (change("link"), change("unlink"));
    println!("PROFILE_READY pid={} documents={rows} changes={changes}", std::process::id());
    std::io::stdout().flush().unwrap();
    let started = Instant::now();
    for _ in 0..changes {
        fixture.db.write(&link, false).unwrap();
        fixture.db.wait_for_index_builds().unwrap();
        fixture.db.write(&unlink, false).unwrap();
        fixture.db.wait_for_index_builds().unwrap();
    }
    println!("PROFILE_COMPLETE elapsed_seconds={:.3}", started.elapsed().as_secs_f64());
    std::hint::black_box(&fixture.directory);
}
