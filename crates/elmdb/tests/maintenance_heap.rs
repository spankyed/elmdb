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

/// The peak heap of `operation` over a fixture of each size, and how much it grew per entity between them: an
/// operation that keeps something per entity is bounded by what it keeps, where a fixed budget would be invented
fn measure_growth(
    name: &str,
    per_entity_bytes: usize,
    sizes: [usize; 2],
    fixture: impl Fn(usize) -> Fixture,
    operation: impl Fn(&Fixture),
) {
    let peaks = sizes.map(|rows| {
        let fixture = fixture(rows);
        let _profiler = dhat::Profiler::builder().testing().trim_backtraces(Some(4)).build();
        operation(&fixture);
        dhat::HeapStats::get().max_bytes
    });
    let growth = peaks[1].saturating_sub(peaks[0]) / (sizes[1] - sizes[0]);
    eprintln!(
        "{name}/{}..{}: peak_heap_bytes={}..{} growth_per_entity_bytes={growth} allowance_bytes={per_entity_bytes}",
        sizes[0], sizes[1], peaks[0], peaks[1]
    );
    assert!(growth <= per_entity_bytes, "{name} grew {growth} bytes per entity, more than {per_entity_bytes}");
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
    // Full derivation spills every entry but keeps an inventory of every entity: about 300 bytes an entity, measured
    // with nothing indexed. Each entity's index entries come to about 2.4 KB, so both sizes overflow each sorter's 8 MiB,
    // whose state moves a peak by about 2 MB, about 350 bytes an entity over the 6,000 between them; an index's entries
    // kept whole would add some 2.8 KB an entity
    let wide = |rows| Fixture::with_tags(rows, 250, false, false);
    measure_growth("full_rebuild", 1024, [6_000, 12_000], wide, |fixture| fixture.db.rebuild(None).unwrap());
    measure_growth("full_check", 1024, [6_000, 12_000], wide, |fixture| {
        assert!(fixture.db.check().unwrap().is_clean())
    });
    // A selected vector rebuild spills its ids and decodes one added vector at a time, beside hannoy's graph builder:
    // about 320 to 350 bytes a vector, measured, the graph's share varying run to run. One decoded 256-dimension vector is
    // 1 KB, so decoding them all would add that
    let vectors = |rows| Fixture::with_vectors(rows, 256);
    measure_growth("vector_rebuild", 768, [1_000, 2_000], vectors, |fixture| {
        fixture.db.rebuild(Some(("Doc", "meaning"))).unwrap()
    });
}
