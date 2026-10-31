//! Read benchmarks over a fixed message store (`docs/archive/roadmap/read-allocation-codec.md`): each read as the binding makes
//! it, from the request's msgpack bytes to the answer's, its answer checked before it is timed.

#[path = "../tests/support/reads.rs"]
mod reads;

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use reads::Messages;

fn read_bytes(c: &mut Criterion) {
    let messages = Messages::new(10_000);
    let mut group = c.benchmark_group("read_bytes");
    for workload in messages.workloads() {
        workload.assert_answer(&messages.db, messages.rows);
        let request = rmp_serde::to_vec_named(&workload.reads).unwrap();
        group.bench_function(BenchmarkId::from_parameter(workload.name), |b| {
            b.iter(|| black_box(messages.db.read_bytes(black_box(&request)).unwrap()))
        });
    }
    group.finish();
    black_box(&messages.directory);
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(30).warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4));
    targets = read_bytes
}
criterion_main!(benches);
