use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use elmdb_engine::{Engine, EntityKey, IndexKind, RawField, RawFieldKind, RawIndex, RawIndexKind, RawSchema, RawType};
use elmdb_lmdb::{LmdbConfig, LmdbStore};
use elmdb_projections::{BuildPolicy, BuildState, ProjectionRegistry, order, text, trigram, value};
use elmdb_sim::{SimClock, SimKvConfig, SimKvStore, SimRng, SimVfs};
use elmdb_storage::{BootId, DbFlags, DurabilityMode, OsVfs, SystemClock};
use elmdb_wire::{CanonicalValue, EntityTarget, ErrorCode, WireOp};
use rustc_hash::FxHashSet;

fn full_schema() -> RawSchema {
    let mut fields = BTreeMap::new();
    fields.insert("code".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });
    fields.insert("score".into(), RawField { kind: RawFieldKind::Any, optional: true, nullable: true });
    fields.insert("title".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });
    fields.insert("body".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });
    fields.insert(
        "tags".into(),
        RawField { kind: RawFieldKind::List { of: Box::new(RawFieldKind::String) }, optional: true, nullable: true },
    );

    let mut indexes = BTreeMap::new();
    indexes.insert(
        "by_code_unique".into(),
        RawIndex { kind: RawIndexKind::Value { fields: vec!["code".into()], unique: true } },
    );
    indexes.insert(
        "by_score".into(),
        RawIndex { kind: RawIndexKind::Value { fields: vec!["score".into()], unique: false } },
    );
    indexes.insert(
        "title_order_sv".into(),
        RawIndex { kind: RawIndexKind::Order { field: "title".into(), locale: "sv".into() } },
    );
    indexes.insert(
        "search_text".into(),
        RawIndex {
            kind: RawIndexKind::Text { fields: vec![("title".into(), 3), ("body".into(), 1), ("tags".into(), 2)] },
        },
    );
    indexes.insert(
        "title_trigram".into(),
        RawIndex { kind: RawIndexKind::Trigram { fields: vec!["title".into(), "body".into()] } },
    );

    let mut types = BTreeMap::new();
    types.insert("Note".into(), RawType { fields, indexes, ..Default::default() });

    RawSchema { types, relations: BTreeMap::new() }
}

fn open_sim_engine(
    policy: BuildPolicy,
) -> (Engine<SimKvStore, SimVfs, SimClock>, Arc<ProjectionRegistry<SimKvStore, SimVfs>>) {
    let store = SimKvStore::new(SimKvConfig::default());
    let vfs = SimVfs::new(BootId([1; 16]));
    let clock = SimClock::new(1_700_000_000_000);
    let reg = Arc::new(ProjectionRegistry::new(policy));
    let (engine, _) = Engine::open_with_projections(
        store,
        vfs,
        clock,
        PathBuf::from("/sim/db"),
        DurabilityMode::Strict,
        Arc::clone(&reg) as _,
    )
    .expect("open sim engine");
    (engine, reg)
}

fn make_fields(pairs: &[(&str, CanonicalValue)]) -> Vec<(String, CanonicalValue)> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
}

#[test]
fn value_unique_unified_numeric_spill_and_long_string_uniqueness() {
    let (engine, reg) = open_sim_engine(BuildPolicy::DeferredWatermark);
    engine.set_schema(&full_schema()).unwrap();

    // Unified numeric key: Int(5) == Float(5.0), and values past 2^53 stay strictly ordered!
    let p53 = 1i64 << 53;
    let ops = vec![
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n-int5".into()),
            fields: make_fields(&[("code", CanonicalValue::String("A".into())), ("score", CanonicalValue::Int(5))]),
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n-flt5".into()),
            fields: make_fields(&[("code", CanonicalValue::String("B".into())), ("score", CanonicalValue::Float(5.0))]),
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n-p53-1".into()),
            fields: make_fields(&[("score", CanonicalValue::Int(p53 + 1))]),
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n-p53-0".into()),
            fields: make_fields(&[("score", CanonicalValue::Int(p53))]),
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n-unord".into()),
            fields: make_fields(&[("score", CanonicalValue::Object(vec![("x".into(), CanonicalValue::Int(1))]))]),
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n-null".into()),
            fields: make_fields(&[("score", CanonicalValue::Null)]),
        },
    ];
    engine.execute_batch(&ops).unwrap();

    // Unique violation on normal code.
    let dup = engine.execute_batch(&[WireOp::Create {
        type_name: "Note".into(),
        external_id: Some("n-dup".into()),
        fields: make_fields(&[("code", CanonicalValue::String("A".into()))]),
    }]);
    assert_eq!(dup.unwrap_err().code, ErrorCode::UniqueViolation);

    // Trashing n-int5 frees unique code "A", allowing another entity to claim it; restoring n-int5 then fails!
    engine
        .execute_batch(&[
            WireOp::Trash { target: EntityTarget::ExternalId("n-int5".into()) },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n-claim".into()),
                fields: make_fields(&[("code", CanonicalValue::String("A".into()))]),
            },
        ])
        .unwrap();
    let restore_err = engine.execute_batch(&[WireOp::Restore { target: EntityTarget::ExternalId("n-int5".into()) }]);
    assert_eq!(restore_err.unwrap_err().code, ErrorCode::UniqueViolation);
    // Destroy claimant and restore original so n-int5 is live again.
    engine
        .execute_batch(&[
            WireOp::Destroy { target: EntityTarget::ExternalId("n-claim".into()) },
            WireOp::Restore { target: EntityTarget::ExternalId("n-int5".into()) },
        ])
        .unwrap();

    // Unique violation on > 448 B long string!
    let long_a = "x".repeat(500);
    engine
        .execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n-long1".into()),
            fields: make_fields(&[("code", CanonicalValue::String(long_a.clone()))]),
        }])
        .unwrap();
    let long_dup = engine.execute_batch(&[WireOp::Create {
        type_name: "Note".into(),
        external_id: Some("n-long2".into()),
        fields: make_fields(&[("code", CanonicalValue::String(long_a))]),
    }]);
    assert_eq!(long_dup.unwrap_err().code, ErrorCode::UniqueViolation);

    // Verify Int(5) and Float(5.0) live in the same posting bucket and range scan preserves 2^53 ordering.
    {
        let (txn, schema) = engine.begin_read_pinned().unwrap();
        let score_idx = schema.indexes.iter().find(|i| i.name == "by_score").unwrap();
        let db = engine.gate().open_subdb_ro(&txn, &score_idx.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        assert_eq!(value::count_eq(&txn, db, &[&CanonicalValue::Int(5)]).unwrap(), 2);
        assert_eq!(value::count_eq(&txn, db, &[&CanonicalValue::Float(5.0)]).unwrap(), 2);
        let eq_keys = value::lookup_eq(&txn, db, &[&CanonicalValue::Float(5.0)]).unwrap();
        assert_eq!(eq_keys.len(), 2);

        let range = value::scan_range(&txn, db, Some((&[&CanonicalValue::Int(p53)], true)), None, false, None).unwrap();
        assert_eq!(range.len(), 2);
        let k0 = engine.get_row("n-p53-0").unwrap().unwrap().key;
        let k1 = engine.get_row("n-p53-1").unwrap().unwrap().key;
        assert_eq!(range, vec![k0, k1]);
    }

    // Spill > 256 keys into bucketed mode (320 identical scores), check exact equivalence, then shrink back < 256!
    let mut bulk = Vec::new();
    for i in 0..320 {
        bulk.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("bulk-{i}")),
            fields: make_fields(&[("score", CanonicalValue::Int(42))]),
        });
    }
    engine.execute_batch(&bulk).unwrap();
    {
        let (txn, schema) = engine.begin_read_pinned().unwrap();
        let score_idx = schema.indexes.iter().find(|i| i.name == "by_score").unwrap();
        let db = engine.gate().open_subdb_ro(&txn, &score_idx.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        assert_eq!(value::count_eq(&txn, db, &[&CanonicalValue::Int(42)]).unwrap(), 320);
        assert_eq!(value::lookup_eq(&txn, db, &[&CanonicalValue::Int(42)]).unwrap().len(), 320);
    }
    assert!(reg.check(&engine, None).unwrap().is_ok());

    // Delete 150 entities to drop back to 170 (<= 256) and verify canonical promotion back to inline!
    let dels: Vec<WireOp> =
        (0..150).map(|i| WireOp::Destroy { target: EntityTarget::ExternalId(format!("bulk-{i}")) }).collect();
    engine.execute_batch(&dels).unwrap();
    let rep = reg.check(&engine, None).unwrap();
    assert!(rep.is_ok(), "mismatches after bucket->inline shrink: {:?}", rep.mismatches);
}

#[test]
fn icu_order_sv_locale_and_long_cut_tie_resolution() {
    let (engine, reg) = open_sim_engine(BuildPolicy::DeferredWatermark);
    engine.set_schema(&full_schema()).unwrap();

    // In Swedish ("sv"), 'ö' sorts strictly AFTER 'z'!
    // Also include two >300-byte strings sharing an identical 300-byte prefix so `cut == true` tie-break fires!
    let shared_prefix = "m".repeat(300);
    let long_b = format!("{shared_prefix}b");
    let long_a = format!("{shared_prefix}a");

    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n-oe".into()),
                fields: make_fields(&[("title", CanonicalValue::String("ö".into()))]),
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n-z".into()),
                fields: make_fields(&[("title", CanonicalValue::String("z".into()))]),
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n-a".into()),
                fields: make_fields(&[("title", CanonicalValue::String("a".into()))]),
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n-long-b".into()),
                fields: make_fields(&[("title", CanonicalValue::String(long_b))]),
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n-long-a".into()),
                fields: make_fields(&[("title", CanonicalValue::String(long_a))]),
            },
        ])
        .unwrap();

    let (txn, schema) = engine.begin_read_pinned().unwrap();
    let idx = schema.indexes.iter().find(|i| i.name == "title_order_sv").unwrap();
    let IndexKind::Order { field, locale } = &idx.kind else { panic!("expected order") };
    let db = engine.gate().open_subdb_ro(&txn, &idx.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
    let cached = reg.collators().get_or_build(locale).unwrap();
    assert!(order::stamp_matches(&txn, db, &cached).unwrap());

    let ordered = order::scan_ordered(&txn, &engine.dbs(), db, &cached, *field, false, None).unwrap();
    let names: Vec<String> = ordered.iter().map(|&k| engine.resolve_ext_id(k).unwrap().unwrap()).collect();
    assert_eq!(names, vec!["n-a", "n-long-a", "n-long-b", "n-z", "n-oe"]);

    // Reverse top-2: should return n-oe, n-z!
    let rev2 = order::scan_ordered(&txn, &engine.dbs(), db, &cached, *field, true, Some(2)).unwrap();
    let rev_names: Vec<String> = rev2.iter().map(|&k| engine.resolve_ext_id(k).unwrap().unwrap()).collect();
    assert_eq!(rev_names, vec!["n-oe", "n-z"]);

    assert!(reg.check(&engine, None).unwrap().is_ok());
}

#[test]
fn bmw_full_text_matches_exhaustive_reference_and_prunes_blocks() {
    let (engine, reg) = open_sim_engine(BuildPolicy::DeferredWatermark);
    engine.set_schema(&full_schema()).unwrap();

    // Generate 650 documents (> 5 full 128-entity blocks) where "common" appears in almost all docs
    // with low tf, while a few docs have very high title/body tf so BMW prunes whole blocks!
    let mut rng = SimRng::seeded(0xBEEF_1234_CAFE_5678);
    let vocab = ["common", "alpha", "beta", "gamma", "delta", "rust", "lmdb", "graph", "vector", "query"];
    let mut ops = Vec::with_capacity(650);
    for i in 0..650 {
        let title = if i == 17 || i == 290 || i == 512 {
            "common common common rust lmdb THR-42".to_owned()
        } else {
            let w1 = vocab[rng.gen_range(vocab.len())];
            let w2 = vocab[rng.gen_range(vocab.len())];
            format!("common {w1} {w2}")
        };
        let mut body = String::from("common ");
        let body_len = 5 + rng.gen_range(25);
        for _ in 0..body_len {
            body.push_str(vocab[rng.gen_range(vocab.len())]);
            body.push(' ');
        }
        let tags = CanonicalValue::Array(vec![
            CanonicalValue::String("first tag".into()),
            CanonicalValue::String("second tag".into()),
        ]);
        ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("doc-{i}")),
            fields: make_fields(&[
                ("title", CanonicalValue::String(title)),
                ("body", CanonicalValue::String(body)),
                ("tags", tags),
            ]),
        });
    }
    engine.execute_batch(&ops).unwrap();

    let (txn, schema) = engine.begin_read_pinned().unwrap();
    let idx = schema.indexes.iter().find(|i| i.name == "search_text").unwrap();
    let db = engine.gate().open_subdb_ro(&txn, &idx.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();

    for (q, require_all, top_k) in [
        ("common rust", false, 10),
        ("common rust", true, 5),
        ("common alpha beta", false, 8),
        ("THR-42", true, 5),
        ("common", false, 3),
    ] {
        let (bmw_hits, stats) = text::search_bmw(&txn, db, q, None, top_k, require_all, None).unwrap();
        let ref_hits = text::search_exhaustive_reference(&txn, db, q, None, top_k, require_all, None).unwrap();
        assert_eq!(bmw_hits.len(), ref_hits.len(), "query '{q}' require_all={require_all}");
        for (a, b) in bmw_hits.iter().zip(ref_hits.iter()) {
            assert_eq!(a.key, b.key, "top-k key mismatch for '{q}'");
            assert!((a.score - b.score).abs() < 1e-11, "score mismatch: {} vs {}", a.score, b.score);
        }
        if q == "common rust" && require_all {
            assert!(stats.docs_scored < 650, "expected BMW/AND to skip non-matching docs");
        }
    }

    // Verify candidate filter pushdown matches reference!
    let mut allowed = FxHashSet::default();
    for i in (0..650).step_by(7) {
        allowed.insert(engine.get_row(&format!("doc-{i}")).unwrap().unwrap().key);
    }
    let (bmw_f, _) = text::search_bmw(&txn, db, "common alpha", false_or_none(), 10, false, Some(&allowed)).unwrap();
    let ref_f = text::search_exhaustive_reference(&txn, db, "common alpha", None, 10, false, Some(&allowed)).unwrap();
    assert_eq!(bmw_f.len(), ref_f.len());
    for (a, b) in bmw_f.iter().zip(ref_f.iter()) {
        assert_eq!(a.key, b.key);
        assert!((a.score - b.score).abs() < 1e-11);
    }

    // Verify prefix expansion reads dict in O(matched) and phrase match respects ELEMENT_GAP!
    let pref = text::prefix_terms(&txn, db, "com", 10).unwrap();
    assert_eq!(pref[0].0, "common");
    assert_eq!(pref[0].1.doc_freq, 650);

    let k17 = engine.get_row("doc-17").unwrap().unwrap().key;
    assert!(text::matches_phrase(&txn, db, k17, &["rust".into(), "lmdb".into()]).unwrap());
    // "tag" + "second" spans two separate list elements separated by ELEMENT_GAP -> must NOT match!
    assert!(!text::matches_phrase(&txn, db, k17, &["tag".into(), "second".into()]).unwrap());
    assert!(text::matches_phrase(&txn, db, k17, &["first".into(), "tag".into()]).unwrap());

    assert!(reg.check(&engine, None).unwrap().is_ok());
}

fn false_or_none() -> Option<&'static str> {
    None
}

#[test]
fn trigram_rarest_first_intersection_and_incremental_updates() {
    let (engine, reg) = open_sim_engine(BuildPolicy::DeferredWatermark);
    engine.set_schema(&full_schema()).unwrap();

    let mut ops = Vec::new();
    for i in 0..100 {
        ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("t-{i}")),
            fields: make_fields(&[(
                "title",
                CanonicalValue::String(if i == 77 {
                    "PostgreSQL Xylophone Engine".into()
                } else {
                    format!("PostgreSQL Database Note {i}")
                }),
            )]),
        });
    }
    engine.execute_batch(&ops).unwrap();

    let (txn, schema) = engine.begin_read_pinned().unwrap();
    let idx = schema.indexes.iter().find(|i| i.name == "title_trigram").unwrap();
    let db = engine.gate().open_subdb_ro(&txn, &idx.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();

    assert_eq!(trigram::trigram_df(&txn, db, "pos").unwrap(), 100);
    assert_eq!(trigram::trigram_df(&txn, db, "xyl").unwrap(), 1);

    let required = trigram::extract_trigrams("PostgreSQL Xylophone");
    let hits = trigram::intersect_trigrams(&txn, db, &required).unwrap();
    let k77 = engine.get_row("t-77").unwrap().unwrap().key;
    assert_eq!(hits, vec![k77]);

    // Missing trigram exits in O(1).
    let none = trigram::intersect_trigrams(&txn, db, &["zzz".into(), "pos".into()]).unwrap();
    assert!(none.is_empty());

    drop(txn);
    // Patch t-77 to remove "Xylophone" and verify df drops to 0 and check() stays clean!
    engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("t-77".into()),
            fields: make_fields(&[("title", CanonicalValue::String("Plain Updated Title".into()))]),
        }])
        .unwrap();
    {
        let (txn2, schema2) = engine.begin_read_pinned().unwrap();
        let idx2 = schema2.indexes.iter().find(|i| i.name == "title_trigram").unwrap();
        let db2 = engine.gate().open_subdb_ro(&txn2, &idx2.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        assert_eq!(trigram::trigram_df(&txn2, db2, "xyl").unwrap(), 0);
    }
    assert!(reg.check(&engine, None).unwrap().is_ok());
}

#[test]
fn watermark_deferred_build_with_interleaved_foreground_writes_matches_rebuild() {
    let (engine, reg) = open_sim_engine(BuildPolicy::DeferredWatermark);

    // Start with schema WITHOUT any indexes, populate 120 entities!
    let mut base_schema = full_schema();
    base_schema.types.get_mut("Note").unwrap().indexes.clear();
    engine.set_schema(&base_schema).unwrap();

    let mut init_ops = Vec::new();
    for i in 0..120 {
        init_ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("w-{i}")),
            fields: make_fields(&[
                ("code", CanonicalValue::String(format!("C-{i}"))),
                ("score", CanonicalValue::Int(i % 7)),
                ("title", CanonicalValue::String(format!("Title {i} alpha"))),
                ("body", CanonicalValue::String(format!("Body text item {i} common"))),
            ]),
        });
    }
    engine.execute_batch(&init_ops).unwrap();

    // Now add ALL 5 indexes via set_schema -> all start in `BuildState::Building { 0, hw }`!
    engine.set_schema(&full_schema()).unwrap();
    let statuses = reg.index_status(&engine).unwrap();
    assert_eq!(statuses.len(), 5);
    assert!(statuses.iter().all(|s| matches!(s.state, BuildState::Building { .. })));

    // Interleave small build chunks (19 entities/chunk) with foreground mutations across ALL watermark zones:
    // - key <= built_up_to (already built)
    // - key in (built_up_to, high_water] (not yet built!)
    // - key > high_water (newly created after schema change!)
    let mut step = 0usize;
    while reg.drive_build_chunk(&engine, 19).unwrap() {
        step += 1;
        // Patch an early entity (<= built_up_to), patch a late entity (> built_up_to & <= hw),
        // trash one, restore one, and create a brand-new entity (> high_water)!
        let mut live_ops = vec![
            WireOp::Patch {
                target: EntityTarget::ExternalId("w-2".into()),
                fields: make_fields(&[
                    ("score", CanonicalValue::Int(100 + step as i64)),
                    ("title", CanonicalValue::String(format!("Updated Early {step}"))),
                ]),
            },
            WireOp::Patch {
                target: EntityTarget::ExternalId("w-115".into()),
                fields: make_fields(&[
                    ("score", CanonicalValue::Int(200 + step as i64)),
                    ("body", CanonicalValue::String(format!("Updated Late {step} xylophone"))),
                ]),
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some(format!("new-{step}")),
                fields: make_fields(&[
                    ("code", CanonicalValue::String(format!("NEW-{step}"))),
                    ("score", CanonicalValue::Int(500 + step as i64)),
                    ("title", CanonicalValue::String(format!("Fresh {step} omega"))),
                ]),
            },
        ];
        if step == 2 {
            live_ops.push(WireOp::Trash { target: EntityTarget::ExternalId("w-50".into()) });
            live_ops.push(WireOp::Destroy { target: EntityTarget::ExternalId("w-10".into()) });
        }
        if step == 4 {
            live_ops.push(WireOp::Restore { target: EntityTarget::ExternalId("w-50".into()) });
        }
        engine.execute_batch(&live_ops).unwrap();
    }

    // All indexes must now be Ready and pass byte-for-byte `check()`!
    let final_statuses = reg.index_status(&engine).unwrap();
    assert!(final_statuses.iter().all(|s| s.state == BuildState::Ready));
    let report = reg.check(&engine, None).unwrap();
    assert!(report.is_ok(), "watermark build drift: {:?}", report.mismatches);
    assert_eq!(report.checked_indexes, 5);

    // Verify `stats()` reports positive entries for every index.
    let stats = reg.stats(&engine).unwrap();
    assert_eq!(stats.len(), 5);
    assert!(stats.iter().all(|s| s.entry_count > 0));
}

#[test]
fn failed_unique_build_transitions_to_failed_and_recovers_on_rebuild() {
    let (engine, reg) = open_sim_engine(BuildPolicy::DeferredWatermark);

    // Create duplicate codes BEFORE unique index exists.
    let mut base = full_schema();
    base.types.get_mut("Note").unwrap().indexes.clear();
    engine.set_schema(&base).unwrap();
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("d-1".into()),
                fields: make_fields(&[("code", CanonicalValue::String("DUP".into()))]),
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("d-2".into()),
                fields: make_fields(&[("code", CanonicalValue::String("DUP".into()))]),
            },
        ])
        .unwrap();

    // Add unique index and drain builds -> must transition cleanly to Failed(UniqueViolation).
    engine.set_schema(&full_schema()).unwrap();
    reg.drain_all_builds(&engine).unwrap();
    let status = reg.index_status(&engine).unwrap().into_iter().find(|s| s.name == "by_code_unique").unwrap();
    assert!(matches!(status.state, BuildState::Failed { code: ErrorCode::UniqueViolation, .. }));

    // Fix duplicate and rebuild -> Ready and check() clean!
    engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("d-2".into()),
            fields: make_fields(&[("code", CanonicalValue::String("UNIQUE-2".into()))]),
        }])
        .unwrap();
    reg.rebuild(&engine, Some(status.index_id)).unwrap();
    let after = reg.index_status(&engine).unwrap().into_iter().find(|s| s.name == "by_code_unique").unwrap();
    assert_eq!(after.state, BuildState::Ready);
    assert!(reg.check(&engine, None).unwrap().is_ok());
}

#[test]
fn live_lmdb_projections_and_600_index_churn_never_hits_mdb_dbs_full() {
    let dir = tempfile::tempdir().unwrap();
    let store = LmdbStore::open(dir.path(), LmdbConfig::default()).unwrap();
    let reg = Arc::new(ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange));
    let (engine, _) = Engine::open_with_projections(
        store,
        OsVfs::new(),
        SystemClock::new(),
        dir.path(),
        DurabilityMode::Strict,
        Arc::clone(&reg) as _,
    )
    .unwrap();

    engine.set_schema(&full_schema()).unwrap();
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("l-1".into()),
                fields: make_fields(&[
                    ("code", CanonicalValue::String("L1".into())),
                    ("score", CanonicalValue::Float(9.5)),
                    ("title", CanonicalValue::String("LMDB Projections Live Test".into())),
                    ("body", CanonicalValue::String("Verifying pooled sub-DB reuse without DBI leaks".into())),
                ]),
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("l-2".into()),
                fields: make_fields(&[
                    ("code", CanonicalValue::String("L2".into())),
                    ("score", CanonicalValue::Int(9)),
                    ("title", CanonicalValue::String("Another Note".into())),
                ]),
            },
        ])
        .unwrap();
    assert!(reg.check(&engine, None).unwrap().is_ok());

    // Churn index definitions 550 times (> LMDB's 512 max_dbs!) to prove SubDbPool recycling + O(1) truncate
    // prevents MDB_DBS_FULL forever!
    for round in 0..550 {
        let mut s = full_schema();
        s.types.get_mut("Note").unwrap().indexes.clear();
        s.types.get_mut("Note").unwrap().indexes.insert(
            format!("churn_{round}"),
            RawIndex { kind: RawIndexKind::Value { fields: vec!["score".into()], unique: false } },
        );
        engine.set_schema(&s).unwrap();
    }
    let rep = reg.check(&engine, None).unwrap();
    assert!(rep.is_ok(), "mismatches after 550 index churns: {:?}", rep.mismatches);

    // Verify lookup still finds l-2 (score == 9) on live LMDB!
    let (txn, schema) = engine.begin_read_pinned().unwrap();
    let idx = &schema.indexes[0];
    let db = engine.gate().open_subdb_ro(&txn, &idx.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
    let keys = value::lookup_eq(&txn, db, &[&CanonicalValue::Float(9.0)]).unwrap();
    assert_eq!(keys, vec![EntityKey(2)]);

    // Regression: NUL-rich prefix > 503 escaped bytes on live LMDB must return empty cleanly (no MDB_BAD_VALSIZE!).
    let nul_prefix = "\0".repeat(300);
    let pref = value::scan_prefix_string(&txn, db, &nul_prefix, None).unwrap();
    assert!(pref.is_empty());
}

#[test]
fn review_regression_bmw_or_three_term_no_false_pruning() {
    // Reproduces P0 #1: Doc in bucket 0 fills top_k=1 heap; in bucket 1 (keys 128..255),
    // term A & term B have low block_ub in bucket 1 while term C enters later in bucket 1
    // at doc 150 where B + C strictly beats doc 1!
    let (engine, reg) = open_sim_engine(BuildPolicy::EagerInSchemaChange);
    engine.set_schema(&full_schema()).unwrap();

    let mut ops = Vec::with_capacity(200);
    // EntityKey(1): "alpha" + "beta" in body so top_k=1 threshold is moderate after doc 1.
    ops.push(WireOp::Create {
        type_name: "Note".into(),
        external_id: Some("doc-1".into()),
        fields: make_fields(&[
            ("title", CanonicalValue::String("alpha alpha beta".into())),
            ("body", CanonicalValue::String("alpha beta pad pad pad".into())),
        ]),
    });
    // Fill bucket 0 (keys 2..129) with docs containing "alpha" or "beta" so "gamma" is much rarer (higher IDF)!
    for i in 2..=129 {
        let text = if i % 2 == 0 { "alpha filler padding text" } else { "beta filler padding text" };
        ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("pad-{i}")),
            fields: make_fields(&[("body", CanonicalValue::String(text.into()))]),
        });
    }
    // EntityKey(130): low-tf "alpha" in long doc -> low block_ub in bucket 1.
    ops.push(WireOp::Create {
        type_name: "Note".into(),
        external_id: Some("doc-130".into()),
        fields: make_fields(&[("body", CanonicalValue::String(format!("alpha {}", "pad ".repeat(60))))]),
    });
    for i in 131..140 {
        ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("pad-{i}")),
            fields: make_fields(&[("body", CanonicalValue::String("filler".into()))]),
        });
    }
    // EntityKey(140): low-tf "beta" in long doc -> low block_ub in bucket 1.
    ops.push(WireOp::Create {
        type_name: "Note".into(),
        external_id: Some("doc-140".into()),
        fields: make_fields(&[("body", CanonicalValue::String(format!("beta {}", "pad ".repeat(60))))]),
    });
    for i in 141..150 {
        ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("pad-{i}")),
            fields: make_fields(&[("body", CanonicalValue::String("filler".into()))]),
        });
    }
    // EntityKey(150): rare "gamma" (very high IDF!) + "beta" in short title -> MUST WIN top_k=1!
    ops.push(WireOp::Create {
        type_name: "Note".into(),
        external_id: Some("doc-150".into()),
        fields: make_fields(&[("title", CanonicalValue::String("beta gamma gamma gamma gamma".into()))]),
    });
    engine.execute_batch(&ops).unwrap();

    let (txn, schema) = engine.begin_read_pinned().unwrap();
    let idx = schema.indexes.iter().find(|i| i.name == "search_text").unwrap();
    let db = engine.gate().open_subdb_ro(&txn, &idx.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();

    for top_k in [1, 2, 3, 5] {
        let (bmw, _) = text::search_bmw(&txn, db, "alpha beta gamma", None, top_k, false, None).unwrap();
        let ref_hits =
            text::search_exhaustive_reference(&txn, db, "alpha beta gamma", None, top_k, false, None).unwrap();
        assert_eq!(bmw.len(), ref_hits.len(), "top_k={top_k}");
        for (a, b) in bmw.iter().zip(ref_hits.iter()) {
            assert_eq!(a.key, b.key, "BMW OR false pruning at top_k={top_k}!");
            assert!((a.score - b.score).abs() < 1e-11);
        }
        assert_eq!(bmw[0].key, EntityKey(150));
    }

    // Also verify 1-term matches_phrase checks term presence (P2 #6).
    assert!(text::matches_phrase(&txn, db, EntityKey(150), &["gamma".into()]).unwrap());
    assert!(!text::matches_phrase(&txn, db, EntityKey(150), &["nonexistent".into()]).unwrap());
    assert!(reg.check(&engine, None).unwrap().is_ok());
}

#[test]
fn review_regression_compound_unique_long_plus_null_and_prefix_range_and_stamp_mismatch() {
    let (engine, reg) = open_sim_engine(BuildPolicy::EagerInSchemaChange);
    let mut schema = full_schema();
    // Drop single-field `by_code_unique` so we isolate compound unique `(code, title)`.
    schema.types.get_mut("Note").unwrap().indexes.remove("by_code_unique");
    schema.types.get_mut("Note").unwrap().indexes.insert(
        "compound_u".into(),
        RawIndex { kind: RawIndexKind::Value { fields: vec!["code".into(), "title".into()], unique: true } },
    );
    engine.set_schema(&schema).unwrap();

    // P1 #4: Two rows with (Long string, Null) must BOTH classify as Without and NOT trigger UniqueViolation!
    let long = "L".repeat(500);
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("ln-1".into()),
                fields: make_fields(&[("code", CanonicalValue::String(long.clone())), ("title", CanonicalValue::Null)]),
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("ln-2".into()),
                fields: make_fields(&[("code", CanonicalValue::String(long)), ("title", CanonicalValue::Null)]),
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("cp-1".into()),
                fields: make_fields(&[
                    ("code", CanonicalValue::String("G1".into())),
                    ("title", CanonicalValue::String("A".into())),
                    ("score", CanonicalValue::Int(10)),
                ]),
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("cp-2".into()),
                fields: make_fields(&[
                    ("code", CanonicalValue::String("G1".into())),
                    ("title", CanonicalValue::String("B".into())),
                    ("score", CanonicalValue::String("str-should-not-bleed".into())),
                ]),
            },
        ])
        .unwrap();

    {
        let (txn, compiled) = engine.begin_read_pinned().unwrap();
        // P1 #5a: Compound prefix range scan on ("G1") inclusive returns both ("G1","A") and ("G1","B")!
        let cidx = compiled.indexes.iter().find(|i| i.name == "compound_u").unwrap();
        let cdb = engine.gate().open_subdb_ro(&txn, &cidx.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        let g1_fwd = value::scan_range(
            &txn,
            cdb,
            Some((&[&CanonicalValue::String("G1".into())], true)),
            Some((&[&CanonicalValue::String("G1".into())], true)),
            false,
            None,
        )
        .unwrap();
        assert_eq!(g1_fwd.len(), 2);
        let g1_rev = value::scan_range(
            &txn,
            cdb,
            Some((&[&CanonicalValue::String("G1".into())], true)),
            Some((&[&CanonicalValue::String("G1".into())], true)),
            true,
            None,
        )
        .unwrap();
        assert_eq!(g1_rev.len(), 2);
        assert_eq!(g1_rev[0], g1_fwd[1]);

        // P1 #5b: Open-ended numeric scan_range on Any field `score >= 0` must NOT bleed into String keys!
        let sidx = compiled.indexes.iter().find(|i| i.name == "by_score").unwrap();
        let sdb = engine.gate().open_subdb_ro(&txn, &sidx.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        let num_only =
            value::scan_range(&txn, sdb, Some((&[&CanonicalValue::Int(0)], true)), None, false, None).unwrap();
        assert_eq!(num_only.len(), 1);
        assert_eq!(engine.resolve_ext_id(num_only[0]).unwrap().as_deref(), Some("cp-1"));
    }

    // P2 #2: check() with unknown IndexId returns UnknownIndex!
    assert_eq!(reg.check(&engine, Some(elmdb_engine::IndexId(9999))).unwrap_err().code, ErrorCode::UnknownIndex);

    // P1 #7: Corrupt/mismatch collator stamp in Order index -> both read and write reject with IndexNotReady!
    let order_idx = engine.schema().indexes.iter().find(|i| i.name == "title_order_sv").unwrap().clone();
    engine
        .run_maintenance_write(|txn, _schema, _dbs| {
            let db = txn.open_subdb(&order_idx.subdb_slot().name(), DbFlags::DEFAULT)?;
            txn.put(db, &[0x00], &[0xFF; 32])?;
            Ok(())
        })
        .unwrap();
    {
        let (txn, _) = engine.begin_read_pinned().unwrap();
        let IndexKind::Order { field, locale } = &order_idx.kind else { panic!() };
        let db = engine.gate().open_subdb_ro(&txn, &order_idx.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        let cached = reg.collators().get_or_build(locale).unwrap();
        let err = order::scan_ordered(&txn, &engine.dbs(), db, &cached, *field, false, None).unwrap_err();
        assert_eq!(err.code, ErrorCode::IndexNotReady);
    }
    let write_err = engine
        .execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("post-stamp-mismatch".into()),
            fields: make_fields(&[("title", CanonicalValue::String("Fail".into()))]),
        }])
        .unwrap_err();
    assert_eq!(write_err.code, ErrorCode::IndexNotReady);

    // Rebuilding restores the stamp and check() passes!
    reg.rebuild(&engine, Some(order_idx.id)).unwrap();
    assert!(reg.check(&engine, None).unwrap().is_ok());
}

#[test]
fn prefix_bmw_matches_exhaustive_oracle_and_prunes_blocks() {
    let (engine, _reg) = open_sim_engine(BuildPolicy::EagerInSchemaChange);
    engine.set_schema(&full_schema()).unwrap();

    // Populate 320 docs across 3 posting blocks (128 entities per bucket) with varying TF & prefixes.
    // Docs 1..=10 (bucket 0) have high TF and minimal length so their BM25F score sets a high top-5
    // threshold; buckets 1 & 2 (ids > 128) have long bodies and single prefix occurrences so BMW
    // soundly prunes later posting blocks while preserving exact top-K equivalence.
    let long_padding = "padding ".repeat(64);
    let mut ops = Vec::with_capacity(320);
    for i in 1u64..=320 {
        let (title, body) = match i {
            1..=10 => ("auth auth auth auth auth token token token token token".to_owned(), String::new()),
            _ => {
                let t = match i % 10 {
                    0 => "authentication token authorization".to_owned(),
                    1 => "authenticator tokenizer".to_owned(),
                    2 => "authoring toolkit".to_owned(),
                    3 => "authentication only".to_owned(),
                    4 => "tokenization only".to_owned(),
                    _ => format!("filler word{i} doc{i}"),
                };
                (t, format!("{long_padding} document {i}"))
            }
        };
        ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("p-{i}")),
            fields: make_fields(&[("title", CanonicalValue::String(title)), ("body", CanonicalValue::String(body))]),
        });
    }
    engine.execute_batch(&ops).unwrap();

    let (txn, schema) = engine.begin_read_pinned().unwrap();
    let fts_idx = schema.indexes.iter().find(|i| matches!(i.kind, IndexKind::Text { .. })).unwrap();
    let db = engine.gate().open_subdb_ro(&txn, &fts_idx.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();

    for require_all in [false, true] {
        // Uncapped top_k = 1000 gives full reference ordering without BMW pruning.
        let (full_hits, _) =
            text::search_bmw_with_prefix(&txn, db, "auth tok", None, 1000, require_all, true, None).unwrap();
        let top_k = 5;
        let (bmw_hits, bmw_stats) =
            text::search_bmw_with_prefix(&txn, db, "auth tok", None, top_k, require_all, true, None).unwrap();

        assert_eq!(bmw_hits.len(), top_k);
        assert_eq!(bmw_hits, full_hits[..top_k], "BMW top-K must exactly match uncapped prefix search");
        assert!(
            bmw_stats.blocks_pruned > 0,
            "expected BMW block pruning across 3 buckets for prefix search (require_all={require_all})"
        );
    }
}

#[test]
fn batch_c_build_chunk_propagates_map_full_and_io_without_poisoning_build_state() {
    use std::sync::atomic::{AtomicU8, Ordering};

    use elmdb_engine::{EdgeDelta, EntityDelta, IndexDef, SystemDbs, max_type_key};
    use elmdb_projections::{ExternalProjection, IndexMismatch};
    use elmdb_storage::{DurabilityMode, GatedWriteTxn, StorageError, SubDbHandle};
    use elmdb_wire::{ElmdbError, ErrorCode, Result};

    // 1. Deterministic `UniqueViolation` during deferred build MUST mark the index `BuildState::Failed`.
    {
        let (engine, reg) = open_sim_engine(BuildPolicy::DeferredWatermark);
        let mut base = RawSchema::default();
        let mut fields = BTreeMap::new();
        fields.insert("code".into(), RawField { kind: RawFieldKind::String, optional: false, nullable: false });
        base.types.insert(
            "Item".into(),
            RawType {
                fields,
                indexes: BTreeMap::new(),
                additional_fields: false,
                short_code: None,
                change_order: false,
                declared_by: None,
            },
        );
        engine.set_schema(&base).unwrap();
        engine
            .execute_batch(&[
                WireOp::Create {
                    type_name: "Item".into(),
                    external_id: Some("i1".into()),
                    fields: make_fields(&[("code", CanonicalValue::String("DUP".into()))]),
                },
                WireOp::Create {
                    type_name: "Item".into(),
                    external_id: Some("i2".into()),
                    fields: make_fields(&[("code", CanonicalValue::String("DUP".into()))]),
                },
            ])
            .unwrap();

        let mut with_uniq = base.clone();
        with_uniq.types.get_mut("Item").unwrap().indexes.insert(
            "uniq_code".into(),
            RawIndex { kind: RawIndexKind::Value { fields: vec!["code".into()], unique: true } },
        );
        engine.set_schema(&with_uniq).unwrap();
        assert!(reg.drive_build_chunk(&engine, 16).unwrap());
        let statuses = reg.index_status(&engine).unwrap();
        assert!(matches!(statuses[0].state, BuildState::Failed { code: ErrorCode::UniqueViolation, .. }));
    }

    // 2. Non-deterministic `StorageError::MapFull` (mode=1) MUST propagate to `retry_on_map_full()`,
    //    auto-growing `map_size` and completing cleanly as `BuildState::Ready` (NEVER `BuildState::Failed`),
    //    and non-deterministic `ErrorCode::Io` (mode=2) MUST propagate out leaving `BuildState::Building`!
    struct FlakyExternal {
        // 0 = ok, 1 = emit MapFull once then flip to 0, 2 = emit Io
        mode: Arc<AtomicU8>,
    }
    impl ExternalProjection<SimKvStore, SimVfs> for FlakyExternal {
        fn handles(&self, kind: &IndexKind) -> bool {
            matches!(kind, IndexKind::Vector { .. })
        }
        fn apply_entity_delta(
            &self,
            _txn: &mut GatedWriteTxn<'_, SimKvStore, SimVfs>,
            _schema: &elmdb_engine::CompiledSchema,
            _dbs: &SystemDbs,
            _def: &IndexDef,
            _db: SubDbHandle,
            _delta: &EntityDelta,
        ) -> Result<()> {
            match self.mode.load(Ordering::SeqCst) {
                1 => {
                    self.mode.store(0, Ordering::SeqCst);
                    Err(ElmdbError::from(StorageError::MapFull { map_size: 4096 }))
                }
                2 => Err(ElmdbError::new(ErrorCode::Io, "transient disk read timeout")),
                _ => Ok(()),
            }
        }
        fn apply_edge_delta(
            &self,
            _txn: &mut GatedWriteTxn<'_, SimKvStore, SimVfs>,
            _schema: &elmdb_engine::CompiledSchema,
            _dbs: &SystemDbs,
            _def: &IndexDef,
            _db: SubDbHandle,
            _delta: &EdgeDelta,
        ) -> Result<()> {
            Ok(())
        }
        fn on_index_added(
            &self,
            txn: &mut GatedWriteTxn<'_, SimKvStore, SimVfs>,
            _schema: &elmdb_engine::CompiledSchema,
            dbs: &SystemDbs,
            def: &IndexDef,
            _db: SubDbHandle,
        ) -> Result<BuildState> {
            match max_type_key(txn, dbs, def.type_id)? {
                Some(hw) => Ok(BuildState::Building { built_up_to: EntityKey(0), high_water: hw }),
                None => Ok(BuildState::Ready),
            }
        }
        fn rebuild_index(
            &self,
            _txn: &mut GatedWriteTxn<'_, SimKvStore, SimVfs>,
            _schema: &elmdb_engine::CompiledSchema,
            _dbs: &SystemDbs,
            _def: &IndexDef,
            _db: SubDbHandle,
        ) -> Result<()> {
            Ok(())
        }
        fn check_index(
            &self,
            _txn: &<SimKvStore as elmdb_storage::KvStore>::RoTxn<'_>,
            _schema: &elmdb_engine::CompiledSchema,
            _dbs: &SystemDbs,
            _def: &IndexDef,
            _db: Option<SubDbHandle>,
        ) -> Result<Vec<IndexMismatch>> {
            Ok(Vec::new())
        }
    }

    {
        let mode = Arc::new(AtomicU8::new(2));
        let store = SimKvStore::default();
        let vfs = SimVfs::default();
        let clock = SimClock::new(1000);
        let reg = Arc::new(
            ProjectionRegistry::new(BuildPolicy::DeferredWatermark)
                .with_external(Arc::new(FlakyExternal { mode: Arc::clone(&mode) })),
        );
        let (engine, _) = Engine::open_with_projections(
            store,
            vfs,
            clock,
            "/sim/flaky_io_build",
            DurabilityMode::Strict,
            Arc::clone(&reg) as _,
        )
        .unwrap();

        let mut base = RawSchema::default();
        let mut fields = BTreeMap::new();
        fields.insert("body".into(), RawField { kind: RawFieldKind::String, optional: false, nullable: false });
        base.types.insert(
            "Note".into(),
            RawType {
                fields,
                indexes: BTreeMap::new(),
                additional_fields: false,
                short_code: None,
                change_order: false,
                declared_by: None,
            },
        );
        engine.set_schema(&base).unwrap();
        engine
            .execute_batch(&[WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n1".into()),
                fields: make_fields(&[("body", CanonicalValue::String("hello".into()))]),
            }])
            .unwrap();

        let mut with_vec = base.clone();
        with_vec.types.get_mut("Note").unwrap().indexes.insert(
            "vec_idx".into(),
            RawIndex {
                kind: RawIndexKind::Vector { fields: vec!["body".into()], dims: 4, quantization: "sq8".into() },
            },
        );
        engine.set_schema(&with_vec).unwrap();

        // Transient Io propagates and leaves BuildState::Building (never Failed).
        let err = reg.drive_build_chunk(&engine, 16).unwrap_err();
        assert_eq!(err.code, ErrorCode::Io);
        let statuses = reg.index_status(&engine).unwrap();
        assert!(
            matches!(statuses[0].state, BuildState::Building { .. }),
            "transient Io error must leave index Building, not Failed: {:?}",
            statuses[0].state
        );

        // MapFull propagates to `retry_on_map_full()`, auto-grows `map_size`, and reaches `BuildState::Ready`!
        let map_before = engine.gate().map_size();
        // Verify `grow_map(Some(1))` never shrinks and `grow_map(None)` doubles cleanly.
        assert_eq!(engine.gate().grow_map(Some(1)).unwrap(), map_before);
        let doubled = engine.gate().grow_map(None).unwrap();
        assert_eq!(doubled, map_before * 2);
        mode.store(1, Ordering::SeqCst);
        reg.drain_all_builds(&engine).unwrap();
        assert!(engine.gate().map_size() > doubled, "MapFull during build chunk must trigger map auto-growth");
        let statuses_after = reg.index_status(&engine).unwrap();
        assert_eq!(statuses_after[0].state, BuildState::Ready);
    }
}
