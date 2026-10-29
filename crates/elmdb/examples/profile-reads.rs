//! One of the read benchmark's reads, repeated, for a native sampler: `profile-reads WORKLOAD [ITERATIONS]`, built with
//! `--profile profiling`. The message store is built and the answer checked before PROFILE_READY; only the reads follow.
#[path = "../tests/support/reads.rs"]
mod reads;

use std::io::Write;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let name = args.next().expect("usage: profile-reads WORKLOAD [ITERATIONS]");
    let iterations: usize = args.next().map(|s| s.parse().expect("positive iteration count")).unwrap_or(10_000);
    let messages = reads::Messages::new(10_000);
    let workload = messages
        .workloads()
        .into_iter()
        .find(|w| w.name == name)
        .unwrap_or_else(|| panic!("no workload {name}: field, page, page_projected or all_rows"));
    workload.assert_answer(&messages.db, messages.rows);
    let request = rmp_serde::to_vec_named(&workload.reads).unwrap();
    println!("PROFILE_READY pid={} workload={name} iterations={iterations}", std::process::id());
    std::io::stdout().flush().unwrap();
    let started = Instant::now();
    for _ in 0..iterations {
        std::hint::black_box(messages.db.read_bytes(std::hint::black_box(&request)).unwrap());
    }
    println!("PROFILE_COMPLETE elapsed_seconds={:.3}", started.elapsed().as_secs_f64());
    std::hint::black_box(&messages.directory);
}
