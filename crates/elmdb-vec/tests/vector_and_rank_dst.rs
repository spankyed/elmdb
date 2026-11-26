use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use elmdb_engine::{
    Cardinality, Engine, EntityKey, RawEndpoint, RawField, RawFieldKind, RawIndex, RawIndexKind, RawRelation,
    RawSchema, RawType,
};
use elmdb_lmdb::{LmdbConfig, LmdbStore};
use elmdb_projections::{BuildPolicy, ProjectionRegistry, rank};
use elmdb_sim::{SimClock, SimKvConfig, SimKvStore, SimRng, SimVfs};
use elmdb_storage::{BootId, DbFlags, DurabilityMode, OsVfs, SystemClock};
use elmdb_vec::{
    DEFAULT_DRAIN_BATCH, SearchNearOpts, VectorProjection, compact_now, cosine_similarity_f32, drain_pending_batch,
    flush, search_like, search_near, set_entity_embeddings, stats,
};
use elmdb_wire::{CanonicalValue, EntityTarget, WireOp};
use rustc_hash::FxHashSet;

fn vec_and_rank_schema(quant: &str, dims: u32) -> RawSchema {
    let mut fields = BTreeMap::new();
    fields.insert("title".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });

    let mut indexes = BTreeMap::new();
    indexes.insert(
        "emb".into(),
        RawIndex { kind: RawIndexKind::Vector { fields: vec!["title".into()], dims, quantization: quant.into() } },
    );
    indexes.insert(
        "pagerank".into(),
        RawIndex { kind: RawIndexKind::Rank { relations: vec!["CITES".into()], weighted: true } },
    );

    let mut types = BTreeMap::new();
    types.insert("Doc".into(), RawType { fields, indexes, ..Default::default() });

    let mut relations = BTreeMap::new();
    relations.insert(
        "CITES".into(),
        RawRelation {
            cardinality: Cardinality::ManyToMany,
            acyclic: false,
            weighted: true,
            endpoints: vec![RawEndpoint { source: "Doc".into(), target: "Doc".into() }],
            declared_by: None,
        },
    );

    RawSchema { types, relations }
}

fn open_sim(
    policy: BuildPolicy,
) -> (Engine<SimKvStore, SimVfs, SimClock>, Arc<ProjectionRegistry<SimKvStore, SimVfs>>) {
    let store = SimKvStore::new(SimKvConfig::default());
    let vfs = SimVfs::new(BootId([1; 16]));
    let clock = SimClock::new(1_700_000_000_000);
    let reg = Arc::new(ProjectionRegistry::new(policy).with_external(Arc::new(VectorProjection)));
    let (engine, _) = Engine::open_with_projections(
        store,
        vfs,
        clock,
        PathBuf::from("/sim/vec"),
        DurabilityMode::Strict,
        Arc::clone(&reg) as _,
    )
    .unwrap();
    (engine, reg)
}

fn rand_unit_vec(rng: &mut SimRng, dims: usize) -> Vec<f32> {
    let mut v = Vec::with_capacity(dims);
    let mut norm_sq = 0.0f32;
    for _ in 0..dims {
        // Uniform in [-1, 1]
        let u = ((rng.next_u64() >> 40) as f32 / ((1u32 << 24) as f32)) * 2.0 - 1.0;
        norm_sq += u * u;
        v.push(u);
    }
    let norm = norm_sq.sqrt().max(1e-6);
    for x in &mut v {
        *x /= norm;
    }
    v
}

#[test]
fn hnsw_sq8_f16_f32_read_your_writes_bounded_drain_and_recall() {
    for quant in ["sq8", "f16", "f32"] {
        let dims = 32u32;
        let (engine, reg) = open_sim(BuildPolicy::EagerInSchemaChange);
        engine.set_schema(&vec_and_rank_schema(quant, dims)).unwrap();
        let idx_id = engine.schema().indexes.iter().find(|i| i.name == "emb").unwrap().id;

        let mut rng = SimRng::seeded(0xCAFE_BABE_0000_0001);
        let n = 220usize;
        let mut ops = Vec::with_capacity(n);
        for i in 0..n {
            ops.push(WireOp::Create {
                type_name: "Doc".into(),
                external_id: Some(format!("doc-{i}")),
                fields: vec![("title".into(), CanonicalValue::String(format!("Doc {i}")))],
            });
        }
        engine.execute_batch(&ops).unwrap();

        let mut corpus: Vec<(EntityKey, Vec<f32>)> = Vec::with_capacity(n);
        for i in 0..n {
            let key = engine.get_row(&format!("doc-{i}")).unwrap().unwrap().key;
            let vec = rand_unit_vec(&mut rng, dims as usize);
            set_entity_embeddings(&engine, idx_id, key, std::slice::from_ref(&vec)).unwrap();
            corpus.push((key, vec));
        }

        // Before draining: all 220 live in `vec_pending` -> exact read-your-writes!
        let st_before = stats(&engine, idx_id).unwrap();
        assert_eq!(st_before.pending_count, n as u64);
        assert_eq!(st_before.live_nodes, 0);

        let query = rand_unit_vec(&mut rng, dims as usize);
        let mut exact: Vec<(EntityKey, f32)> =
            corpus.iter().map(|(k, v)| (*k, cosine_similarity_f32(&query, v))).collect();
        exact.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let exact_top10: Vec<EntityKey> = exact.iter().take(10).map(|(k, _)| *k).collect();

        {
            let (txn, schema) = engine.begin_read_pinned().unwrap();
            let def = schema.index_by_id(idx_id).unwrap();
            let db = engine.gate().open_subdb_ro(&txn, &def.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
            let pre_drain = search_near(&txn, &engine.dbs(), def, db, &query, SearchNearOpts::new(10)).unwrap();
            let pre_keys: Vec<EntityKey> = pre_drain.iter().map(|h| h.key).collect();
            assert_eq!(pre_keys, exact_top10, "pre-drain read-your-writes must be 100% exact ({quant})");
        }

        // Drain in bounded batches of 48 (never starving interactive writes!).
        let first_batch = drain_pending_batch(&engine, idx_id, DEFAULT_DRAIN_BATCH).unwrap();
        assert_eq!(first_batch, DEFAULT_DRAIN_BATCH);
        assert!(reg.check(&engine, Some(idx_id)).unwrap().is_ok());

        // Drain remainder via `flush()`.
        let rest = flush(&engine, idx_id).unwrap();
        assert_eq!(first_batch + rest, n);
        let st_after = stats(&engine, idx_id).unwrap();
        assert_eq!(st_after.pending_count, 0);
        assert_eq!(st_after.live_nodes, n as u64);
        assert!(reg.check(&engine, Some(idx_id)).unwrap().is_ok());

        // Check recall@10 >= 0.90 against brute-force F32!
        {
            let (txn, schema) = engine.begin_read_pinned().unwrap();
            let def = schema.index_by_id(idx_id).unwrap();
            let db = engine.gate().open_subdb_ro(&txn, &def.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
            let hits = search_near(&txn, &engine.dbs(), def, db, &query, SearchNearOpts::new(10)).unwrap();
            assert_eq!(hits.len(), 10);
            let hit_set: FxHashSet<EntityKey> = hits.iter().map(|h| h.key).collect();
            let overlap = exact_top10.iter().filter(|k| hit_set.contains(k)).count();
            assert!(overlap >= 9, "quant={quant} recall@10 too low: {overlap}/10");

            // Tight predicate filter (<= 2048) uses exact F32 scan!
            let allowed: FxHashSet<EntityKey> = corpus.iter().step_by(5).map(|(k, _)| *k).collect();
            let filt_hits = search_near(
                &txn,
                &engine.dbs(),
                def,
                db,
                &query,
                SearchNearOpts { top_k: 5, candidates: Some(&allowed), best_chunk_per_entity: true },
            )
            .unwrap();
            let mut filt_exact: Vec<(EntityKey, f32)> = corpus
                .iter()
                .filter(|(k, _)| allowed.contains(k))
                .map(|(k, v)| (*k, cosine_similarity_f32(&query, v)))
                .collect();
            filt_exact.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let filt_expected: Vec<EntityKey> = filt_exact.into_iter().take(5).map(|(k, _)| k).collect();
            assert_eq!(filt_hits.iter().map(|h| h.key).collect::<Vec<_>>(), filt_expected);

            // search_like excludes self!
            let like = search_like(&txn, &engine.dbs(), def, db, corpus[0].0, 5, None).unwrap();
            assert!(!like.iter().any(|h| h.key == corpus[0].0));
            assert_eq!(like.len(), 5);
        }
    }
}

#[test]
fn hnsw_generation_filter_prevents_stale_hits_on_update_trash_destroy() {
    let (engine, reg) = open_sim(BuildPolicy::EagerInSchemaChange);
    engine.set_schema(&vec_and_rank_schema("sq8", 8)).unwrap();
    let idx_id = engine.schema().indexes.iter().find(|i| i.name == "emb").unwrap().id;

    engine
        .execute_batch(&[
            WireOp::Create { type_name: "Doc".into(), external_id: Some("e1".into()), fields: vec![] },
            WireOp::Create { type_name: "Doc".into(), external_id: Some("e2".into()), fields: vec![] },
        ])
        .unwrap();
    let k1 = engine.get_row("e1").unwrap().unwrap().key;
    let k2 = engine.get_row("e2").unwrap().unwrap().key;

    let v_x = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let v_y = vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
    let v_z = vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0];

    set_entity_embeddings(&engine, idx_id, k1, std::slice::from_ref(&v_x)).unwrap();
    set_entity_embeddings(&engine, idx_id, k2, std::slice::from_ref(&v_z)).unwrap();
    flush(&engine, idx_id).unwrap();

    // Update k1 from v_x to v_y WITHOUT flushing yet!
    // Old HNSW node for k1 still sits in TAG_NODES with v_x, but pre-rerank generation filter MUST hide it!
    set_entity_embeddings(&engine, idx_id, k1, std::slice::from_ref(&v_y)).unwrap();
    {
        let (txn, schema) = engine.begin_read_pinned().unwrap();
        let def = schema.index_by_id(idx_id).unwrap();
        let db = engine.gate().open_subdb_ro(&txn, &def.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        let near_x = search_near(&txn, &engine.dbs(), def, db, &v_x, SearchNearOpts::new(5)).unwrap();
        // Neither k1 (now v_y) nor k2 (v_z) should have similarity ~1.0 to v_x!
        for h in &near_x {
            assert!(h.score < 0.1, "stale vector leaked on update! hit={h:?}");
        }
        let near_y = search_near(&txn, &engine.dbs(), def, db, &v_y, SearchNearOpts::new(1)).unwrap();
        assert_eq!(near_y[0].key, k1);
        assert!((near_y[0].score - 1.0).abs() < 1e-5);
    }

    // Flush, then trash k1 -> k1's embedding must be tombstoned and invisible immediately!
    flush(&engine, idx_id).unwrap();
    engine.execute_batch(&[WireOp::Trash { target: EntityTarget::ExternalId("e1".into()) }]).unwrap();
    {
        let (txn, schema) = engine.begin_read_pinned().unwrap();
        let def = schema.index_by_id(idx_id).unwrap();
        let db = engine.gate().open_subdb_ro(&txn, &def.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        let hits = search_near(&txn, &engine.dbs(), def, db, &v_y, SearchNearOpts::new(5)).unwrap();
        assert!(!hits.iter().any(|h| h.key == k1), "trashed entity leaked into vector hits!");
    }
    assert!(reg.check(&engine, Some(idx_id)).unwrap().is_ok());
}

#[test]
fn live_lmdb_vec_pending_survives_reopen_and_ping_pong_compaction_reclaims_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let idx_id;
    let query = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];

    // Open #1: create entities and write embeddings WITHOUT flushing!
    {
        let store = LmdbStore::open(dir.path(), LmdbConfig::default()).unwrap();
        let reg = Arc::new(
            ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange).with_external(Arc::new(VectorProjection)),
        );
        let (engine, _) = Engine::open_with_projections(
            store,
            OsVfs,
            SystemClock::new(),
            dir.path(),
            DurabilityMode::Strict,
            Arc::clone(&reg) as _,
        )
        .unwrap();
        engine.set_schema(&vec_and_rank_schema("sq8", 8)).unwrap();
        idx_id = engine.schema().indexes.iter().find(|i| i.name == "emb").unwrap().id;

        for i in 0..20 {
            engine
                .execute_batch(&[WireOp::Create {
                    type_name: "Doc".into(),
                    external_id: Some(format!("p-{i}")),
                    fields: vec![],
                }])
                .unwrap();
            let k = engine.get_row(&format!("p-{i}")).unwrap().unwrap().key;
            let mut v = vec![0.0f32; 8];
            v[i % 8] = 1.0;
            set_entity_embeddings(&engine, idx_id, k, std::slice::from_ref(&v)).unwrap();
        }
        assert_eq!(stats(&engine, idx_id).unwrap().pending_count, 20);
    }

    // Open #2: reopen from disk, verify `vec_pending` persisted, flush, overwrite to create tombstones,
    // and run ping-pong compaction!
    {
        let store = LmdbStore::open(dir.path(), LmdbConfig::default()).unwrap();
        let reg = Arc::new(
            ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange).with_external(Arc::new(VectorProjection)),
        );
        let (engine, _) = Engine::open_with_projections(
            store,
            OsVfs,
            SystemClock::new(),
            dir.path(),
            DurabilityMode::Strict,
            Arc::clone(&reg) as _,
        )
        .unwrap();
        assert_eq!(stats(&engine, idx_id).unwrap().pending_count, 20);
        flush(&engine, idx_id).unwrap();
        let st1 = stats(&engine, idx_id).unwrap();
        assert_eq!(st1.pending_count, 0);
        assert_eq!(st1.live_nodes, 20);
        assert_eq!(st1.dead_nodes, 0);

        let slot_before = engine.schema().index_by_id(idx_id).unwrap().slot;

        // Overwrite 10 entities to generate 10 dead HNSW nodes.
        for i in 0..10 {
            let k = engine.get_row(&format!("p-{i}")).unwrap().unwrap().key;
            let mut v = vec![0.0f32; 8];
            v[(i + 1) % 8] = 1.0;
            set_entity_embeddings(&engine, idx_id, k, std::slice::from_ref(&v)).unwrap();
        }
        flush(&engine, idx_id).unwrap();
        assert_eq!(stats(&engine, idx_id).unwrap().dead_nodes, 10);

        // Ping-pong compact into a fresh SubDbSlot and verify dead_nodes == 0 and active slot rotated!
        let compacted = compact_now(&engine, idx_id).unwrap();
        assert_eq!(compacted.dead_nodes, 0);
        assert_eq!(compacted.live_nodes, 20);
        let slot_after = engine.schema().index_by_id(idx_id).unwrap().slot;
        assert_ne!(slot_before, slot_after, "ping-pong compaction must swap active SubDbSlot!");

        // Compact a second time -> should recycle `slot_before` cleanly (ping-pong!) without leaking slots!
        let compacted2 = compact_now(&engine, idx_id).unwrap();
        assert_eq!(compacted2.dead_nodes, 0);
        assert_eq!(engine.schema().index_by_id(idx_id).unwrap().slot, slot_before);

        assert!(reg.check(&engine, None).unwrap().is_ok());

        let (txn, schema) = engine.begin_read_pinned().unwrap();
        let def = schema.index_by_id(idx_id).unwrap();
        let db = engine.gate().open_subdb_ro(&txn, &def.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        let hits = search_near(&txn, &engine.dbs(), def, db, &query, SearchNearOpts::new(3)).unwrap();
        assert!(!hits.is_empty());
        assert!((hits[0].score - 1.0).abs() < 1e-5);
    }
}

#[test]
fn push_pagerank_preserves_zlg_invariant_across_dynamic_mutations_and_matches_rebuild() {
    let (engine, reg) = open_sim(BuildPolicy::EagerInSchemaChange);
    engine.set_schema(&vec_and_rank_schema("sq8", 4)).unwrap();
    let rank_id = engine.schema().indexes.iter().find(|i| i.name == "pagerank").unwrap().id;

    // Create 5 docs: hub receives links from a, b, c, d; d also links to a (cycle); isolated stays dangling.
    engine
        .execute_batch(&[
            WireOp::Create { type_name: "Doc".into(), external_id: Some("hub".into()), fields: vec![] },
            WireOp::Create { type_name: "Doc".into(), external_id: Some("a".into()), fields: vec![] },
            WireOp::Create { type_name: "Doc".into(), external_id: Some("b".into()), fields: vec![] },
            WireOp::Create { type_name: "Doc".into(), external_id: Some("c".into()), fields: vec![] },
            WireOp::Create { type_name: "Doc".into(), external_id: Some("iso".into()), fields: vec![] },
            WireOp::Link {
                source: EntityTarget::ExternalId("a".into()),
                rel_kind: "CITES".into(),
                target: EntityTarget::ExternalId("hub".into()),
                weight: Some(2.0f64.to_bits()),
                info: None,
            },
            WireOp::Link {
                source: EntityTarget::ExternalId("b".into()),
                rel_kind: "CITES".into(),
                target: EntityTarget::ExternalId("hub".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
            WireOp::Link {
                source: EntityTarget::ExternalId("c".into()),
                rel_kind: "CITES".into(),
                target: EntityTarget::ExternalId("hub".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
            WireOp::Link {
                source: EntityTarget::ExternalId("hub".into()),
                rel_kind: "CITES".into(),
                target: EntityTarget::ExternalId("a".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
        ])
        .unwrap();

    // Even BEFORE draining pushes, exact ZLG residual invariant MUST hold!
    let rep0 = reg.check(&engine, Some(rank_id)).unwrap();
    assert!(rep0.is_ok(), "ZLG invariant failed before drain: {:?}", rep0.mismatches);

    // Flush pushes to convergence!
    rank::flush_rank(&engine, rank_id).unwrap();
    let rep1 = reg.check(&engine, Some(rank_id)).unwrap();
    assert!(rep1.is_ok(), "ZLG invariant failed after flush: {:?}", rep1.mismatches);

    let hub_key = engine.get_row("hub").unwrap().unwrap().key;
    {
        let (txn, schema) = engine.begin_read_pinned().unwrap();
        let def = schema.index_by_id(rank_id).unwrap();
        let db = engine.gate().open_subdb_ro(&txn, &def.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        let err_bound = rank::l1_error_bound(&txn, db).unwrap();
        assert!(err_bound < 1e-4, "L1 error bound too high after flush: {err_bound}");
        let top = rank::scan_ranked(&txn, db, false, Some(5)).unwrap();
        assert_eq!(top.len(), 5);
        assert_eq!(top[0].0, hub_key, "hub must rank #1! got {top:?}");
    }

    // Dynamic mutations: unlink b->hub, add c->b, trash iso, create d -> hub in one batch!
    engine
        .execute_batch(&[
            WireOp::Unlink {
                source: EntityTarget::ExternalId("b".into()),
                rel_kind: Some("CITES".into()),
                target: Some(EntityTarget::ExternalId("hub".into())),
            },
            WireOp::Link {
                source: EntityTarget::ExternalId("c".into()),
                rel_kind: "CITES".into(),
                target: EntityTarget::ExternalId("b".into()),
                weight: Some(3.0f64.to_bits()),
                info: None,
            },
            WireOp::Trash { target: EntityTarget::ExternalId("iso".into()) },
            WireOp::Create { type_name: "Doc".into(), external_id: Some("d".into()), fields: vec![] },
            WireOp::Link {
                source: EntityTarget::ExternalId("d".into()),
                rel_kind: "CITES".into(),
                target: EntityTarget::ExternalId("hub".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
        ])
        .unwrap();

    // ZLG invariant holds immediately after dynamic mutations (even before flush)!
    let rep2 = reg.check(&engine, Some(rank_id)).unwrap();
    assert!(rep2.is_ok(), "ZLG invariant failed after dynamic mutations: {:?}", rep2.mismatches);

    rank::flush_rank(&engine, rank_id).unwrap();
    let inc_scores: Vec<(EntityKey, f64)> = {
        let (txn, schema) = engine.begin_read_pinned().unwrap();
        let def = schema.index_by_id(rank_id).unwrap();
        let db = engine.gate().open_subdb_ro(&txn, &def.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        rank::scan_ranked(&txn, db, false, None).unwrap()
    };

    // Rebuild from scratch and assert L1 distance ||x_inc - x_rebuild||_1 < 1e-4!
    reg.rebuild(&engine, Some(rank_id)).unwrap();
    let reb_scores: Vec<(EntityKey, f64)> = {
        let (txn, schema) = engine.begin_read_pinned().unwrap();
        let def = schema.index_by_id(rank_id).unwrap();
        let db = engine.gate().open_subdb_ro(&txn, &def.subdb_slot().name(), DbFlags::DEFAULT).unwrap().unwrap();
        rank::scan_ranked(&txn, db, false, None).unwrap()
    };
    assert_eq!(inc_scores.len(), reb_scores.len());
    let inc_map: BTreeMap<EntityKey, f64> = inc_scores.into_iter().collect();
    let reb_map: BTreeMap<EntityKey, f64> = reb_scores.into_iter().collect();
    let l1_diff: f64 = inc_map.iter().map(|(k, v)| (v - reb_map[k]).abs()).sum();
    assert!(l1_diff < 1e-4, "incremental vs rebuild PageRank L1 diff too large: {l1_diff}");
}
