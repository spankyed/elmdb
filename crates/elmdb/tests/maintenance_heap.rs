//! One test per executable: no other test can allocate concurrently inside a measured window.
mod support;

use std::hint::black_box;

use support::Fixture;

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

fn measure(name: &str, budget_mib: usize, operation: impl FnOnce()) {
    let _profiler = dhat::Profiler::builder().testing().trim_backtraces(Some(4)).build();
    operation();
    let stats = dhat::HeapStats::get();
    eprintln!("{name}: peak_heap_bytes={} budget_bytes={}", stats.max_bytes, budget_mib * 1024 * 1024);
    dhat::assert!(stats.max_bytes <= budget_mib * 1024 * 1024);
}

#[test]
#[ignore = "dedicated heap gate: cargo test -p elmdb --release --test maintenance_heap -- --ignored --nocapture"]
fn maintenance_heap_bounds() {
    // Creating/opening the database and deriving the initial indexes are deliberately outside the profiler.
    // These are workload-specific Rust heap budgets, not limits on LMDB mmap, RSS or arbitrary records.
    for rows in [1000, 10_000] {
        let fixture = Fixture::new(rows, false, true);
        measure(&format!("export/{rows}"), 24, || {
            let mut entities = 0;
            let mut relations = 0;
            fixture
                .db
                .primary_loads(|load| {
                    match load {
                        elmdb::engine::maintenance::Load::Entity { .. } => entities += 1,
                        elmdb::engine::maintenance::Load::Relation { .. } => relations += 1,
                        _ => {}
                    }
                    Ok(())
                })
                .unwrap();
            assert_eq!((entities, relations), (fixture.rows, fixture.rows));
        });
        measure(&format!("value_rebuild/{rows}"), 16, || fixture.db.rebuild(Some(("Doc", "tags"))).unwrap());
        fixture.assert_limited_query();
        assert!(fixture.db.check().unwrap().is_clean());
        black_box(&fixture.directory);
    }
    for rows in [1000, 10_000] {
        let fixture = Fixture::new(rows, true, false);
        measure(&format!("text_rebuild/{rows}"), 32, || fixture.db.rebuild(Some(("Doc", "text"))).unwrap());
        assert!(fixture.db.check().unwrap().is_clean());
    }
}
