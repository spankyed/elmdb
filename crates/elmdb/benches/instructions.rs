//! Instruction counts of the operations the Criterion benchmarks time (`maintenance.rs`, `reads.rs`), each run once
//! under Valgrind's Callgrind. A count doesn't move with the machine's load, so two runs of one commit read the same
//! and a change shows in one run of each (`docs/architecture/testing.md`, Instruction counts). Each setup, its fixture
//! and its answer check are outside the count, but for `rank_drain`'s, which `rank_build` counts alone. Valgrind runs on Linux, so elsewhere this says so and exits.

/// Every allocation of this benchmark, from one address range that only moves forward. glibc's `malloc` costs what the
/// heap's shape makes it, and that shape follows the CPU and the binary: one binary's `read_bytes` differed by 1% between
/// two CPU models with every function of its own the same (`docs/evidence/instruction-counts.md`). Here an allocation
/// costs the same whatever came before it, so a count moves when the work does; a new allocation still costs its
/// instructions, and `scripts/diagnostics-overhead.py` counts them apart. It frees nothing, so `realloc` always copies.
/// C libraries' own `malloc` stays glibc's. Valgrind runs on Linux alone
#[cfg(target_os = "linux")]
mod bump {
    use std::alloc::{GlobalAlloc, Layout};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    /// The range reserved, of which only what is written takes memory: past every case's total, setup included
    /// (the same evidence), and within what Valgrind will map
    const RESERVED: usize = 4 << 30;
    /// What `malloc` promises, so that a copy or a comparison takes the path it would there
    const MIN_ALIGN: usize = 16;

    pub struct Bump;

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    static END: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
    }

    /// Reserves the range. The first allocation is the runtime's, before any other thread
    #[cold]
    fn reserve() {
        const PROT_READ_WRITE: i32 = 1 | 2;
        const MAP_PRIVATE_ANONYMOUS_NORESERVE: i32 = 0x02 | 0x20 | 0x4000;
        let base =
            unsafe { mmap(std::ptr::null_mut(), RESERVED, PROT_READ_WRITE, MAP_PRIVATE_ANONYMOUS_NORESERVE, -1, 0) };
        if base as isize == -1 {
            eprintln!("reserving the benchmark's allocations failed");
            std::process::abort();
        }
        NEXT.store(base as usize, Relaxed);
        END.store(base as usize + RESERVED, Relaxed);
    }

    /// Named so that a Callgrind file can count the allocations, and never inlined so that it stays there to count
    #[inline(never)]
    fn bump_allocation(layout: Layout) -> *mut u8 {
        if END.load(Relaxed) == 0 {
            reserve();
        }
        let align = layout.align().max(MIN_ALIGN);
        let at = NEXT.fetch_add(layout.size() + align - 1, Relaxed);
        let start = (at + align - 1) & !(align - 1);
        if start + layout.size() > END.load(Relaxed) {
            eprintln!("the benchmark allocated past the {RESERVED} bytes reserved");
            std::process::abort();
        }
        start as *mut u8
    }

    unsafe impl GlobalAlloc for Bump {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            bump_allocation(layout)
        }

        unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
    }
}

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: bump::Bump = bump::Bump;

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
use gungraun::{Callgrind, EntryPoint, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group, main};

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
    let fixture = match read {
        "rankWeighted" => support::Fixture::weighted_graph(ROWS, 5),
        "ranked" => support::Fixture::ranked_graph(ROWS, 5),
        _ => support::Fixture::graph(ROWS, 5),
    };
    fixture.assert_graph_read(read);
    let reads = fixture.graph_read(read);
    (fixture, reads)
}

// A graph read over 1,000 documents of five edges each: the graph built from the store and the algorithm over it,
// `rank` again over the same graph weighted, and its top ten read from a rank index over it
#[cfg(target_os = "linux")]
#[library_benchmark]
#[benches::read(
    args = ["weakComponents", "strongComponents", "rank", "rankWeighted", "ranked", "personalized", "communities", "topological", "similar", "similarAdamicAdar"],
    setup = graph_read
)]
fn graph((fixture, reads): (support::Fixture, Reads)) -> (support::Fixture, Vec<u8>) {
    let answer = fixture.db.read(black_box(&reads)).unwrap();
    (fixture, black_box(answer))
}

#[cfg(target_os = "linux")]
library_benchmark_group!(name = graphs, benchmarks = [graph]);

/// Callgrind's name for the index's `build_rank`, as `builder.rs` keeps it a function of its own
#[cfg(target_os = "linux")]
const ENTRY: &str = "elmdb::engine::builder::build_rank";

/// Enough documents that a change over near links stays reading a node at a time, past a quarter of them
#[cfg(target_os = "linux")]
const NEAR_ROWS: usize = 4000;

#[cfg(target_os = "linux")]
fn rank_change(links: &str) -> (support::Fixture, Batch, Batch) {
    let (fixture, rows) = match links {
        "near" => (support::Fixture::ranked_local_graph(NEAR_ROWS, 5), NEAR_ROWS),
        _ => (support::Fixture::ranked_graph(ROWS, 5), ROWS),
    };
    let change = |op: &str| {
        let edge = serde_json::json!({"op":op, "source":"Doc-00000000", "kind":"refs",
            "target":format!("Doc-{:08}", rows / 2)});
        Batch { ops: vec![serde_json::from_value(edge).unwrap()], now: Some(1.0) }
    };
    (fixture, change("link"), change("unlink"))
}

// An edge linked and unlinked, each drained: what the index's thread does for them, counted from `build_rank`, as the
// benchmark's own thread does none of it. A fixture's build goes through `build_rank` too, so `rank_build` counts that alone
// over the same fixture: the drain is one less the other. The entry point names the function, not the closures in it, each
// of which would toggle collection back off for its length
#[cfg(target_os = "linux")]
#[library_benchmark(
    config = LibraryBenchmarkConfig::default().tool(Callgrind::default().entry_point(EntryPoint::Custom(ENTRY.to_owned())))
)]
#[benches::links(args = ["spread", "near"], setup = rank_change)]
fn rank_drain((fixture, link, unlink): (support::Fixture, Batch, Batch)) -> support::Fixture {
    fixture.db.write(black_box(&link), false).unwrap();
    fixture.db.wait_for_index_builds().unwrap();
    fixture.db.write(black_box(&unlink), false).unwrap();
    fixture.db.wait_for_index_builds().unwrap();
    fixture
}

#[cfg(target_os = "linux")]
#[library_benchmark(
    config = LibraryBenchmarkConfig::default().tool(Callgrind::default().entry_point(EntryPoint::Custom(ENTRY.to_owned())))
)]
#[benches::links(args = ["spread", "near"], setup = rank_change)]
fn rank_build((fixture, link, unlink): (support::Fixture, Batch, Batch)) -> support::Fixture {
    black_box((&link, &unlink));
    fixture
}

#[cfg(target_os = "linux")]
library_benchmark_group!(name = rank_drains, benchmarks = [rank_drain, rank_build]);

#[cfg(target_os = "linux")]
main!(library_benchmark_groups = keys, maintenance, read, write, graphs, rank_drains);

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("The instruction counts run under Valgrind, on Linux: docs/architecture/testing.md, Instruction counts");
}
