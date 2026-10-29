//! How many heap allocations, and how many bytes, one of each of the read benchmark's reads makes, counted by dhat
//! over a hundred of them (three of the read of every row, which dhat slows most) after one warm-up read. Its own
//! executable, since dhat's allocator is the global one.
#[path = "../tests/support/reads.rs"]
mod reads;

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

fn main() {
    let messages = reads::Messages::new(10_000);
    for workload in messages.workloads() {
        workload.assert_answer(&messages.db, messages.rows);
        let request = rmp_serde::to_vec_named(&workload.reads).unwrap();
        let answer = messages.db.read_bytes(&request).unwrap().len();
        let reads: u64 = if workload.name == "all_rows" { 3 } else { 100 };
        let _profiler = dhat::Profiler::builder().testing().build();
        for _ in 0..reads {
            std::hint::black_box(messages.db.read_bytes(std::hint::black_box(&request)).unwrap());
        }
        let stats = dhat::HeapStats::get();
        println!(
            "{}: answer_bytes={answer} allocations_per_read={} allocated_bytes_per_read={} peak_heap_bytes={}",
            workload.name,
            stats.total_blocks / reads,
            stats.total_bytes / reads,
            stats.max_bytes
        );
    }
    std::hint::black_box(&messages.directory);
}
