//! How many heap allocations, and how many bytes, one of each of the read benchmark's reads makes, counted by dhat
//! over a hundred of them (three of the read of every row, which dhat slows most) after one warm-up read. Its own
//! executable, since dhat's allocator is the global one.
#[path = "../tests/support/reads.rs"]
mod reads;

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let json = match args.as_slice() {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => panic!("usage: read-allocations [--json]"),
    };
    let messages = reads::Messages::new(10_000);
    let mut measurements = Vec::new();
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
        measurements.push(serde_json::json!({
            "workload": workload.name,
            "iterations": reads,
            "answerBytes": answer,
            "totalAllocations": stats.total_blocks,
            "totalAllocatedBytes": stats.total_bytes,
            "peakHeapBytes": stats.max_bytes,
        }));
        if !json {
            println!(
                "{}: answer_bytes={answer} allocations_per_read={} allocated_bytes_per_read={} peak_heap_bytes={}",
                workload.name,
                stats.total_blocks / reads,
                stats.total_bytes / reads,
                stats.max_bytes
            );
        }
    }
    if json {
        println!(
            "{}",
            serde_json::json!({
                "schemaVersion": 1,
                "measurement": "rust-read-allocations",
                "fixture": {"version": 1, "rows": messages.rows},
                "instrumentation": "dhat; fixture and warmup excluded; preexisting/native/mapped memory excluded",
                "debugAssertions": cfg!(debug_assertions),
                "workloads": measurements,
            })
        );
    }
    std::hint::black_box(&messages.directory);
}
