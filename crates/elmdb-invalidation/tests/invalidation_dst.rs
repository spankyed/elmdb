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
        let _ = idx.register_after_read(wid, 0, engine.schema().epoch, &rs, &engine.schema());
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
    let (read_seq, _read_maint, read_epoch, _out, read_set) =
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
    let (stale_unrelated, _) = idx2.register_after_read(WatchId(1), read_seq, read_epoch, &read_set, &engine.schema());
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
    let (stale_related, _) = idx2.register_after_read(WatchId(2), read_seq, read_epoch, &read_set, &engine.schema());
    assert!(stale_related, "concurrent commit touching watched entity MUST stale reader via CommitRing");

    // CommitRing overflow (> COMMIT_RING_CAP commits of unrelated Author) -> MUST fail strictly safe!
    let (snap_seq, _snap_maint, snap_epoch, _o, snap_rs) =
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
    let (stale_overflow, _) = idx3.register_after_read(WatchId(3), snap_seq, snap_epoch, &snap_rs, &engine.schema());
    assert!(stale_overflow, "CommitRing overflow (> 1024 commits) MUST fail strictly safe (stale = true)");
    let _ = ids.author_type;
}

// ---------------------------------------------------------------------------
// 4. Real Cross-Process `commit_log` Catch-Up on Two OS Processes
// ---------------------------------------------------------------------------

#[test]
#[ignore = "child process helper invoked by cross_process_lmdb_selective_catchup_schema_bump_and_window_overflow"]
fn child_xproc_invalidation_writer() {
    let db_dir = std::path::PathBuf::from(std::env::var("ELMDB_TEST_XPROC_DIR").expect("ELMDB_TEST_XPROC_DIR"));
    let action = std::env::var("ELMDB_TEST_XPROC_ACTION").expect("ELMDB_TEST_XPROC_ACTION");
    let store = LmdbStore::open(&db_dir, LmdbConfig::default()).unwrap();
    let reg =
        Arc::new(ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange).with_external(Arc::new(VectorProjection)));
    let (eng_b, _) = Engine::open_with_projections(
        store,
        OsVfs::new(),
        SystemClock::default(),
        db_dir,
        DurabilityMode::Strict,
        Arc::clone(&reg) as _,
    )
    .unwrap();
    match action.as_str() {
        "create_note" => {
            eng_b
                .execute_batch(&[WireOp::Create {
                    type_name: "Note".into(),
                    external_id: Some("from_b".into()),
                    fields: vec![("title".into(), CanonicalValue::String("Cross-process".into()))],
                }])
                .unwrap();
        }
        "bump_schema" => {
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
        }
        "set_embeddings" => {
            let ids = resolve_ids(&eng_b.schema());
            let kx1 = eng_b.get_row("x1").unwrap().unwrap().key;
            set_entity_embeddings(&eng_b, ids.idx_vec, kx1, &[vec![1.0, 0.0, 0.0, 0.0]]).unwrap();
        }
        other => panic!("unknown action: {other}"),
    }
}

fn run_xproc_child(db_dir: &std::path::Path, action: &str) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_xproc_invalidation_writer", "--ignored", "--nocapture"])
        .env("ELMDB_TEST_XPROC_DIR", db_dir)
        .env("ELMDB_TEST_XPROC_ACTION", action)
        .status()
        .unwrap();
    assert!(status.success(), "child writer '{action}' failed: {status}");
}

#[test]
fn cross_process_lmdb_selective_catchup_schema_bump_and_window_overflow() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join("xproc_db");

    let store = LmdbStore::open(&db_dir, LmdbConfig::default()).unwrap();
    let reg =
        Arc::new(ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange).with_external(Arc::new(VectorProjection)));
    let (eng_a, _) = Engine::open_with_projections(
        store,
        OsVfs::new(),
        SystemClock::default(),
        db_dir.clone(),
        DurabilityMode::Strict,
        Arc::clone(&reg) as _,
    )
    .unwrap();
    eng_a.set_schema(&test_schema()).unwrap();

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

    // Real child process B mutates ONLY a Note!
    run_xproc_child(&db_dir, "create_note");

    // Process A catches up via persisted `commit_log` -> ONLY `w_note` invalidated, `w_author` untouched!
    let dirty_selective = hub_a.catch_up_cross_process(&eng_a, 100).unwrap();
    assert!(dirty_selective.contains(&w_note), "cross-process Note create MUST invalidate Note watch");
    assert!(!dirty_selective.contains(&w_author), "cross-process Note create must NOT invalidate Author watch");

    // Real child process B bumps schema epoch -> Process A MUST invalidate BOTH watches!
    run_xproc_child(&db_dir, "bump_schema");
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
    let (snap_seq, _snap_maint, snap_epoch, _o, vec_rs) =
        run_tracked_snapshot(&engine, &qe, &vec_plan, ExecBudgets::default()).unwrap();
    let mut idx = InvalidationIndex::new(snap_seq, snap_epoch);
    set_entity_embeddings(&engine, ids.idx_vec, kv1, &[vec![1.0, 0.0, 0.0, 0.0]]).unwrap();
    // Maintenance write shares exact same `commit_seq == snap_seq`!
    idx.notify_subdb_write(snap_seq, snap_epoch, ids.idx_vec_slot, &[0x01]);
    let (stale, _) = idx.register_after_read(WatchId(99), snap_seq, snap_epoch, &vec_rs, &engine.schema());
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

    // P0-4 & E3: Unregistering / stopping before `rerun_query_watch` fails fast with Closed without resurrecting!
    hub.unregister_watch(w_imp);
    let err_unreg =
        hub.rerun_query_watch(w_imp, &engine, &qe, &note_plan, ExecBudgets::default(), 110, 1, |_| None).unwrap_err();
    assert_eq!(err_unreg.code, ErrorCode::Closed);
    assert_eq!(hub.stats().active_watches, 0);
    assert_eq!(hub.stats().tier2_live_ranges, 0);

    hub.stop_watches();
    let closed_err = hub
        .register_query_watch(&engine, &qe, &note_plan, ExecBudgets::default(), WatchPolicy::default(), 120, 1, |_| {
            None
        })
        .unwrap_err();
    assert_eq!(closed_err.code, ErrorCode::Closed, "register_query_watch after stop_watches must return Closed");
    assert_eq!(hub.stats().active_watches, 0, "register_query_watch after stop_watches must not leak into index");
}

#[test]
fn review_regression_cross_process_maintenance_write_and_4096_overflow() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join("xproc_maint_db");

    let store = LmdbStore::open(&db_dir, LmdbConfig::default()).unwrap();
    let reg =
        Arc::new(ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange).with_external(Arc::new(VectorProjection)));
    let (eng_a, _) = Engine::open_with_projections(
        store,
        OsVfs::new(),
        SystemClock::default(),
        db_dir.clone(),
        DurabilityMode::Strict,
        Arc::clone(&reg) as _,
    )
    .unwrap();
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

    // P0-3B: Real child process B performs a maintenance write (`set_entity_embeddings`, unchanged `commit_seq`, advanced `maint_seq`!)
    run_xproc_child(&db_dir, "set_embeddings");
    let dirty_maint = hub_a.catch_up_cross_process(&eng_a, 100).unwrap();
    assert!(dirty_maint.contains(&w_vec), "cross-process maintenance write must be detected via maint_seq advance");

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

#[test]
fn batch3_out_of_order_notify_commit_and_subdb_seq_gap_monotonicity() {
    let (engine, _reg, ids) = open_sim();
    let qe = QueryEngine::default();
    let base_sum = engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Author".into(),
                external_id: Some("a1".into()),
                fields: vec![("name".into(), CanonicalValue::String("A1".into()))],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n1".into()),
                fields: vec![("title".into(), CanonicalValue::String("N1".into()))],
            },
        ])
        .unwrap()
        .summary
        .unwrap();

    let mut idx = InvalidationIndex::new_with_maint_seq(base_sum.seq, base_sum.schema_epoch, base_sum.maint_seq);
    let note_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
        terminal: TerminalSpec::Ids,
    });
    let (s0, m0, e0, _o, rs0) = run_tracked_snapshot(&engine, &qe, &note_plan, ExecBudgets::default()).unwrap();
    assert!(!idx.register_after_read_with_maint(WatchId(1), s0, m0, e0, &rs0, &engine.schema()).0);

    // Produce two commits: seq S+1 (touches Note) and seq S+2 (touches Author).
    let sum1 = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("n1".into()),
            fields: vec![("title".into(), CanonicalValue::String("N1-v2".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    let sum2 = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("a1".into()),
            fields: vec![("name".into(), CanonicalValue::String("A1-v2".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();

    // Deliver OUT OF ORDER: sum2 first (seq gap -> fail-safe invalidates WatchId(1)), then late sum1!
    let d2 = idx.notify_commit(&engine.schema(), &sum2);
    assert!(d2.contains(&WatchId(1)), "out-of-order sum2 must fail-safe invalidate WatchId(1)");
    let d1 = idx.notify_commit(&engine.schema(), &sum1);
    assert!(d1.is_empty(), "late out-of-order sum1 must not corrupt ring or spuriously re-invalidate");

    // A fresh reader at S+2 MUST register cleanly as NOT stale!
    let (s2, m2, e2, _o2, rs2) = run_tracked_snapshot(&engine, &qe, &note_plan, ExecBudgets::default()).unwrap();
    assert!(!idx.register_after_read_with_maint(WatchId(1), s2, m2, e2, &rs2, &engine.schema()).0);

    // Standalone counter writes advance LMDB txnid without bumping commit_seq or maint_seq:
    // neither catch_up_cross_process nor a subsequent unrelated Author commit may falsely invalidate WatchId(1)!
    assert_eq!(engine.next_counter("TICKET").unwrap(), 1);
    engine.set_counter("TICKET", 50).unwrap();
    assert!(
        idx.catch_up_cross_process(&engine).unwrap().is_empty(),
        "standalone counter writes must NOT trigger cross-process watch invalidation"
    );
    let author_sum = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("a1".into()),
            fields: vec![("name".into(), CanonicalValue::String("A1-v3".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    assert!(
        idx.notify_commit(&engine.schema(), &author_sum).is_empty(),
        "unrelated Author commit after standalone counter writes must NOT falsely invalidate Note watch"
    );

    // Now commit seq S+4 WITHOUT calling notify_commit yet, and fire notify_subdb_write at seq S+4:
    // Because SubDbWrite carries no commit summary, `seq > prev_seq` MUST fail-safe invalidate WatchId(1)!
    let sum3 = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("n1".into()),
            fields: vec![("title".into(), CanonicalValue::String("N1-v3".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    let d_sub =
        idx.notify_subdb_write_at_maint(sum3.seq, sum3.schema_epoch, sum3.maint_seq + 1, ids.idx_vec_slot, &[0x01]);
    assert!(
        d_sub.contains(&WatchId(1)),
        "notify_subdb_write observing an unseen normal commit seq MUST fail-safe invalidate all watches"
    );

    // Review P1-1 regression: `notify_full_rebuild(seq, epoch)` at same `seq == latest_seq`
    // MUST mark a concurrent reader at `seq` as stale (not hit early fast-path return false)!
    let (s3, m3, e3, _o3, rs3) = run_tracked_snapshot(&engine, &qe, &note_plan, ExecBudgets::default()).unwrap();
    let mut idx_rb = InvalidationIndex::new_with_maint_seq(s3, e3, m3);
    idx_rb.notify_full_rebuild(s3, e3);
    assert!(
        idx_rb.register_after_read_with_maint(WatchId(42), s3, m3, e3, &rs3, &engine.schema()).0,
        "same-seq notify_full_rebuild MUST mark concurrent reader stale"
    );

    // Orthogonal `maint_seq` single-commit, stale-receipt, and gap regression tests (PR #261 P0-1):
    let (s4, m4, e4, _o4, rs4) = run_tracked_snapshot(&engine, &qe, &note_plan, ExecBudgets::default()).unwrap();
    let mut idx_mg = InvalidationIndex::new_with_maint_seq(s4, e4, m4);
    assert!(!idx_mg.register_after_read_with_maint(WatchId(77), s4, m4, e4, &rs4, &engine.schema()).0);
    // Per-commit local maintenance notifications (`m4 -> m4+1 -> m4+2`) on an unrelated vector slot stay selective!
    assert!(idx_mg.notify_subdb_write_at_maint(s4, e4, m4 + 1, ids.idx_vec_slot, &[0x01]).is_empty());
    assert!(idx_mg.notify_subdb_write_at_maint(s4, e4, m4 + 2, ids.idx_vec_slot, &[0x01]).is_empty());
    // Readers opened before or between chunks on Note are still NOT stale (slot doesn't match), and at m4+2 settle!
    assert!(!idx_mg.register_after_read_with_maint(WatchId(77), s4, m4, e4, &rs4, &engine.schema()).0);
    assert!(!idx_mg.register_after_read_with_maint(WatchId(77), s4, m4 + 1, e4, &rs4, &engine.schema()).0);
    assert!(!idx_mg.register_after_read_with_maint(WatchId(77), s4, m4 + 2, e4, &rs4, &engine.schema()).0);
    // Delayed stale receipt (`maint_seq <= latest_maint_seq`, even with an older schema_epoch) is safely ignored
    // and never advances `latest_maint_seq` or corrupts the ring!
    assert!(idx_mg.notify_subdb_write_at_maint(s4, e4, m4 + 1, ids.idx_vec_slot, &[0x01]).is_empty());
    assert!(idx_mg.notify_subdb_write_at_maint(s4, e4.saturating_sub(1), m4 + 1, ids.idx_vec_slot, &[0x01]).is_empty());
    assert_eq!(idx_mg.latest_maint_seq(), m4 + 2);
    // Unseen cross-process maintenance write (`m4 + 2 -> m4 + 4`) MUST fail-safe invalidate all watches!
    let d_mg = idx_mg.notify_subdb_write_at_maint(s4, e4, m4 + 4, ids.idx_vec_slot, &[0x01]);
    assert!(
        d_mg.contains(&WatchId(77)),
        "notify_subdb_write_at_maint with maint_seq gap (> prev_maint + 1) MUST fail-safe invalidate all watches"
    );

    // PR #261 P1-1 regression: zero-work `run_maintenance_write_opt` aborts without bumping `maint_seq`!
    let maint_before = {
        let (ro, _) = engine.begin_read_pinned().unwrap();
        elmdb_engine::read_maint_seq(&ro, &engine.dbs()).unwrap()
    };
    let opt_res: Option<((), _)> = engine.run_maintenance_write_opt(|_txn, _schema, _dbs| Ok(None)).unwrap();
    assert!(opt_res.is_none());
    let maint_after = {
        let (ro, _) = engine.begin_read_pinned().unwrap();
        elmdb_engine::read_maint_seq(&ro, &engine.dbs()).unwrap()
    };
    assert_eq!(maint_after, maint_before, "0-work run_maintenance_write_opt must NOT bump maint_seq");

    // Exhaustive `InvalidationIndex` & `LiveQueryHub` logical-clock boundary assertions:
    // 1. `new_with_maint_seq` initializes all clocks & drop watermarks; `stats()` reports them accurately.
    let schema_now = engine.schema();
    let e_now = schema_now.epoch;
    let mut idx_clk = InvalidationIndex::new_with_maint_seq(10, e_now, 5);
    let st = idx_clk.stats();
    assert_eq!(st.latest_seq, 10);
    assert_eq!(st.latest_schema_epoch, e_now);
    assert_eq!(st.latest_maint_seq, 5);
    // Older `read_at_seq < ring_dropped_seq_up_to` (with current maint) AND older `read_at_maint_seq < ring_dropped_maint_up_to`
    // (with current seq) must BOTH independently report stale!
    assert!(idx_clk.register_after_read_with_maint(WatchId(200), 9, 5, e_now, &rs4, &schema_now).0);
    assert!(idx_clk.register_after_read_with_maint(WatchId(200), 10, 4, e_now, &rs4, &schema_now).0);
    // Reader whose `read_schema_epoch == latest_schema_epoch` while `schema.epoch` differs must report stale!
    let mut idx_ep = InvalidationIndex::new_with_maint_seq(10, e_now + 1, 5);
    assert!(idx_ep.register_after_read_with_maint(WatchId(201), 10, 5, e_now + 1, &rs4, &schema_now).0);
    // Reader at `read_at_seq > ring_dropped_seq_up_to` (after 1 unrelated commit + 1 unrelated maint write in ring)
    // must NOT be falsely marked stale!
    let cp_author = elmdb_engine::CrossProcCommitSummary {
        seq: 11,
        schema_epoch: e_now,
        touched_types: vec![ids.author_type],
        touched_entities: Some(vec![]),
        touched_rel_kinds: vec![],
    };
    assert!(idx_clk.apply_cross_proc_summary(&schema_now, &cp_author).is_empty());
    assert!(idx_clk.notify_subdb_write_at_maint(11, e_now, 6, ids.idx_vec_slot, &[0x01]).is_empty());
    assert!(!idx_clk.register_after_read_with_maint(WatchId(200), 11, 5, e_now, &rs4, &schema_now).0);
    // Direct `apply_cross_proc_summary` touching Note invalidates WatchId(200).
    let cp_note = elmdb_engine::CrossProcCommitSummary {
        seq: 12,
        schema_epoch: e_now,
        touched_types: vec![ids.note_type],
        touched_entities: Some(vec![]),
        touched_rel_kinds: vec![],
    };
    assert_eq!(idx_clk.apply_cross_proc_summary(&schema_now, &cp_note), vec![WatchId(200)]);
    // Boundary checks on `check_ring_staleness` strict `>` comparisons (CrossProc, Local, SubDbWrite):
    // (a) CrossProc: seq 12 touched Note, seq 13 touched Author -> reader at seq 12 is NOT stale;
    //     after seq 14 touches Note, reader at seq 12 IS stale!
    let cp_author2 = elmdb_engine::CrossProcCommitSummary {
        seq: 13,
        schema_epoch: e_now,
        touched_types: vec![ids.author_type],
        touched_entities: Some(vec![]),
        touched_rel_kinds: vec![],
    };
    assert!(idx_clk.apply_cross_proc_summary(&schema_now, &cp_author2).is_empty());
    assert!(
        !idx_clk.register_after_read_with_maint(WatchId(200), 12, 6, e_now, &rs4, &schema_now).0,
        "reader at seq 12 after Note@12 and Author@13 must NOT be stale"
    );
    let cp_note2 = elmdb_engine::CrossProcCommitSummary {
        seq: 14,
        schema_epoch: e_now,
        touched_types: vec![ids.note_type],
        touched_entities: Some(vec![]),
        touched_rel_kinds: vec![],
    };
    assert_eq!(idx_clk.apply_cross_proc_summary(&schema_now, &cp_note2), vec![WatchId(200)]);
    assert!(
        idx_clk.register_after_read_with_maint(WatchId(200), 12, 6, e_now, &rs4, &schema_now).0,
        "reader at seq 12 with Note@14 in ring MUST be stale"
    );
    // (b) Local: commit S+1 touches Note, commit S+2 touches Author -> reader at S+1 is NOT stale!
    let mut idx_loc = InvalidationIndex::new_with_maint_seq(s4, e4, m4);
    let loc_note = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("n1".into()),
            fields: vec![("title".into(), CanonicalValue::String("N1-loc".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    let loc_auth = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("a1".into()),
            fields: vec![("name".into(), CanonicalValue::String("A1-loc".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    idx_loc.notify_commit(&engine.schema(), &loc_note);
    idx_loc.notify_commit(&engine.schema(), &loc_auth);
    assert!(
        !idx_loc
            .register_after_read_with_maint(WatchId(202), loc_note.seq, loc_note.maint_seq, e4, &rs4, &schema_now)
            .0,
        "reader at loc_note.seq with only unrelated Author commit after it must NOT be stale"
    );
    // (c) SubDbWrite: maint m+1 touches Vector slot, maint m+2 touches unrelated slot -> Vector reader at m+1 is NOT stale!
    let vec_plan2 = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            spec: ScoreSpec::VectorNear { index_id: ids.idx_vec, vector: vec![1.0, 0.0, 0.0, 0.0], top_k: 5 },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let (vs, vm, ve, _vo, vrs) = run_tracked_snapshot(&engine, &qe, &vec_plan2, ExecBudgets::default()).unwrap();
    let mut idx_sub = InvalidationIndex::new_with_maint_seq(vs, ve, vm);
    idx_sub.notify_subdb_write_at_maint(vs, ve, vm + 1, ids.idx_vec_slot, &[0x01]);
    idx_sub.notify_subdb_write_at_maint(vs, ve, vm + 2, ids.idx_vec_slot.wrapping_add(1), &[0x01]);
    assert!(
        !idx_sub.register_after_read_with_maint(WatchId(203), vs, vm + 1, ve, &vrs, &schema_now).0,
        "Vector reader at vm+1 with only unrelated slot write at vm+2 must NOT be stale"
    );

    // 2. `notify_full_rebuild_at_maint` stale-receipt guard & fresh-receipt invalidation.
    // Stale (maint_seq <= 6, seq <= 14, epoch <= e_now) is ignored:
    assert!(idx_clk.notify_full_rebuild_at_maint(14, e_now, 6).is_empty());
    // Fresh maint_seq > 6 (jumping to 10 != latest_maint_seq + 1) invalidates and records exact maint_seq 10:
    assert_eq!(idx_clk.notify_full_rebuild_at_maint(14, e_now, 10), vec![WatchId(200)]);
    assert_eq!(idx_clk.latest_maint_seq(), 10);
    // Same maint_seq == 10 with advanced seq > 14 invalidates:
    assert_eq!(idx_clk.notify_full_rebuild_at_maint(15, e_now, 10), vec![WatchId(200)]);
    // Same maint_seq == 10 & seq == 15 with advanced epoch > e_now invalidates:
    assert_eq!(idx_clk.notify_full_rebuild_at_maint(15, e_now + 1, 10), vec![WatchId(200)]);
    // `notify_schema_change` invalidates active watches and advances epoch:
    assert_eq!(idx_clk.notify_schema_change(15, e_now + 2), vec![WatchId(200)]);
    assert_eq!(idx_clk.latest_schema_epoch(), e_now + 2);

    // 3. `LiveQueryHub::notify_schema_change_with_maint` returns invalidated WatchIds.
    let hub_sc = LiveQueryHub::from_engine(&engine, 8).unwrap();
    let (w_sc, _) = hub_sc
        .register_query_watch(&engine, &qe, &note_plan, ExecBudgets::default(), WatchPolicy::default(), 0, 1, |_| None)
        .unwrap();
    let dirty_sc = hub_sc.notify_schema_change_with_maint(s4, e_now + 1, m4, 10);
    assert_eq!(dirty_sc, vec![w_sc]);
}

#[test]
fn batch_e_scheduler_abort_backoff_and_monotonic_snapshot_fence() {
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;

    // 1. E1: Sans-I/O WatchScheduler exponential failure backoff, idempotency, monotonic clock clamping,
    //    reset on finish_run, and non-starvation of healthy watches.
    let mut sched = WatchScheduler::new(8);
    let failing = WatchId(1);
    let healthy = WatchId(2);
    sched.register(
        failing,
        WatchPolicy { mode: WatchMode::Declarative, min_gap_ms: 0, adaptive_multiplier: 0 },
        1000,
        0,
        true,
        None,
    );
    sched.register(
        healthy,
        WatchPolicy { mode: WatchMode::Declarative, min_gap_ms: 16, adaptive_multiplier: 0 },
        1000,
        0,
        true,
        None,
    );

    sched.mark_dirty(&[failing], 1000);
    assert_eq!(sched.next_due(1000, 0).watch_id, Some(failing));
    assert!(sched.abort_run(failing, true, 1000));
    // 1st failure -> 50ms backoff (not immediately due at 1000ms!).
    assert_eq!(sched.next_due(1000, 0).watch_id, None);
    assert_eq!(sched.next_wake_delay_ms(1000), Some(elmdb_invalidation::DEFAULT_FAILURE_BACKOFF_BASE_MS));
    // Calling abort_run a second time while already !running is idempotent (must not double backoff to 100ms).
    assert!(sched.abort_run(failing, true, 1000));
    assert_eq!(sched.next_wake_delay_ms(1000), Some(50));

    // Healthy watch is NEVER starved while failing watch is backed off!
    sched.mark_dirty(&[healthy], 1016);
    assert_eq!(sched.next_due(1016, 0).watch_id, Some(healthy));
    assert!(sched.finish_run(healthy, 1016, 1, None));

    // 2nd failure at 1050 -> 100ms; 3rd at 1150 -> 200ms; exponential cap at 30_000ms.
    assert_eq!(sched.next_due(1050, 0).watch_id, Some(failing));
    assert!(sched.abort_run(failing, true, 1050));
    assert_eq!(sched.next_wake_delay_ms(1050), Some(100));

    assert_eq!(sched.next_due(1150, 0).watch_id, Some(failing));
    assert!(sched.abort_run(failing, true, 1150));
    assert_eq!(sched.next_wake_delay_ms(1150), Some(200));

    let mut t = 1350u64;
    for _ in 0..20 {
        assert_eq!(sched.next_due(t, 0).watch_id, Some(failing));
        assert!(sched.abort_run(failing, true, t));
        let delay = sched.next_wake_delay_ms(t).unwrap();
        assert!(delay <= elmdb_invalidation::DEFAULT_FAILURE_BACKOFF_MAX_MS);
        t += delay;
    }
    assert_eq!(sched.next_due(t, 0).watch_id, Some(failing));
    assert!(sched.abort_run(failing, true, t));
    assert_eq!(sched.next_wake_delay_ms(t), Some(elmdb_invalidation::DEFAULT_FAILURE_BACKOFF_MAX_MS));

    // Watch with min_gap_ms > 50ms keeps adaptive cooldown on 1st failure (never shortened to 50ms).
    let slow_gap = WatchId(3);
    sched.register(
        slow_gap,
        WatchPolicy { mode: WatchMode::Declarative, min_gap_ms: 200, adaptive_multiplier: 0 },
        t,
        0,
        true,
        None,
    );
    sched.mark_dirty(&[slow_gap], t);
    assert!(sched.begin_run(slow_gap));
    assert!(sched.abort_run(slow_gap, true, t));
    assert_eq!(sched.next_wake_delay_ms(t), Some(200));
    // abort_run with requeue_dirty = false leaves watch clean (not dirty).
    assert_eq!(sched.next_due(t + 200, 0).watch_id, Some(slow_gap));
    assert!(sched.abort_run(slow_gap, false, t + 200));
    assert!(!sched.tick(t + 1000).due_ids.contains(&slow_gap));
    sched.unregister(slow_gap);

    // Backward host clock step during abort_run must clamp `last_run_ms` monotonically (`t` vs `t - 500`).
    let prev_last_run = t;
    t += elmdb_invalidation::DEFAULT_FAILURE_BACKOFF_MAX_MS;
    assert_eq!(sched.next_due(t, 0).watch_id, Some(failing));
    assert!(sched.abort_run(failing, true, prev_last_run - 500));
    assert_eq!(
        sched.next_wake_delay_ms(prev_last_run - 500),
        Some(elmdb_invalidation::DEFAULT_FAILURE_BACKOFF_MAX_MS + 500)
    );

    // Successful finish_run resets consecutive_failures back to 0 (normal min_gap_ms = 0).
    assert_eq!(sched.next_due(t, 0).watch_id, Some(failing));
    assert!(sched.finish_run(failing, t, 0, None));
    sched.mark_dirty(&[failing], t);
    assert_eq!(sched.next_wake_delay_ms(t), Some(0));

    // 2. E3: Unit tests for monotonic `(seq, maint_seq, epoch)` snapshot fence, overlapping runs,
    //    and `dirty_since_ms` fairness preservation in WatchScheduler.
    assert!(sched.enter_rerun(WatchId(999)).is_none());
    assert!(!sched.can_accept_rerun(WatchId(999), 1, (1, 0, 0)));
    sched.seed_snapshot(failing, (5, 2, 1));
    let r1 = sched.enter_rerun(failing).unwrap();
    let r2 = sched.enter_rerun(failing).unwrap();
    assert!(r2 > r1);
    assert!(sched.can_accept_rerun(failing, r2, (5, 2, 1))); // equal snapshot, latest run accepted
    assert!(sched.can_accept_rerun(failing, r2, (6, 2, 1))); // advanced seq accepted
    assert!(!sched.can_accept_rerun(failing, r2, (4, 2, 1))); // stale seq rejected
    assert!(!sched.can_accept_rerun(failing, r2, (5, 1, 1))); // stale maint_seq rejected
    assert!(!sched.can_accept_rerun(failing, r2, (5, 2, 0))); // stale schema_epoch rejected
    // r2 finishes first at (6, 2, 1): r1 is still in flight so `running` MUST stay true!
    assert!(sched.finish_rerun(failing, r2, (6, 2, 1), false, t, 1, None));
    assert!(!sched.watches_settled(), "running must remain true while overlapping r1 is in flight");
    // Older r1 at EQUAL snapshot (6, 2, 1) is rejected because r1 < accepted_run_id (r2),
    // while r1 at a STRICTLY NEWER snapshot (7, 2, 1) and r2/r2+1 at equal snapshot are accepted!
    assert!(!sched.can_accept_rerun(failing, r1, (6, 2, 1)));
    assert!(!sched.can_accept_rerun(failing, r1, (5, 2, 1)));
    assert!(sched.can_accept_rerun(failing, r1, (7, 2, 1)));
    assert!(sched.can_accept_rerun(failing, r2, (6, 2, 1)));
    assert!(sched.can_accept_rerun(failing, r2 + 1, (6, 2, 1)));
    // With 2 active runs, abandoning one keeps running=true until the last is abandoned.
    let _r_extra = sched.enter_rerun(failing).unwrap();
    sched.abandon_rerun(failing);
    assert!(!sched.watches_settled());
    sched.abandon_rerun(failing);
    assert!(sched.watches_settled(), "abandoning last in-flight rerun clears running");

    // Overlapping abort_rerun preserves running while sibling is active, respects requeue_dirty=false,
    // and applies backoff when last aborts.
    let r3 = sched.enter_rerun(failing).unwrap();
    let _r4 = sched.enter_rerun(failing).unwrap();
    assert!(sched.abort_rerun(failing, false, t));
    assert!(!sched.watches_settled());
    assert!(sched.abort_rerun(failing, false, t));
    assert!(sched.watches_settled(), "abort_rerun(requeue_dirty=false) leaves watch clean");
    let _r4b = sched.enter_rerun(failing).unwrap();
    let _r4c = sched.enter_rerun(failing).unwrap();
    // Aborting one overlapping rerun with requeue_dirty=true must set dirty=true even while sibling is active.
    assert!(sched.abort_rerun(failing, true, t));
    assert!(!sched.watches_settled());
    sched.abandon_rerun(failing);
    assert!(!sched.watches_settled(), "dirty set by first abort_rerun must survive sibling abandon_rerun");
    // Now abort both runs to verify backoff applies once the last active rerun aborts.
    let _r4d = sched.enter_rerun(failing).unwrap();
    let _r4e = sched.enter_rerun(failing).unwrap();
    assert!(sched.abort_rerun(failing, true, t));
    assert!(!sched.watches_settled());
    assert!(sched.abort_rerun(failing, true, t));
    assert_eq!(sched.next_wake_delay_ms(t), Some(elmdb_invalidation::DEFAULT_FAILURE_BACKOFF_BASE_MS * 2));

    // Verify finish_rerun with stale=true sets dirty when clean AND preserves earlier dirty_since_ms when already dirty.
    let r5 = sched.enter_rerun(failing).unwrap();
    assert!(r5 > r3);
    assert!(sched.finish_rerun(failing, r5, (7, 2, 1), true, t + 50, 1, None));
    assert!(!sched.watches_settled());
    let r6 = sched.enter_rerun(failing).unwrap();
    sched.mark_dirty(&[failing], t + 60);
    sched.mark_dirty(&[healthy], t + 70);
    assert!(sched.finish_rerun(failing, r6, (8, 2, 1), true, t + 90, 1, None));
    assert_eq!(
        sched.tick(t + 200).due_ids,
        vec![failing, healthy],
        "finish_rerun(stale=true) must preserve earlier dirty_since_ms=t+60 ahead of healthy at t+70"
    );
    sched.stop_watches();
    assert!(sched.enter_rerun(failing).is_none());
    assert!(!sched.can_accept_rerun(failing, 99, (8, 3, 2)));

    // 3. E3: LiveQueryHub fail-fast pre-check & concurrent stale-snapshot fencing!
    let (engine, _reg, ids) = open_sim();
    let engine = Arc::new(engine);
    let qe = Arc::new(QueryEngine::default());
    let hub = Arc::new(LiveQueryHub::from_engine(&engine, 8).unwrap());
    let count_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
        terminal: TerminalSpec::Count,
    });

    engine
        .execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n1".into()),
            fields: vec![("title".into(), CanonicalValue::String("one".into()))],
        }])
        .unwrap();

    let (wid, init_count) = hub
        .register_query_watch_in_snapshot(
            &engine,
            &qe,
            ExecBudgets::default(),
            WatchPolicy::default(),
            10,
            1,
            |_ro, _schema, exec, _unres| {
                let out = exec(&count_plan)?;
                let QueryResult::Count(c) = out.result else { panic!("expected Count") };
                Ok((c, Some(hash_result_128(&c.to_le_bytes()))))
            },
        )
        .unwrap();
    assert_eq!(init_count, 1);

    // Fail-fast on unregistered watch BEFORE invoking compile_and_materialize callback!
    let calls = AtomicUsize::new(0);
    let err_missing = hub
        .rerun_query_watch_in_snapshot(
            WatchId(999_999),
            &engine,
            &qe,
            ExecBudgets::default(),
            20,
            1,
            |_ro, _schema, _exec, _unres| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok((0u64, None))
            },
        )
        .unwrap_err();
    assert_eq!(err_missing.code, elmdb_wire::ErrorCode::Closed);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    // Concurrent stale rerun vs newer rerun on the same watch_id:
    // Slow thread captures snapshot at seq S1 (count = 1), pauses at barrier while main thread
    // commits a 2nd Note (seq S2, count = 2) and completes a newer rerun. When slow thread finishes,
    // monotonic fence MUST reject stale S1 (should_deliver == false) without overwriting S2's ReadSet!
    let started = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let t_hub = Arc::clone(&hub);
    let t_eng = Arc::clone(&engine);
    let t_qe = Arc::clone(&qe);
    let t_plan = count_plan.clone();
    let t_started = Arc::clone(&started);
    let t_release = Arc::clone(&release);
    let slow = std::thread::spawn(move || {
        t_hub
            .rerun_query_watch_in_snapshot(
                wid,
                &t_eng,
                &t_qe,
                ExecBudgets::default(),
                30,
                1,
                |_ro, _schema, exec, _unres| {
                    let out = exec(&t_plan)?;
                    let QueryResult::Count(c) = out.result else { panic!("expected Count") };
                    t_started.wait();
                    t_release.wait();
                    Ok((c, Some(hash_result_128(&c.to_le_bytes()))))
                },
            )
            .unwrap()
    });

    started.wait();
    let sum2 = engine
        .execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n2".into()),
            fields: vec![("title".into(), CanonicalValue::String("two".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    hub.notify_commit(&engine.schema(), &sum2, 40);

    let (fast_count, fast_deliver) = hub
        .rerun_query_watch_in_snapshot(
            wid,
            &engine,
            &qe,
            ExecBudgets::default(),
            50,
            1,
            |_ro, _schema, exec, _unres| {
                let out = exec(&count_plan)?;
                let QueryResult::Count(c) = out.result else { panic!("expected Count") };
                Ok((c, Some(hash_result_128(&c.to_le_bytes()))))
            },
        )
        .unwrap();
    assert_eq!(fast_count, 2);
    assert!(fast_deliver);

    // Release slow stale thread (count = 1 at S1) -> MUST NOT deliver or overwrite S2's hash/ReadSet!
    release.wait();
    let (slow_count, slow_deliver) = slow.join().unwrap();
    assert_eq!(slow_count, 1);
    assert!(!slow_deliver, "stale concurrent snapshot S1 must be fenced off after S2 landed");
    assert!(hub.watches_settled());

    // Rerunning at S2 again still sees S2's hash (proving slow S1 did not clobber last_result_hash!).
    let (check_count, check_deliver) = hub
        .rerun_query_watch_in_snapshot(
            wid,
            &engine,
            &qe,
            ExecBudgets::default(),
            100,
            1,
            |_ro, _schema, exec, _unres| {
                let out = exec(&count_plan)?;
                let QueryResult::Count(c) = out.result else { panic!("expected Count") };
                Ok((c, Some(hash_result_128(&c.to_le_bytes()))))
            },
        )
        .unwrap();
    assert_eq!(check_count, 2);
    assert!(!check_deliver);

    // Unregister racing DURING an in-flight snapshot returns Ok((_, false)) and never resurrects!
    let u_started = Arc::new(Barrier::new(2));
    let u_release = Arc::new(Barrier::new(2));
    let u_hub = Arc::clone(&hub);
    let u_eng = Arc::clone(&engine);
    let u_qe = Arc::clone(&qe);
    let u_plan = count_plan.clone();
    let u_s = Arc::clone(&u_started);
    let u_r = Arc::clone(&u_release);
    let racing = std::thread::spawn(move || {
        u_hub
            .rerun_query_watch_in_snapshot(
                wid,
                &u_eng,
                &u_qe,
                ExecBudgets::default(),
                120,
                1,
                |_ro, _schema, exec, _unres| {
                    let out = exec(&u_plan)?;
                    let QueryResult::Count(c) = out.result else { panic!("expected Count") };
                    u_s.wait();
                    u_r.wait();
                    Ok((c, Some(hash_result_128(&c.to_le_bytes()))))
                },
            )
            .unwrap()
    });
    u_started.wait();
    assert!(hub.unregister_watch(wid));
    u_release.wait();
    let (_, race_deliver) = racing.join().unwrap();
    assert!(!race_deliver);
    // Also exercise `rerun_query_watch` (non-snapshot path) for changed delivery + Closed after unregister.
    let (wid_plain, _) = hub
        .register_query_watch(&engine, &qe, &count_plan, ExecBudgets::default(), WatchPolicy::default(), 130, 1, |o| {
            let QueryResult::Count(c) = o.result else { return None };
            Some(hash_result_128(&c.to_le_bytes()))
        })
        .unwrap();
    let sum3 = engine
        .execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n3".into()),
            fields: vec![("title".into(), CanonicalValue::String("three".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    hub.notify_commit(&engine.schema(), &sum3, 140);
    let (out_plain, del_plain) = hub
        .rerun_query_watch(wid_plain, &engine, &qe, &count_plan, ExecBudgets::default(), 150, 1, |o| {
            let QueryResult::Count(c) = o.result else { return None };
            Some(hash_result_128(&c.to_le_bytes()))
        })
        .unwrap();
    assert_eq!(out_plain.result, QueryResult::Count(3));
    assert!(del_plain);
    assert!(hub.unregister_watch(wid_plain));
    assert_eq!(
        hub.rerun_query_watch(wid_plain, &engine, &qe, &count_plan, ExecBudgets::default(), 160, 1, |_| None)
            .unwrap_err()
            .code,
        elmdb_wire::ErrorCode::Closed
    );
}

#[test]
fn pr268_batch2_cross_proc_schema_epoch_self_advances_and_dirties_sibling_watches() {
    let (engine, _reg, ids) = open_sim();
    let mut rs = ReadSet::new();
    rs.record_type_scan(ids.note_type);

    let schema_e1 = (*engine.schema()).clone();
    let e1 = schema_e1.epoch;
    let mut schema_e2 = schema_e1.clone();
    schema_e2.epoch = e1 + 1;
    let mut schema_e3 = schema_e1.clone();
    schema_e3.epoch = e1 + 2;

    // Register W1 and W2 at epoch e1.
    let mut idx = InvalidationIndex::new_with_maint_seq(1, e1, 0);
    assert_eq!(idx.register_after_read_with_maint(WatchId(1), 1, 0, e1, &rs, &schema_e1), (false, vec![]));
    assert_eq!(idx.register_after_read_with_maint(WatchId(2), 1, 0, e1, &rs, &schema_e1), (false, vec![]));

    // Single cross-proc bump (e1 -> e2): W1 reruns at e2 and sees current_schema at e2.
    // W1 settles cleanly (`stale = false`), latest_schema_epoch advances to e2, and sibling W2 is returned dirty!
    let (stale_w1, others_w1) = idx.register_after_read_with_maint(WatchId(1), 1, 1, schema_e2.epoch, &rs, &schema_e2);
    assert!(!stale_w1, "watch read at current cross-proc epoch must settle cleanly");
    assert_eq!(others_w1, vec![WatchId(2)], "sibling watch must be dirtied on cross-proc schema bump");
    assert_eq!(idx.stats().latest_schema_epoch, schema_e2.epoch);

    // Double cross-proc bump (e2 -> e3 -> e4) while W2 only read at e3 while current_schema is already e4:
    // W2 must stay stale for one final rerun at e4, latest_schema_epoch advances to e4, and W1 is dirtied!
    let mut schema_e4 = schema_e1.clone();
    schema_e4.epoch = e1 + 3;
    let (stale_w2, others_w2) = idx.register_after_read_with_maint(WatchId(2), 1, 2, schema_e3.epoch, &rs, &schema_e4);
    assert!(stale_w2, "watch read at e3 while current schema is e4 must rerun once more");
    assert_eq!(others_w2, vec![WatchId(1)]);
    assert_eq!(idx.stats().latest_schema_epoch, schema_e4.epoch);

    // Final rerun of W2 at e4 settles immediately with zero extra dirty siblings!
    let (stale_w2_final, others_w2_final) =
        idx.register_after_read_with_maint(WatchId(2), 1, 3, schema_e4.epoch, &rs, &schema_e4);
    assert!(!stale_w2_final);
    assert!(others_w2_final.is_empty());

    // End-to-end LiveQueryHub test (both `register_query_watch` / `rerun_query_watch` and `*_in_snapshot`):
    // Simulate a cross-proc `set_schema` by calling `engine.set_schema` WITHOUT notifying `hub`.
    let qe = QueryEngine::default();
    let hub = LiveQueryHub::from_engine(&engine, 8).unwrap();
    let count_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
        terminal: TerminalSpec::Count,
    });
    let (w_a, _) = hub
        .register_query_watch(&engine, &qe, &count_plan, ExecBudgets::default(), WatchPolicy::default(), 10, 1, |o| {
            let QueryResult::Count(c) = o.result else { return None };
            Some(hash_result_128(&c.to_le_bytes()))
        })
        .unwrap();
    let (w_b, _) = hub
        .register_query_watch_in_snapshot(
            &engine,
            &qe,
            ExecBudgets::default(),
            WatchPolicy::default(),
            10,
            1,
            |_ro, _sc, exec, _| {
                let out = exec(&count_plan)?;
                let QueryResult::Count(c) = out.result else { panic!("count") };
                Ok((c, Some(hash_result_128(&c.to_le_bytes()))))
            },
        )
        .unwrap();
    assert!(hub.watches_settled());

    // Bump schema epoch on engine WITHOUT notifying `hub` (exact cross-process `set_schema` state).
    let mut evolved = test_schema();
    evolved
        .types
        .get_mut("Note")
        .unwrap()
        .fields
        .insert("extra_col".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });
    engine.set_schema(&evolved).unwrap();

    // Registering a new watch `w_c` sees the new epoch -> settles cleanly AND marks `w_a` & `w_b` dirty!
    let (_w_c, _) = hub
        .register_query_watch(&engine, &qe, &count_plan, ExecBudgets::default(), WatchPolicy::default(), 20, 1, |o| {
            let QueryResult::Count(c) = o.result else { return None };
            Some(hash_result_128(&c.to_le_bytes()))
        })
        .unwrap();
    assert!(!hub.watches_settled(), "existing watches w_a and w_b must be marked dirty");

    // Rerun w_a and w_b -> both settle cleanly (zero infinite spin)!
    hub.rerun_query_watch(w_a, &engine, &qe, &count_plan, ExecBudgets::default(), 30, 1, |o| {
        let QueryResult::Count(c) = o.result else { return None };
        Some(hash_result_128(&c.to_le_bytes()))
    })
    .unwrap();
    hub.rerun_query_watch_in_snapshot(w_b, &engine, &qe, ExecBudgets::default(), 30, 1, |_ro, _sc, exec, _| {
        let out = exec(&count_plan)?;
        let QueryResult::Count(c) = out.result else { panic!("count") };
        Ok((c, Some(hash_result_128(&c.to_le_bytes()))))
    })
    .unwrap();
    assert!(hub.watches_settled(), "all watches must settle after rerunning against the new schema epoch");

    // PR #270 P1-3: Extra-field-only Patch/Unset and archived-field Unset MUST invalidate `PointWatch { all_fields: true }`
    // while NOT falsely invalidating a field-specific `PointWatch { fields: {title} }`!
    let mut with_extra = evolved.clone();
    with_extra.types.get_mut("Note").unwrap().additional_fields = true;
    engine.set_schema(&with_extra).unwrap();
    engine
        .execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("pw-note".into()),
            fields: vec![
                ("title".into(), CanonicalValue::String("hello".into())),
                ("extra_col".into(), CanonicalValue::String("to-archive".into())),
            ],
        }])
        .unwrap();
    let pw_key = engine.get_row("pw-note").unwrap().unwrap().key;
    // Archive `extra_col` so `pw-note` carries an archived declared field.
    let mut archived_schema = with_extra.clone();
    archived_schema.types.get_mut("Note").unwrap().fields.remove("extra_col");
    engine.set_schema(&archived_schema).unwrap();

    let all_fields_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![pw_key]))),
        terminal: TerminalSpec::Rows { fields: None },
    });
    let mut title_mask = elmdb_engine::FieldMask::empty();
    title_mask.insert(ids.title);
    let title_only_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![pw_key]))),
        terminal: TerminalSpec::Rows { fields: Some(title_mask) },
    });
    let (s_pw, m_pw, e_pw, _, rs_all) =
        run_tracked_snapshot(&engine, &qe, &all_fields_plan, ExecBudgets::default()).unwrap();
    let (_, _, _, _, rs_title) = run_tracked_snapshot(&engine, &qe, &title_only_plan, ExecBudgets::default()).unwrap();
    let mut pw_idx = InvalidationIndex::new_with_maint_seq(s_pw, e_pw, m_pw);
    assert_eq!(
        pw_idx.register_after_read_with_maint(WatchId(501), s_pw, m_pw, e_pw, &rs_all, &engine.schema()),
        (false, vec![])
    );
    assert_eq!(
        pw_idx.register_after_read_with_maint(WatchId(502), s_pw, m_pw, e_pw, &rs_title, &engine.schema()),
        (false, vec![])
    );

    // Extra-field-only Patch invalidates all_fields watch (501) ONLY, NOT title-only watch (502)!
    let sum_extra_patch = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("pw-note".into()),
            fields: vec![("dyn_k".into(), CanonicalValue::String("v1".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    assert_eq!(pw_idx.notify_commit(&engine.schema(), &sum_extra_patch), vec![WatchId(501)]);

    // No-op extra-field Patch with identical value does NOT invalidate either watch!
    let sum_extra_noop = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("pw-note".into()),
            fields: vec![("dyn_k".into(), CanonicalValue::String("v1".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    assert!(pw_idx.notify_commit(&engine.schema(), &sum_extra_noop).is_empty());

    // Extra-field-only Unset invalidates all_fields watch (501) ONLY!
    let sum_extra_unset = engine
        .execute_batch(&[WireOp::Unset {
            target: EntityTarget::ExternalId("pw-note".into()),
            fields: vec!["dyn_k".into()],
        }])
        .unwrap()
        .summary
        .unwrap();
    assert_eq!(pw_idx.notify_commit(&engine.schema(), &sum_extra_unset), vec![WatchId(501)]);

    // Archived-field-only Unset invalidates all_fields watch (501) ONLY!
    let sum_arch_unset = engine
        .execute_batch(&[WireOp::Unset {
            target: EntityTarget::ExternalId("pw-note".into()),
            fields: vec!["extra_col".into()],
        }])
        .unwrap()
        .summary
        .unwrap();
    assert_eq!(pw_idx.notify_commit(&engine.schema(), &sum_arch_unset), vec![WatchId(501)]);
}
