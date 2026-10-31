//! Public API benchmarks: setup is excluded; mutations repeatedly replace the same logical index contents.

#[path = "../tests/support/mod.rs"]
mod support;

use std::hint::black_box;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use elmdb::format::key::{Part, encode};
use support::Fixture;

/// The entity counts the maintenance cases run at: 1,000 unless `ELMDB_BENCH_ROWS` lists others, comma-separated, as
/// `1000,10000` does to see how each cost grows with the data. A size not asked for builds no fixture
fn sizes() -> Vec<usize> {
    std::env::var("ELMDB_BENCH_ROWS").map_or_else(
        |_| vec![1000],
        |sizes| sizes.split(',').map(|n| n.trim().parse().expect("ELMDB_BENCH_ROWS lists entity counts")).collect(),
    )
}

fn maintenance(c: &mut Criterion) {
    let mut keys = c.benchmark_group("key_encode");
    for size in [16, 128, 1024] {
        let parts = [Part::String("Doc".into()), Part::String("x".repeat(size)), Part::Number(42.0)];
        keys.throughput(Throughput::Bytes(size as u64));
        keys.bench_with_input(BenchmarkId::from_parameter(size), &parts, |b, parts| {
            b.iter(|| black_box(encode(black_box(parts))))
        });
    }
    keys.finish();

    for rows in sizes() {
        let fixture = Fixture::new(rows, true, false);
        fixture.assert_limited_query();
        let reads = fixture.limited_query();
        let mut group = c.benchmark_group("maintenance");
        group.throughput(Throughput::Elements(fixture.rows as u64));
        group.bench_function(BenchmarkId::new("value_rebuild", rows), |b| {
            b.iter(|| fixture.db.rebuild(Some(("Doc", "tags"))).unwrap())
        });
        group.bench_function(BenchmarkId::new("text_rebuild", rows), |b| {
            b.iter(|| fixture.db.rebuild(Some(("Doc", "text"))).unwrap())
        });
        group.bench_function(BenchmarkId::new("export", rows), |b| {
            b.iter(|| {
                let mut count = 0usize;
                fixture
                    .db
                    .primary_loads(|load| {
                        black_box(load);
                        count += 1;
                        Ok(())
                    })
                    .unwrap();
                black_box(count)
            })
        });
        group.finish();
        // A query returns at most 50 IDs; its input corpus size belongs in the ID, not in throughput.
        c.bench_function(&format!("indexed_limit/{rows}"), |b| {
            b.iter(|| black_box(fixture.db.read(black_box(&reads)).unwrap()))
        });
        assert!(fixture.db.check().unwrap().is_clean());
        black_box(&fixture.directory);

        // Five edges a document: building the graph and running each algorithm over it
        let graph = Fixture::graph(rows, 5);
        let mut group = c.benchmark_group("graph");
        group.throughput(Throughput::Elements(rows as u64));
        for read in ["weakComponents", "rank", "personalized"] {
            graph.assert_graph_read(read);
            let request = graph.graph_read(read);
            group.bench_function(BenchmarkId::new(read, rows), |b| {
                b.iter(|| black_box(graph.db.read(black_box(&request)).unwrap()))
            });
        }
        group.finish();
        black_box(&graph.directory);
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(20).warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(3));
    targets = maintenance
}
criterion_main!(benches);
