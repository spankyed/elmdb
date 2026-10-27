//! A fixed text-rebuild CPU workload. Attach a native sampler after PROFILE_READY; preparation is excluded.
#[path = "../tests/support/mod.rs"]
mod support;

use std::io::Write;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let rows: usize = args.next().map(|s| s.parse().expect("positive row count")).unwrap_or(10_000);
    let iterations: usize = args.next().map(|s| s.parse().expect("positive iteration count")).unwrap_or(30);
    assert!(rows > 0 && iterations > 0 && args.next().is_none(), "usage: profile-maintenance [rows iterations]");
    let fixture = support::Fixture::new(rows, true, false);
    fixture.assert_limited_query();
    println!("PROFILE_READY pid={} rows={rows} iterations={iterations}", std::process::id());
    std::io::stdout().flush().unwrap();
    let started = Instant::now();
    for _ in 0..iterations {
        fixture.db.rebuild(Some(("Doc", "text"))).unwrap();
    }
    println!("PROFILE_COMPLETE elapsed_seconds={:.3}", started.elapsed().as_secs_f64());
    assert!(fixture.db.check().unwrap().is_clean());
    std::hint::black_box(&fixture.directory);
}
