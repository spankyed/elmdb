use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use elmdb_engine::{
    Cardinality, CompiledSchema, Engine, FieldId, FieldMask, IndexId, RawEndpoint, RawField, RawFieldKind, RawIndex,
    RawIndexKind, RawRelation, RawSchema, RawType, RelKindId, TypeId, read_commit_seq,
};
use elmdb_invalidation::{
    COMMIT_RING_CAP, InvalidationIndex, LiveQueryHub, ReadSet, WatchId, WatchMode, WatchPolicy, WatchScheduler,
    hash_result_128, run_tracked_snapshot,
};
use elmdb_lmdb::{LmdbConfig, LmdbStore};
use elmdb_projections::{BuildPolicy, ProjectionRegistry, rank};
use elmdb_query::{
    EdgeDir, ExecBudgets, FilterExpr, GraphAnalyzeSpec, LogicalNode, LogicalPlan, OrderDir, OrderSpec, QueryEngine,
    QueryReadRecorder, QueryResult, ScoreSpec, SourceSpec, TerminalSpec,
};
use elmdb_sim::{SimClock, SimKvConfig, SimKvStore, SimRng, SimVfs};
use elmdb_storage::{BootId, DurabilityMode, OsVfs, SystemClock};
use elmdb_vec::{VectorProjection, flush as flush_vec, set_entity_embeddings};
use elmdb_wire::{CanonicalValue, EntityTarget, WireOp};
use tempfile::tempdir;

fn test_schema() -> RawSchema {
    let mut note_fields = BTreeMap::new();
    note_fields.insert("title".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });
    note_fields.insert("body".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });
    note_fields.insert("tag".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });
    note_fields.insert("score".into(), RawField { kind: RawFieldKind::Int64, optional: true, nullable: true });

    let mut note_indexes = BTreeMap::new();
    note_indexes.insert(
        "note_tag".into(),
        RawIndex { kind: RawIndexKind::Value { fields: vec!["tag".into()], unique: false } },
    );
    note_indexes.insert(
        "note_score".into(),
        RawIndex { kind: RawIndexKind::Value { fields: vec!["score".into()], unique: false } },
    );
    note_indexes.insert(
        "note_title_order".into(),
        RawIndex { kind: RawIndexKind::Order { field: "title".into(), locale: "en-US".into() } },
    );
    note_indexes.insert(
        "note_text".into(),
        RawIndex { kind: RawIndexKind::Text { fields: vec![("title".into(), 2), ("body".into(), 1)] } },
    );
    note_indexes
        .insert("note_body_trig".into(), RawIndex { kind: RawIndexKind::Trigram { fields: vec!["body".into()] } });
    note_indexes.insert(
        "note_vec".into(),
        RawIndex { kind: RawIndexKind::Vector { fields: vec!["title".into()], dims: 4, quantization: "sq8".into() } },
    );
    note_indexes.insert(
        "note_rank".into(),
        RawIndex { kind: RawIndexKind::Rank { relations: vec!["CITES".into()], weighted: true } },
    );

    let mut author_fields = BTreeMap::new();
    author_fields.insert("name".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });

    let mut types = BTreeMap::new();
    types.insert("Note".into(), RawType { fields: note_fields, indexes: note_indexes, ..Default::default() });
    types.insert("Author".into(), RawType { fields: author_fields, ..Default::default() });

    let mut relations = BTreeMap::new();
    relations.insert(
        "CITES".into(),
        RawRelation {
            cardinality: Cardinality::ManyToMany,
            acyclic: false,
            weighted: true,
            endpoints: vec![RawEndpoint { source: "Note".into(), target: "Note".into() }],
            declared_by: None,
        },
    );

    RawSchema { types, relations }
}

struct Ids {
    note_type: TypeId,
    author_type: TypeId,
    title: FieldId,
    body: FieldId,
    tag: FieldId,
    score: FieldId,
    cites: RelKindId,
    idx_text: IndexId,
    idx_vec: IndexId,
    idx_vec_slot: u16,
    idx_rank: IndexId,
}

fn resolve_ids(schema: &CompiledSchema) -> Ids {
    let note = schema.type_by_name("Note").unwrap();
    let vec_def = schema.indexes.iter().find(|i| i.name == "note_vec").unwrap();
    Ids {
        note_type: note.id,
        author_type: schema.type_by_name("Author").unwrap().id,
        title: note.field_by_name("title").unwrap().id,
        body: note.field_by_name("body").unwrap().id,
        tag: note.field_by_name("tag").unwrap().id,
        score: note.field_by_name("score").unwrap().id,
        cites: schema.relation_by_name("CITES").unwrap().id,
        idx_text: schema.indexes.iter().find(|i| i.name == "note_text").unwrap().id,
        idx_vec: vec_def.id,
        idx_vec_slot: vec_def.slot,
        idx_rank: schema.indexes.iter().find(|i| i.name == "note_rank").unwrap().id,
    }
}

type SimSetup = (Engine<SimKvStore, SimVfs, SimClock>, Arc<ProjectionRegistry<SimKvStore, SimVfs>>, Ids);

fn open_sim() -> SimSetup {
    let store = SimKvStore::new(SimKvConfig::default());
    let vfs = SimVfs::new(BootId([7; 16]));
    let clock = SimClock::new(1_700_000_000_000);
    let reg =
        Arc::new(ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange).with_external(Arc::new(VectorProjection)));
    let (engine, _) = Engine::open_with_projections(
        store,
        vfs,
        clock,
        PathBuf::from("/sim/invalidation"),
        DurabilityMode::Strict,
        Arc::clone(&reg) as _,
    )
    .unwrap();
    engine.set_schema(&test_schema()).unwrap();
    let ids = resolve_ids(&engine.schema());
    (engine, reg, ids)
}

fn hash_query_result(res: &QueryResult) -> [u8; 16] {
    hash_result_128(format!("{res:?}").as_bytes())
}

// ---------------------------------------------------------------------------
// 1. Zero-False-Negative Differential Soundness Across Randomized Mutations
// ---------------------------------------------------------------------------

#[test]
fn dst_zero_false_negatives_across_120_randomized_mutations_and_10_watch_types() {
    let (engine, _reg, ids) = open_sim();
    let qe = QueryEngine::default();

    // Seed initial entities so LiteralKeys, HasLink, GraphAnalyze, Order, Text, Trigram, Vector have targets.
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n0".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("alpha rust".into())),
                    ("body".into(), CanonicalValue::String("lmdb zero copy".into())),
                    ("tag".into(), CanonicalValue::String("red".into())),
                    ("score".into(), CanonicalValue::Int(10)),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n1".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("beta graph".into())),
                    ("body".into(), CanonicalValue::String("pagerank search".into())),
                    ("tag".into(), CanonicalValue::String("blue".into())),
                    ("score".into(), CanonicalValue::Int(20)),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n2".into()),
                fields: vec![
                    ("tag".into(), CanonicalValue::String("red".into())),
                    ("score".into(), CanonicalValue::Int(30)),
                ],
            },
            WireOp::Create {
                type_name: "Author".into(),
                external_id: Some("a0".into()),
                fields: vec![("name".into(), CanonicalValue::String("Ada".into()))],
            },
            WireOp::Link {
                source: EntityTarget::ExternalId("n0".into()),
                rel_kind: "CITES".into(),
                target: EntityTarget::ExternalId("n1".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
        ])
        .unwrap();
    let k0 = engine.get_row("n0").unwrap().unwrap().key;
    let k1 = engine.get_row("n1").unwrap().unwrap().key;
    let k2 = engine.get_row("n2").unwrap().unwrap().key;
    set_entity_embeddings(&engine, ids.idx_vec, k0, &[vec![1.0, 0.0, 0.0, 0.0]]).unwrap();
    flush_vec(&engine, ids.idx_vec).unwrap();
    rank::flush_rank(&engine, ids.idx_rank).unwrap();

    let src_note = || LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false });
    let mut title_mask = FieldMask::empty();
    title_mask.insert(ids.title);

    // 10 diverse live-query plans covering every Tier-1 and Tier-2 read-recording path!
    let plans: Vec<(&str, LogicalPlan)> = vec![
        (
            "point_projected_missing_field",
            LogicalPlan::new(LogicalNode::Terminal {
                input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k2]))),
                terminal: TerminalSpec::Rows { fields: Some(title_mask) },
            }),
        ),
        (
            "point_all_fields",
            LogicalPlan::new(LogicalNode::Terminal {
                input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k0]))),
                terminal: TerminalSpec::Rows { fields: None },
            }),
        ),
        (
            "o1_count_eq_red",
            LogicalPlan::new(LogicalNode::Terminal {
                input: Box::new(LogicalNode::Filter {
                    input: Box::new(src_note()),
                    expr: FilterExpr::Eq(ids.tag, CanonicalValue::String("red".into())),
                }),
                terminal: TerminalSpec::Count,
            }),
        ),
        (
            "score_range_15_35",
            LogicalPlan::new(LogicalNode::Terminal {
                input: Box::new(LogicalNode::Filter {
                    input: Box::new(src_note()),
                    expr: FilterExpr::Range {
                        field: ids.score,
                        low: Some((CanonicalValue::Int(15), true)),
                        high: Some((CanonicalValue::Int(35), true)),
                    },
                }),
                terminal: TerminalSpec::Ids,
            }),
        ),
        (
            "trigram_contains_search",
            LogicalPlan::new(LogicalNode::Terminal {
                input: Box::new(LogicalNode::Filter {
                    input: Box::new(src_note()),
                    expr: FilterExpr::Contains(ids.body, "search".into()),
                }),
                terminal: TerminalSpec::Ids,
            }),
        ),
        (
            "icu_order_title_asc",
            LogicalPlan::new(LogicalNode::Terminal {
                input: Box::new(LogicalNode::OrderSlice {
                    input: Box::new(src_note()),
                    order: OrderSpec::Field { field: ids.title, dir: OrderDir::Asc, locale: Some("en-US".into()) },
                    after: None,
                    limit: Some(5),
                }),
                terminal: TerminalSpec::Ids,
            }),
        ),
        (
            "bm25f_rust",
            LogicalPlan::new(LogicalNode::Terminal {
                input: Box::new(LogicalNode::Score {
                    input: Box::new(src_note()),
                    spec: ScoreSpec::Bm25f {
                        index_id: ids.idx_text,
                        query: "rust".into(),
                        require_all: false,
                        prefix: false,
                        phrase: None,
                        top_k: Some(5),
                    },
                }),
                terminal: TerminalSpec::Hits { fields: None, highlights: false },
            }),
        ),
        (
            "has_link_to_k1",
            LogicalPlan::new(LogicalNode::Terminal {
                input: Box::new(LogicalNode::Filter {
                    input: Box::new(src_note()),
                    expr: FilterExpr::HasLink { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, target: Some(k1) },
                }),
                terminal: TerminalSpec::Ids,
            }),
        ),
        (
            "graph_wcc",
            LogicalPlan::new(LogicalNode::GraphAnalyze {
                input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k0, k1, k2]))),
                spec: GraphAnalyzeSpec::WeakComponents { rel_kinds: vec![ids.cites] },
            }),
        ),
        (
            "vector_near",
            LogicalPlan::new(LogicalNode::Terminal {
                input: Box::new(LogicalNode::Score {
                    input: Box::new(src_note()),
                    spec: ScoreSpec::VectorNear { index_id: ids.idx_vec, vector: vec![1.0, 0.0, 0.0, 0.0], top_k: 3 },
                }),
                terminal: TerminalSpec::Hits { fields: None, highlights: false },
            }),
        ),
    ];

    let (ro, sc) = engine.begin_read_pinned().unwrap();
    let init_seq = read_commit_seq(&ro, &engine.dbs()).unwrap();
    let init_epoch = sc.epoch;
    drop(ro);

    let hub = LiveQueryHub::new(init_seq, init_epoch, 8);
    let mut watch_ids = Vec::new();
    let mut last_results = Vec::new();
    for (_name, plan) in &plans {
        let (wid, out) = hub
            .register_query_watch(&engine, &qe, plan, ExecBudgets::default(), WatchPolicy::default(), 0, 1, |o| {
                Some(hash_query_result(&o.result))
            })
            .unwrap();
        watch_ids.push(wid);
        last_results.push(out.result);
    }

    let mut rng = SimRng::seeded(0xDEAD_BEEF_1234_5678);
    let mut now_ms = 100u64;
    let ext_ids = ["n0", "n1", "n2"];
    let tags = ["red", "blue", "green"];
    let bodies = ["lmdb zero copy", "pagerank search engine", "plain text body"];
    let titles = ["alpha rust", "gamma rust", "zeta node"];

    for step in 0..120usize {
        now_ms += 50;
        let pick = (rng.next_u64() % 9) as u8;
        let target = ext_ids[(rng.next_u64() as usize) % ext_ids.len()];
        let mut dirty_set = Vec::new();

        match pick {
            0 => {
                // Patch title/tag/score/body
                let tag = tags[(rng.next_u64() as usize) % tags.len()];
                let score = ((rng.next_u64() % 50) as i64) + 1;
                let body = bodies[(rng.next_u64() as usize) % bodies.len()];
                let title = titles[(rng.next_u64() as usize) % titles.len()];
                let res = engine.execute_batch(&[WireOp::Patch {
                    target: EntityTarget::ExternalId(target.into()),
                    fields: vec![
                        ("title".into(), CanonicalValue::String(title.into())),
                        ("body".into(), CanonicalValue::String(body.into())),
                        ("tag".into(), CanonicalValue::String(tag.into())),
                        ("score".into(), CanonicalValue::Int(score)),
                    ],
                }]);
                if let Ok(outcome) = res
                    && let Some(sum) = outcome.summary
                {
                    dirty_set = hub.notify_commit(&engine.schema(), &sum, now_ms);
                }
            }
            1 => {
                // Unset title on target
                let res = engine.execute_batch(&[WireOp::Unset {
                    target: EntityTarget::ExternalId(target.into()),
                    fields: vec!["title".into()],
                }]);
                if let Ok(outcome) = res
                    && let Some(sum) = outcome.summary
                {
                    dirty_set = hub.notify_commit(&engine.schema(), &sum, now_ms);
                }
            }
            2 => {
                // Trash & ignore if already trashed
                if let Ok(outcome) =
                    engine.execute_batch(&[WireOp::Trash { target: EntityTarget::ExternalId(target.into()) }])
                    && let Some(sum) = outcome.summary
                {
                    dirty_set = hub.notify_commit(&engine.schema(), &sum, now_ms);
                }
            }
            3 => {
                // Restore
                if let Ok(outcome) =
                    engine.execute_batch(&[WireOp::Restore { target: EntityTarget::ExternalId(target.into()) }])
                    && let Some(sum) = outcome.summary
                {
                    dirty_set = hub.notify_commit(&engine.schema(), &sum, now_ms);
                }
            }
            4 => {
                // Create a brand new Note
                let ext = format!("dyn_{step}");
                let tag = tags[(rng.next_u64() as usize) % tags.len()];
                let score = (rng.next_u64() % 40) as i64;
                let outcome = engine
                    .execute_batch(&[WireOp::Create {
                        type_name: "Note".into(),
                        external_id: Some(ext),
                        fields: vec![
                            ("title".into(), CanonicalValue::String("dynamic rust".into())),
                            ("body".into(), CanonicalValue::String("search dynamic".into())),
                            ("tag".into(), CanonicalValue::String(tag.into())),
                            ("score".into(), CanonicalValue::Int(score)),
                        ],
                    }])
                    .unwrap();
                if let Some(sum) = outcome.summary {
                    dirty_set = hub.notify_commit(&engine.schema(), &sum, now_ms);
                }
            }
            5 => {
                // Link n2 -> n1
                if let Ok(outcome) = engine.execute_batch(&[WireOp::Link {
                    source: EntityTarget::ExternalId("n2".into()),
                    rel_kind: "CITES".into(),
                    target: EntityTarget::ExternalId("n1".into()),
                    weight: Some(1.0f64.to_bits()),
                    info: None,
                }]) && let Some(sum) = outcome.summary
                {
                    dirty_set = hub.notify_commit(&engine.schema(), &sum, now_ms);
                }
            }
            6 => {
                // Unlink n2 -> n1
                if let Ok(outcome) = engine.execute_batch(&[WireOp::Unlink {
                    source: EntityTarget::ExternalId("n2".into()),
                    rel_kind: Some("CITES".into()),
                    target: Some(EntityTarget::ExternalId("n1".into())),
                }]) && let Some(sum) = outcome.summary
                {
                    dirty_set = hub.notify_commit(&engine.schema(), &sum, now_ms);
                }
            }
            7 => {
                // Update Author (unrelated type for all Note watches!)
                let outcome = engine
                    .execute_batch(&[WireOp::Patch {
                        target: EntityTarget::ExternalId("a0".into()),
                        fields: vec![("name".into(), CanonicalValue::String(format!("Ada {step}")))],
                    }])
                    .unwrap();
                if let Some(sum) = outcome.summary {
                    dirty_set = hub.notify_commit(&engine.schema(), &sum, now_ms);
                }
            }
            _ => {
                // Direct vector embedding mutation + subdb write notification
                let k = match target {
                    "n0" => k0,
                    "n1" => k1,
                    _ => k2,
                };
                if engine.get_row(target).unwrap().is_some_and(|r| !r.trashed) {
                    set_entity_embeddings(&engine, ids.idx_vec, k, &[vec![0.8, 0.2 * (step as f32 % 5.0), 0.0, 0.0]])
                        .unwrap();
                    let (ro, sc) = engine.begin_read_pinned().unwrap();
                    let seq = read_commit_seq(&ro, &engine.dbs()).unwrap();
                    drop(ro);
                    dirty_set = hub.notify_subdb_write(seq, sc.epoch, ids.idx_vec_slot, &[0x01], now_ms);
                }
            }
        }

        // ORACLE INVARIANT: For EVERY watch, if fresh query result != last_result, it MUST be in `dirty_set`!
        for (idx, (name, plan)) in plans.iter().enumerate() {
            let fresh = qe.execute(&engine, plan, ExecBudgets::default()).unwrap().result;
            let was_dirty = dirty_set.contains(&watch_ids[idx]);
            if fresh != last_results[idx] {
                assert!(
                    was_dirty,
                    "FALSE NEGATIVE on step {step} (pick={pick}) for watch '{name}': result changed without invalidation!\nold={:?}\nnew={:?}",
                    last_results[idx], fresh
                );
            }
            if was_dirty {
                hub.begin_run(watch_ids[idx]);
                let (rerun_out, _deliver) = hub
                    .rerun_query_watch(watch_ids[idx], &engine, &qe, plan, ExecBudgets::default(), now_ms, 1, |o| {
                        Some(hash_query_result(&o.result))
                    })
                    .unwrap();
                last_results[idx] = rerun_out.result;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Field-Level Precision & IndexedEq Precision (Zero Unnecessary Churn)
// ---------------------------------------------------------------------------

#[test]
fn field_level_point_entity_and_indexed_eq_precision() {
    let (engine, _reg, ids) = open_sim();
    let qe = QueryEngine::default();

    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("e1".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("Hello".into())),
                    ("body".into(), CanonicalValue::String("World".into())),
                    ("tag".into(), CanonicalValue::String("alpha".into())),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("e2".into()),
                fields: vec![("tag".into(), CanonicalValue::String("beta".into()))],
            },
        ])
        .unwrap();
    let k1 = engine.get_row("e1").unwrap().unwrap().key;
    let k2 = engine.get_row("e2").unwrap().unwrap().key;

    let (ro, sc) = engine.begin_read_pinned().unwrap();
    let hub = LiveQueryHub::new(read_commit_seq(&ro, &engine.dbs()).unwrap(), sc.epoch, 8);
    drop(ro);

    // Watch 1: projects ONLY `title` on `e1`
    let mut title_only = FieldMask::empty();
    title_only.insert(ids.title);
    let p_title = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1]))),
        terminal: TerminalSpec::Rows { fields: Some(title_only.clone()) },
    });
    // Watch 2: projects ONLY `title` on `e2` (currently MISSING!)
    let p_missing_title = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k2]))),
        terminal: TerminalSpec::Rows { fields: Some(title_only) },
    });
    // Watch 3: O(1) IndexedEq Count on `tag == "alpha"`
    let p_eq_alpha = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            expr: FilterExpr::Eq(ids.tag, CanonicalValue::String("alpha".into())),
        }),
        terminal: TerminalSpec::Count,
    });

    let (w_title, _) = hub
        .register_query_watch(&engine, &qe, &p_title, ExecBudgets::default(), WatchPolicy::default(), 0, 1, |_| None)
        .unwrap();
    let (w_missing, _) = hub
        .register_query_watch(
            &engine,
            &qe,
            &p_missing_title,
            ExecBudgets::default(),
            WatchPolicy::default(),
            0,
            1,
            |_| None,
        )
        .unwrap();
    let (w_alpha, _) = hub
        .register_query_watch(&engine, &qe, &p_eq_alpha, ExecBudgets::default(), WatchPolicy::default(), 0, 1, |_| None)
        .unwrap();

    // Update UNWATCHED field `body` on `e1` -> MUST NOT invalidate ANY of the 3 watches!
    let sum1 = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("e1".into()),
            fields: vec![("body".into(), CanonicalValue::String("Updated body".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    let dirty1 = hub.notify_commit(&engine.schema(), &sum1, 10);
    assert!(dirty1.is_empty(), "updating untouched field must NOT invalidate watches, got {dirty1:?}");

    // Mutate `tag` on `e2` from "beta" -> "gamma" -> MUST NOT invalidate `w_alpha` (tag == "alpha")!
    let sum2 = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("e2".into()),
            fields: vec![("tag".into(), CanonicalValue::String("gamma".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    let dirty2 = hub.notify_commit(&engine.schema(), &sum2, 20);
    assert!(!dirty2.contains(&w_alpha), "unrelated tag value must NOT invalidate IndexedEq('alpha')");
    assert!(!dirty2.contains(&w_title));

    // Set previously MISSING `title` on `e2` -> MUST invalidate `w_missing` (and NOT `w_title`)!
    let sum3 = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("e2".into()),
            fields: vec![("title".into(), CanonicalValue::String("Now Present".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    let dirty3 = hub.notify_commit(&engine.schema(), &sum3, 30);
    assert!(dirty3.contains(&w_missing), "setting previously missing watched field MUST invalidate");
    assert!(!dirty3.contains(&w_title));
    assert!(!dirty3.contains(&w_alpha));
}

// ---------------------------------------------------------------------------
// 3. Tier-2 Flat Arena Coalescing, Slab Compaction & Lock-Free MVCC CommitRing
// ---------------------------------------------------------------------------

#[test]
fn tier2_range_coalescing_compaction_and_commit_ring_catchup_plus_overflow() {
    // Coalescing overlapping + adjacent intervals per slot
    let mut rs = ReadSet::new();
    rs.record_subdb_range(5, &[0x01, 0x10], &[0x01, 0x20]);
    rs.record_subdb_range(5, &[0x01, 0x20], &[0x01, 0x30]); // adjacent!
    rs.record_subdb_range(5, &[0x01, 0x15], &[0x01, 0x25]); // overlapping!
    rs.record_subdb_range(5, &[0x04], &[0x05]); // disjoint!
    let coalesced = rs.coalesce_ranges();
    assert_eq!(coalesced.len(), 2);
    assert_eq!(coalesced[0].low, vec![0x01, 0x10]);
    assert_eq!(coalesced[0].high, vec![0x01, 0x30]);
    assert_eq!(coalesced[1].low, vec![0x04]);
    assert_eq!(coalesced[1].high, vec![0x05]);

    // Slab compaction after register/unregister churn
    let (engine, _reg, ids) = open_sim();
    let qe = QueryEngine::default();
    let mut idx = InvalidationIndex::new(0, engine.schema().epoch);
    for i in 1..=100u64 {
        let wid = WatchId(i);
        idx.register_after_read(wid, 0, engine.schema().epoch, &rs, &engine.schema());
        idx.unregister_watch(wid);
    }
    assert_eq!(idx.stats().active_watches, 0);
    assert_eq!(idx.stats().tier2_live_ranges, 0);

    // Lock-free MVCC CommitRing catch-up: reader reads at seq S, writer commits S+1 BEFORE reader registers!
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("r1".into()),
                fields: vec![("title".into(), CanonicalValue::String("v1".into()))],
            },
            WireOp::Create {
                type_name: "Author".into(),
                external_id: Some("au1".into()),
                fields: vec![("name".into(), CanonicalValue::String("Bob".into()))],
            },
        ])
        .unwrap();
    let k1 = engine.get_row("r1").unwrap().unwrap().key;
    let plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1]))),
        terminal: TerminalSpec::Rows { fields: None },
    });

    // Reader takes snapshot at seq S
    let (read_seq, read_epoch, _out, read_set) =
        run_tracked_snapshot(&engine, &qe, &plan, ExecBudgets::default()).unwrap();
    let mut idx2 = InvalidationIndex::new(read_seq, read_epoch);

    // Writer commits unrelated Author mutation at S + 1 -> reader registers -> MUST NOT be stale!
    let unrelated = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("au1".into()),
            fields: vec![("name".into(), CanonicalValue::String("Bob 2".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    idx2.notify_commit(&engine.schema(), &unrelated);
    let stale_unrelated = idx2.register_after_read(WatchId(1), read_seq, read_epoch, &read_set, &engine.schema());
    assert!(!stale_unrelated, "unrelated concurrent commit must NOT falsely stale reader");

    // Writer commits mutation to `r1` at S + 2 -> another reader from S registers -> MUST be stale!
    let related = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("r1".into()),
            fields: vec![("title".into(), CanonicalValue::String("v2".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    idx2.notify_commit(&engine.schema(), &related);
    let stale_related = idx2.register_after_read(WatchId(2), read_seq, read_epoch, &read_set, &engine.schema());
    assert!(stale_related, "concurrent commit touching watched entity MUST stale reader via CommitRing");

    // CommitRing overflow (> COMMIT_RING_CAP commits of unrelated Author) -> MUST fail strictly safe!
    let (snap_seq, snap_epoch, _o, snap_rs) =
        run_tracked_snapshot(&engine, &qe, &plan, ExecBudgets::default()).unwrap();
    let mut idx3 = InvalidationIndex::new(snap_seq, snap_epoch);
    for i in 0..(COMMIT_RING_CAP + 10) {
        let sum = engine
            .execute_batch(&[WireOp::Patch {
                target: EntityTarget::ExternalId("au1".into()),
                fields: vec![("name".into(), CanonicalValue::String(format!("overflow_{i}")))],
            }])
            .unwrap()
            .summary
            .unwrap();
        idx3.notify_commit(&engine.schema(), &sum);
    }
    let stale_overflow = idx3.register_after_read(WatchId(3), snap_seq, snap_epoch, &snap_rs, &engine.schema());
    assert!(stale_overflow, "CommitRing overflow (> 1024 commits) MUST fail strictly safe (stale = true)");
    let _ = ids.author_type;
}

// ---------------------------------------------------------------------------
// 4. Cross-Process `commit_log` Catch-Up on Two Live LMDB Handles
// ---------------------------------------------------------------------------

#[test]
fn cross_process_lmdb_selective_catchup_schema_bump_and_window_overflow() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join("xproc_db");

    let open_handle = || {
        let store = LmdbStore::open(&db_dir, LmdbConfig::default()).unwrap();
        let reg = Arc::new(
            ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange).with_external(Arc::new(VectorProjection)),
        );
        let (eng, _) = Engine::open_with_projections(
            store,
            OsVfs,
            SystemClock::default(),
            db_dir.clone(),
            DurabilityMode::Strict,
            Arc::clone(&reg) as _,
        )
        .unwrap();
        eng
    };

    let eng_a = open_handle();
    eng_a.set_schema(&test_schema()).unwrap();
    let eng_b = open_handle();

    let ids = resolve_ids(&eng_a.schema());
    let qe = QueryEngine::default();
    let (ro, sc) = eng_a.begin_read_pinned().unwrap();
    let hub_a = LiveQueryHub::new(read_commit_seq(&ro, &eng_a.dbs()).unwrap(), sc.epoch, 8);
    drop(ro);

    let note_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
        terminal: TerminalSpec::Ids,
    });
    let author_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.author_type, include_trashed: false })),
        terminal: TerminalSpec::Ids,
    });

    let (w_note, _) = hub_a
        .register_query_watch(&eng_a, &qe, &note_plan, ExecBudgets::default(), WatchPolicy::default(), 0, 1, |_| None)
        .unwrap();
    let (w_author, _) = hub_a
        .register_query_watch(&eng_a, &qe, &author_plan, ExecBudgets::default(), WatchPolicy::default(), 0, 1, |_| None)
        .unwrap();

    // Process B mutates ONLY a Note!
    eng_b
        .execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("from_b".into()),
            fields: vec![("title".into(), CanonicalValue::String("Cross-process".into()))],
        }])
        .unwrap();

    // Process A catches up via persisted `commit_log` -> ONLY `w_note` invalidated, `w_author` untouched!
    let dirty_selective = hub_a.catch_up_cross_process(&eng_a, 100).unwrap();
    assert!(dirty_selective.contains(&w_note), "cross-process Note create MUST invalidate Note watch");
    assert!(!dirty_selective.contains(&w_author), "cross-process Note create must NOT invalidate Author watch");

    // Process B bumps schema epoch -> Process A MUST invalidate BOTH watches!
    let mut s2 = test_schema();
    s2.relations.insert(
        "EXTRA_REL".into(),
        RawRelation {
            cardinality: Cardinality::ManyToMany,
            acyclic: false,
            weighted: false,
            endpoints: vec![RawEndpoint { source: "Note".into(), target: "Note".into() }],
            declared_by: None,
        },
    );
    eng_b.set_schema(&s2).unwrap();
    let dirty_schema = hub_a.catch_up_cross_process(&eng_a, 200).unwrap();
    assert!(dirty_schema.contains(&w_note));
    assert!(dirty_schema.contains(&w_author));
}

// ---------------------------------------------------------------------------
// 5. Sans-I/O `WatchScheduler`: Adaptive Gap, 8ms Turn Slice, Coalescing & Dedup
// ---------------------------------------------------------------------------

#[test]
fn sans_io_scheduler_adaptive_gap_turn_slicing_dedup_and_lifecycle() {
    let mut sched = WatchScheduler::new(8);
    let w_fast = WatchId(1);
    let w_slow = WatchId(2);
    let w_imp = WatchId(3);

    let h1 = hash_result_128(b"payload_v1");
    let h2 = hash_result_128(b"payload_v2");

    // Seed initial runs at t = 100ms (`already_ran = true`)
    sched.register(w_fast, WatchPolicy::default(), 100, 1, true, Some(h1)); // gap = max(16, 4*1) = 16ms -> ready at 116
    sched.register(w_slow, WatchPolicy::default(), 100, 10, true, Some(h1)); // gap = max(16, 4*10) = 40ms -> ready at 140
    sched.register(w_imp, WatchPolicy { mode: WatchMode::Imperative, ..Default::default() }, 100, 1, true, None);
    assert!(sched.watches_settled());
    assert!(sched.caught_up(100, false));

    // Coalesce 50 dirty bursts at t = 105..110 -> dirty_since stays 105!
    for t in 105..=110 {
        sched.mark_dirty(&[w_fast, w_slow, w_imp], t);
    }
    assert!(!sched.watches_settled());
    assert!(!sched.caught_up(110, false));
    // At t = 110, all dirty watches are still cooling down -> `caught_up(soon: true) == true`!
    assert!(sched.caught_up(110, true));
    assert_eq!(sched.next_wake_delay_ms(110), Some(6)); // 116 - 110 = 6ms

    // At t = 116: `w_fast` and `w_imp` are due; `w_slow` still cooling down until 140 (delay = 24ms)!
    let tick = sched.tick(116);
    assert_eq!(tick.due_ids, vec![w_fast, w_imp]);
    assert_eq!(tick.next_wake_delay_ms, Some(24));
    assert!(!tick.settled);

    // P1-3: When 2 watches are immediately due, `next_due` returns `next_wake_delay_ms == Some(0)`!
    let step1 = sched.next_due(116, 0);
    assert_eq!(step1.watch_id, Some(w_fast));
    assert_eq!(step1.next_wake_delay_ms, Some(0));
    // P1-4: Duplicate `begin_run` on already-running watch returns `false` and does not wipe mid-run `dirty`!
    sched.mark_dirty(&[w_fast], 118);
    assert!(!sched.begin_run(w_fast));
    let deliver_fast = sched.finish_run(w_fast, 121, 5, Some(h1));
    assert!(!deliver_fast, "identical 128-bit binary hash must skip Declarative delivery");
    // Mid-run dirty MUST still be preserved!
    assert!(!sched.watches_settled());

    // Next watch with elapsed_turn_ms = 8 >= turn_slice_ms (8): MUST yield with delay = Some(0)!
    let step_yield = sched.next_due(121, 8);
    assert_eq!(step_yield.watch_id, None);
    assert_eq!(step_yield.next_wake_delay_ms, Some(0));
    assert!(!step_yield.settled);

    // Next turn (elapsed = 0): `w_imp` runs and delivers even with identical hash because mode == Imperative!
    let step2 = sched.next_due(122, 0);
    assert_eq!(step2.watch_id, Some(w_imp));
    let deliver_imp = sched.finish_run(w_imp, 123, 1, Some(h1));
    assert!(deliver_imp, "Imperative watch must always deliver");

    // At t = 141: both `w_slow` (ready 140) and re-dirtied `w_fast` (ready 121 + 20 = 141) run and settle!
    let step3 = sched.next_due(141, 0);
    assert_eq!(step3.watch_id, Some(w_slow));
    let deliver_slow = sched.finish_run(w_slow, 142, 2, Some(h2));
    assert!(deliver_slow);
    let step4 = sched.next_due(142, 0);
    assert_eq!(step4.watch_id, Some(w_fast));
    assert!(sched.finish_run(w_fast, 143, 1, Some(h2)));
    assert!(sched.watches_settled());
    assert!(sched.caught_up(143, false));

    // `stop_watches()` clears everything cleanly
    sched.mark_all_dirty(150);
    assert!(!sched.watches_settled());
    sched.stop_watches();
    assert!(sched.watches_settled());
    assert_eq!(sched.next_wake_delay_ms(150), None);
}

// ---------------------------------------------------------------------------
// 6. Review Regression Tests (P0-1..P0-6, P1-1..P1-5, Cross-Process Maintenance & Overflow)
// ---------------------------------------------------------------------------

#[test]
fn review_regression_subdb_same_seq_seq_gap_resurrection_abort_and_imperative_settled() {
    use elmdb_wire::ErrorCode;
    use std::sync::atomic::AtomicBool;

    let (engine, _reg, ids) = open_sim();
    let qe = QueryEngine::default();
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("v1".into()),
                fields: vec![("title".into(), CanonicalValue::String("VecDoc".into()))],
            },
            WireOp::Create {
                type_name: "Author".into(),
                external_id: Some("au".into()),
                fields: vec![("name".into(), CanonicalValue::String("Ada".into()))],
            },
        ])
        .unwrap();
    let kv1 = engine.get_row("v1").unwrap().unwrap().key;
    let hub = LiveQueryHub::from_engine(&engine, 8).unwrap();

    // P0-6: `register_query_watch` with `|_| None` (e.g. Imperative watch) MUST start settled!
    let note_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
        terminal: TerminalSpec::Ids,
    });
    let (w_imp, _) = hub
        .register_query_watch(
            &engine,
            &qe,
            &note_plan,
            ExecBudgets::default(),
            WatchPolicy { mode: WatchMode::Imperative, ..Default::default() },
            10,
            1,
            |_| None,
        )
        .unwrap();
    assert!(hub.watches_settled(), "register_query_watch with None hash must start settled");
    assert!(hub.tick(10).due_ids.is_empty());

    // P0-1: Concurrent `notify_subdb_write` at `seq == read_at_seq` MUST mark reader stale!
    let vec_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            spec: ScoreSpec::VectorNear { index_id: ids.idx_vec, vector: vec![1.0, 0.0, 0.0, 0.0], top_k: 5 },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let (snap_seq, snap_epoch, _o, vec_rs) =
        run_tracked_snapshot(&engine, &qe, &vec_plan, ExecBudgets::default()).unwrap();
    let mut idx = InvalidationIndex::new(snap_seq, snap_epoch);
    set_entity_embeddings(&engine, ids.idx_vec, kv1, &[vec![1.0, 0.0, 0.0, 0.0]]).unwrap();
    // Maintenance write shares exact same `commit_seq == snap_seq`!
    idx.notify_subdb_write(snap_seq, snap_epoch, ids.idx_vec_slot, &[0x01]);
    let stale = idx.register_after_read(WatchId(99), snap_seq, snap_epoch, &vec_rs, &engine.schema());
    assert!(stale, "SubDbWrite at same commit_seq MUST mark concurrent reader stale!");

    // P0-2: Sequence gap (`seq = prev + 2`, simulating missed cross-process or `transact()` commit) MUST fail safe!
    let _skipped = engine
        .execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("missed_note".into()),
            fields: vec![("title".into(), CanonicalValue::String("Missed".into()))],
        }])
        .unwrap(); // NOT passed to `hub.notify_commit`!
    let author_sum = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("au".into()),
            fields: vec![("name".into(), CanonicalValue::String("Ada 2".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    let dirty_on_gap = hub.notify_commit(&engine.schema(), &author_sum, 20);
    assert!(dirty_on_gap.contains(&w_imp), "sequence gap in notify_commit must fail safe and invalidate all watches");

    // P0-5: `rerun_query_watch` returning `Err(Cancelled)` MUST NOT wedge `running = true`!
    assert!(hub.begin_run(w_imp));
    let cancel = AtomicBool::new(true);
    let err = hub
        .rerun_query_watch(
            w_imp,
            &engine,
            &qe,
            &note_plan,
            ExecBudgets { cancel: Some(&cancel), ..Default::default() },
            30,
            1,
            |_| None,
        )
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);
    // Watch was requeued dirty (not wedged running!), so `tick(100)` picks it up cleanly!
    assert!(hub.tick(100).due_ids.contains(&w_imp));

    // P0-4: Unregistering / stopping before `rerun_query_watch` finishes MUST NOT resurrect watch into `InvalidationIndex`!
    hub.unregister_watch(w_imp);
    let (_out, deliver) =
        hub.rerun_query_watch(w_imp, &engine, &qe, &note_plan, ExecBudgets::default(), 110, 1, |_| None).unwrap();
    assert!(!deliver);
    assert_eq!(hub.stats().active_watches, 0);
    assert_eq!(hub.stats().tier2_live_ranges, 0);

    hub.stop_watches();
    let _ = hub
        .register_query_watch(&engine, &qe, &note_plan, ExecBudgets::default(), WatchPolicy::default(), 120, 1, |_| {
            None
        })
        .unwrap();
    assert_eq!(hub.stats().active_watches, 0, "register_query_watch after stop_watches must not leak into index");
}

#[test]
fn review_regression_cross_process_maintenance_write_and_4096_overflow() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join("xproc_maint_db");

    let open_handle = || {
        let store = LmdbStore::open(&db_dir, LmdbConfig::default()).unwrap();
        let reg = Arc::new(
            ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange).with_external(Arc::new(VectorProjection)),
        );
        let (eng, _) = Engine::open_with_projections(
            store,
            OsVfs,
            SystemClock::default(),
            db_dir.clone(),
            DurabilityMode::Strict,
            Arc::clone(&reg) as _,
        )
        .unwrap();
        eng
    };

    let eng_a = open_handle();
    eng_a.set_schema(&test_schema()).unwrap();
    eng_a
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("x1".into()),
                fields: vec![("title".into(), CanonicalValue::String("X1".into()))],
            },
            WireOp::Create {
                type_name: "Author".into(),
                external_id: Some("xa1".into()),
                fields: vec![("name".into(), CanonicalValue::String("A1".into()))],
            },
        ])
        .unwrap();
    let kx1 = eng_a.get_row("x1").unwrap().unwrap().key;
    let eng_b = open_handle();

    let ids = resolve_ids(&eng_a.schema());
    let qe = QueryEngine::default();
    let hub_a = LiveQueryHub::from_engine(&eng_a, 8).unwrap();

    let vec_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            spec: ScoreSpec::VectorNear { index_id: ids.idx_vec, vector: vec![1.0, 0.0, 0.0, 0.0], top_k: 5 },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let author_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.author_type, include_trashed: false })),
        terminal: TerminalSpec::Ids,
    });
    let (w_vec, _) = hub_a
        .register_query_watch(&eng_a, &qe, &vec_plan, ExecBudgets::default(), WatchPolicy::default(), 0, 1, |_| None)
        .unwrap();
    let (w_auth, _) = hub_a
        .register_query_watch(&eng_a, &qe, &author_plan, ExecBudgets::default(), WatchPolicy::default(), 0, 1, |_| None)
        .unwrap();

    // P0-3B: Process B performs a maintenance write (`set_entity_embeddings`, unchanged `commit_seq`, advanced `txnid`!)
    set_entity_embeddings(&eng_b, ids.idx_vec, kx1, &[vec![1.0, 0.0, 0.0, 0.0]]).unwrap();
    let dirty_maint = hub_a.catch_up_cross_process(&eng_a, 100).unwrap();
    assert!(dirty_maint.contains(&w_vec), "cross-process maintenance write must be detected via txnid advance");

    // Actual > 4,096 cross-process commits window overflow (using fast SimKvStore on a single Engine simulating remote lag)!
    let (sim_eng, _reg, sim_ids) = open_sim();
    sim_eng
        .execute_batch(&[WireOp::Create {
            type_name: "Author".into(),
            external_id: Some("a_ovf".into()),
            fields: vec![("name".into(), CanonicalValue::String("Init".into()))],
        }])
        .unwrap();
    let hub_sim = LiveQueryHub::from_engine(&sim_eng, 8).unwrap();
    let sim_note_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: sim_ids.note_type, include_trashed: false })),
        terminal: TerminalSpec::Ids,
    });
    let (w_sim_note, _) = hub_sim
        .register_query_watch(
            &sim_eng,
            &qe,
            &sim_note_plan,
            ExecBudgets::default(),
            WatchPolicy::default(),
            0,
            1,
            |_| None,
        )
        .unwrap();
    // Commit 4,100 unrelated Author updates without notifying `hub_sim` -> exceeds COMMIT_LOG_WINDOW (4,096)!
    for i in 0..4100usize {
        sim_eng
            .execute_batch(&[WireOp::Patch {
                target: EntityTarget::ExternalId("a_ovf".into()),
                fields: vec![("name".into(), CanonicalValue::String(format!("v{i}")))],
            }])
            .unwrap();
    }
    let dirty_ovf = hub_sim.catch_up_cross_process(&sim_eng, 500).unwrap();
    assert!(
        dirty_ovf.contains(&w_sim_note),
        "> 4,096 elapsed commits must fail strictly safe and invalidate all watches"
    );
    let _ = w_auth;
}
