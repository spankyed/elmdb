//! Instruction counts of the operations the Criterion benchmarks time (`maintenance.rs`, `reads.rs`), each run once
//! under Valgrind's Callgrind. A count doesn't move with the machine's load, so two runs of one commit read the same
//! and a change shows in one run of each (`docs/architecture/testing.md`, Instruction counts). Each setup, its fixture
//! and its answer check are outside the count. Valgrind runs on Linux, so elsewhere this says so and exits.

#[cfg(target_os = "linux")]
#[path = "../tests/support/mod.rs"]
mod support;

#[cfg(target_os = "linux")]
#[path = "../tests/support/reads.rs"]
mod reads;

#[cfg(target_os = "linux")]
use std::hint::black_box;

#[cfg(target_os = "linux")]
use elmdb::engine::batch::Batch;
#[cfg(target_os = "linux")]
use elmdb::engine::plan::Reads;
#[cfg(target_os = "linux")]
use elmdb::format::key::{Part, encode};
#[cfg(target_os = "linux")]
use gungraun::{library_benchmark, library_benchmark_group, main};

/// The rows the maintenance operations run over: the smaller of the Criterion benchmark's two sizes, which Valgrind's
/// slowdown keeps to seconds
#[cfg(target_os = "linux")]
const ROWS: usize = 1000;

/// The messages the reads run over, as many as the Criterion read benchmark's
#[cfg(target_os = "linux")]
const MESSAGES: usize = 10_000;

#[cfg(target_os = "linux")]
fn key_parts(size: usize) -> [Part; 3] {
    [Part::String("Doc".into()), Part::String("x".repeat(size)), Part::Number(42.0)]
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[benches::bytes(args = [16, 128, 1024], setup = key_parts)]
fn key_encode(parts: [Part; 3]) -> Vec<u8> {
    black_box(encode(black_box(&parts)))
}

#[cfg(target_os = "linux")]
fn fixture() -> support::Fixture {
    let fixture = support::Fixture::new(ROWS, true, false);
    fixture.assert_limited_query();
    // The store's directory lives as long as the fixture the benchmark hands back
    assert!(fixture.directory.path().is_dir());
    fixture
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::rows(setup = fixture)]
fn value_rebuild(fixture: support::Fixture) -> support::Fixture {
    fixture.db.rebuild(Some(("Doc", "tags"))).unwrap();
    fixture
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::rows(setup = fixture)]
fn text_rebuild(fixture: support::Fixture) -> support::Fixture {
    fixture.db.rebuild(Some(("Doc", "text"))).unwrap();
    fixture
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::rows(setup = fixture)]
fn export(fixture: support::Fixture) -> (support::Fixture, usize) {
    let mut count = 0usize;
    fixture
        .db
        .primary_loads(|load| {
            black_box(load);
            count += 1;
            Ok(())
        })
        .unwrap();
    (fixture, black_box(count))
}

#[cfg(target_os = "linux")]
fn limited() -> (support::Fixture, Reads) {
    let fixture = fixture();
    let reads = fixture.limited_query();
    (fixture, reads)
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::rows(setup = limited)]
fn indexed_limit((fixture, reads): (support::Fixture, Reads)) -> (support::Fixture, Vec<u8>) {
    let answer = fixture.db.read(black_box(&reads)).unwrap();
    (fixture, black_box(answer))
}

/// The message store and one of its workloads' request, as the binding sends it, its answer checked
#[cfg(target_os = "linux")]
fn workload(name: &str) -> (reads::Messages, Vec<u8>) {
    let messages = reads::Messages::new(MESSAGES);
    let workload = messages.workloads().into_iter().find(|w| w.name == name).expect("a workload of that name");
    workload.assert_answer(&messages.db, messages.rows);
    assert!(messages.directory.path().is_dir());
    let request = rmp_serde::to_vec_named(&workload.reads).unwrap();
    (messages, request)
}

#[cfg(target_os = "linux")]
#[library_benchmark]
#[benches::workload(args = ["field", "page", "page_projected", "all_rows"], setup = workload)]
fn read_bytes((messages, request): (reads::Messages, Vec<u8>)) -> (reads::Messages, Vec<u8>) {
    let answer = messages.db.read_bytes(black_box(&request)).unwrap();
    (messages, black_box(answer))
}

#[cfg(target_os = "linux")]
library_benchmark_group!(name = keys, benchmarks = [key_encode]);

#[cfg(target_os = "linux")]
library_benchmark_group!(name = maintenance, benchmarks = [value_rebuild, text_rebuild, export, indexed_limit]);

#[cfg(target_os = "linux")]
fn one_write() -> (support::Fixture, Batch) {
    let set = serde_json::json!({"op":"set", "id":"Doc-00000000", "fields":{"body":"changed"}});
    (support::Fixture::new(ROWS, false, false), Batch { ops: vec![serde_json::from_value(set).unwrap()], now: None })
}

// One plain write: asking for the writer, the change and its commit, not synced
#[cfg(target_os = "linux")]
#[library_benchmark]
#[bench::one(setup = one_write)]
fn small_write((fixture, batch): (support::Fixture, Batch)) -> support::Fixture {
    black_box(fixture.db.write(black_box(&batch), false).unwrap());
    fixture
}

#[cfg(target_os = "linux")]
library_benchmark_group!(name = write, benchmarks = [small_write]);

#[cfg(target_os = "linux")]
library_benchmark_group!(name = read, benchmarks = [read_bytes]);

#[cfg(target_os = "linux")]
fn graph_read(read: &str) -> (support::Fixture, Reads) {
    let fixture = support::Fixture::graph(ROWS, 5);
    fixture.assert_graph_read(read);
    let reads = fixture.graph_read(read);
    (fixture, reads)
}

// A graph read over 1,000 documents of five edges each: the graph built from the store and the algorithm over it
#[cfg(target_os = "linux")]
#[library_benchmark]
#[benches::read(args = ["components", "rank", "personalized"], setup = graph_read)]
fn graph((fixture, reads): (support::Fixture, Reads)) -> (support::Fixture, Vec<u8>) {
    let answer = fixture.db.read(black_box(&reads)).unwrap();
    (fixture, black_box(answer))
}

#[cfg(target_os = "linux")]
library_benchmark_group!(name = graphs, benchmarks = [graph]);

#[cfg(target_os = "linux")]
main!(library_benchmark_groups = keys, maintenance, read, write, graphs);

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("The instruction counts run under Valgrind, on Linux: docs/architecture/testing.md, Instruction counts");
}
