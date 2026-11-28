use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use elmdb_engine::{
    Cardinality, CompiledSchema, Engine, EntityKey, FieldId, FieldMask, IndexId, RawEndpoint, RawField, RawFieldKind,
    RawIndex, RawIndexKind, RawRelation, RawSchema, RawType, RelKindId, TypeId, read_packed_row, scan_adj_in_raw,
    scan_adj_out_raw,
};
use elmdb_lmdb::{LmdbConfig, LmdbStore};
use elmdb_projections::{BuildPolicy, ProjectionRegistry, rank};
use elmdb_query::{
    Cursor, EdgeDir, ExactSum, ExecBudgets, FilterExpr, FuseMethod, GraphAnalyzeSpec, LogicalNode, LogicalPlan,
    OrderDir, OrderSpec, QueryEngine, QueryReadRecorder, QueryResult, ScoreSpec, SourceSpec, TerminalSpec,
    TraverseSpec, extract_regex_trigrams, leapfrog_intersect,
};
use elmdb_sim::{SimClock, SimKvConfig, SimKvStore, SimVfs};
use elmdb_storage::{BootId, DurabilityMode, KvCursor, KvRead, KvStore, OsVfs, SystemClock};
use elmdb_vec::{VectorProjection, flush as flush_vec, set_entity_embeddings};
use elmdb_wire::{CanonicalValue, EntityTarget, ErrorCode, WireOp};
use tempfile::tempdir;

// ---------------------------------------------------------------------------
// Schema & engine test harness
// ---------------------------------------------------------------------------

fn test_schema() -> RawSchema {
    let mut note_fields = BTreeMap::new();
    note_fields.insert("title".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });
    note_fields.insert("body".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });
    note_fields.insert("score".into(), RawField { kind: RawFieldKind::Any, optional: true, nullable: true });
    note_fields.insert("tag".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });
    note_fields.insert("updated_at".into(), RawField { kind: RawFieldKind::Int64, optional: true, nullable: true });

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
        .insert("note_body_trigram".into(), RawIndex { kind: RawIndexKind::Trigram { fields: vec!["body".into()] } });
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
    relations.insert(
        "AUTHORED_BY".into(),
        RawRelation {
            cardinality: Cardinality::ManyToMany,
            acyclic: false,
            weighted: false,
            endpoints: vec![RawEndpoint { source: "Note".into(), target: "Author".into() }],
            declared_by: None,
        },
    );

    RawSchema { types, relations }
}

struct Ids {
    note_type: TypeId,
    title: FieldId,
    body: FieldId,
    score: FieldId,
    tag: FieldId,
    updated_at: FieldId,
    cites: RelKindId,
    authored_by: RelKindId,
    idx_text: IndexId,
    idx_vec: IndexId,
    idx_rank: IndexId,
}

fn resolve_ids(schema: &CompiledSchema) -> Ids {
    let note = schema.type_by_name("Note").unwrap();
    Ids {
        note_type: note.id,
        title: note.field_by_name("title").unwrap().id,
        body: note.field_by_name("body").unwrap().id,
        score: note.field_by_name("score").unwrap().id,
        tag: note.field_by_name("tag").unwrap().id,
        updated_at: note.field_by_name("updated_at").unwrap().id,
        cites: schema.relation_by_name("CITES").unwrap().id,
        authored_by: schema.relation_by_name("AUTHORED_BY").unwrap().id,
        idx_text: schema.indexes.iter().find(|i| i.name == "note_text").unwrap().id,
        idx_vec: schema.indexes.iter().find(|i| i.name == "note_vec").unwrap().id,
        idx_rank: schema.indexes.iter().find(|i| i.name == "note_rank").unwrap().id,
    }
}

type SimHarness = (Engine<SimKvStore, SimVfs, SimClock>, Arc<ProjectionRegistry<SimKvStore, SimVfs>>, Ids);

fn open_sim(policy: BuildPolicy) -> SimHarness {
    let store = SimKvStore::new(SimKvConfig::default());
    let vfs = SimVfs::new(BootId([1; 16]));
    let clock = SimClock::new(1_700_000_000_000);
    let reg = Arc::new(ProjectionRegistry::new(policy).with_external(Arc::new(VectorProjection)));
    let (engine, _) = Engine::open_with_projections(
        store,
        vfs,
        clock,
        PathBuf::from("/sim/query"),
        DurabilityMode::Strict,
        Arc::clone(&reg) as _,
    )
    .unwrap();
    engine.set_schema(&test_schema()).unwrap();
    let ids = resolve_ids(&engine.schema());
    (engine, reg, ids)
}

fn flush_all<S: KvStore, V: elmdb_storage::Vfs, C: elmdb_storage::Clock>(
    engine: &Engine<S, V, C>,
    reg: &ProjectionRegistry<S, V>,
    ids: &Ids,
) {
    while reg.drive_build_chunk(engine, 1024).unwrap() {}
    flush_vec(engine, ids.idx_vec).unwrap();
    rank::flush_rank(engine, ids.idx_rank).unwrap();
}

/// Simple full-scan oracle for evaluating `FilterExpr` directly from primary `entities`.
fn oracle_filter<S: KvStore, V: elmdb_storage::Vfs, C: elmdb_storage::Clock>(
    engine: &Engine<S, V, C>,
    type_id: TypeId,
    include_trashed: bool,
    expr: &FilterExpr,
) -> Vec<EntityKey> {
    let (txn, _schema) = engine.begin_read_pinned().unwrap();
    let dbs = engine.dbs();
    let mut all = Vec::new();
    let mut cur = txn.cursor(dbs.entities).unwrap();
    let mut item = cur.first().unwrap();
    while let Some((k, _)) = item {
        if k.len() == 8 {
            let ek = EntityKey(u64::from_ne_bytes(k.try_into().unwrap()));
            if let Some(row) = read_packed_row(&txn, &dbs, ek).unwrap()
                && row.header.type_id == type_id
                && (include_trashed || !row.header.is_trashed())
                && oracle_eval_expr(&txn, &dbs, ek, &row.fields, expr)
            {
                all.push(ek);
            }
        }
        item = cur.next().unwrap();
    }
    all.sort();
    all
}

fn oracle_num(v: &CanonicalValue) -> Option<f64> {
    match v {
        CanonicalValue::Int(i) => Some(*i as f64),
        CanonicalValue::Float(f) => Some(*f),
        _ => None,
    }
}

fn oracle_eq(a: &CanonicalValue, b: &CanonicalValue) -> bool {
    if a == b {
        return true;
    }
    if let (Some(fa), Some(fb)) = (oracle_num(a), oracle_num(b)) {
        return fa == fb;
    }
    false
}

fn oracle_cmp(a: &CanonicalValue, b: &CanonicalValue) -> Option<std::cmp::Ordering> {
    if let (Some(fa), Some(fb)) = (oracle_num(a), oracle_num(b)) {
        return fa.partial_cmp(&fb);
    }
    match (a, b) {
        (CanonicalValue::String(sa), CanonicalValue::String(sb)) => Some(sa.cmp(sb)),
        (CanonicalValue::Bool(ba), CanonicalValue::Bool(bb)) => Some(ba.cmp(bb)),
        _ => None,
    }
}

fn oracle_eval_expr<R: KvRead>(
    txn: &R,
    dbs: &elmdb_engine::SystemDbs,
    ek: EntityKey,
    row_fields: &BTreeMap<FieldId, CanonicalValue>,
    expr: &FilterExpr,
) -> bool {
    match expr {
        FilterExpr::Eq(f, v) => row_fields.get(f).is_some_and(|rv| oracle_eq(rv, v)),
        FilterExpr::Ne(f, v) => row_fields.get(f).is_none_or(|rv| !oracle_eq(rv, v)),
        FilterExpr::In(f, vs) => row_fields.get(f).is_some_and(|rv| vs.iter().any(|v| oracle_eq(rv, v))),
        FilterExpr::Range { field, low, high } => {
            let Some(rv) = row_fields.get(field) else { return false };
            if let Some((lv, inc)) = low {
                let Some(ord) = oracle_cmp(rv, lv) else { return false };
                if ord == std::cmp::Ordering::Less || (!inc && ord == std::cmp::Ordering::Equal) {
                    return false;
                }
            }
            if let Some((hv, inc)) = high {
                let Some(ord) = oracle_cmp(rv, hv) else { return false };
                if ord == std::cmp::Ordering::Greater || (!inc && ord == std::cmp::Ordering::Equal) {
                    return false;
                }
            }
            true
        }
        FilterExpr::StartsWith(f, p) => {
            matches!(row_fields.get(f), Some(CanonicalValue::String(s)) if s.starts_with(p.as_str()))
        }
        FilterExpr::Contains(f, p) => {
            matches!(row_fields.get(f), Some(CanonicalValue::String(s)) if s.to_lowercase().contains(&p.to_lowercase()))
        }
        FilterExpr::Regex(f, pat) => {
            let re = regex::Regex::new(pat).unwrap();
            matches!(row_fields.get(f), Some(CanonicalValue::String(s)) if re.is_match(s))
        }
        FilterExpr::IsNull(f) => matches!(row_fields.get(f), None | Some(CanonicalValue::Null)),
        FilterExpr::IsPresent(f) => !matches!(row_fields.get(f), None | Some(CanonicalValue::Null)),
        FilterExpr::HasLink { rel_kinds, dir, target } => {
            let mut edges = Vec::new();
            match dir {
                EdgeDir::Out => edges.extend(scan_adj_out_raw(txn, dbs, ek, rel_kinds).unwrap()),
                EdgeDir::In => edges.extend(scan_adj_in_raw(txn, dbs, ek, rel_kinds).unwrap()),
                EdgeDir::Both => {
                    edges.extend(scan_adj_out_raw(txn, dbs, ek, rel_kinds).unwrap());
                    edges.extend(scan_adj_in_raw(txn, dbs, ek, rel_kinds).unwrap());
                }
            }
            edges.into_iter().any(|(other, _, _)| target.is_none_or(|t| t == other))
        }
        FilterExpr::And(parts) => parts.iter().all(|p| oracle_eval_expr(txn, dbs, ek, row_fields, p)),
        FilterExpr::Or(parts) => parts.iter().any(|p| oracle_eval_expr(txn, dbs, ek, row_fields, p)),
        FilterExpr::Not(inner) => !oracle_eval_expr(txn, dbs, ek, row_fields, inner),
    }
}

fn cf(v: f64) -> CanonicalValue {
    CanonicalValue::Float(v)
}

// ---------------------------------------------------------------------------
// 1. Predicates, Index Pushdowns, Leapfrog, Long Strings, Unified Numerics
// ---------------------------------------------------------------------------

#[test]
fn predicates_match_oracle_with_value_trigram_regex_leapfrog_and_long_strings() {
    let (engine, reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    let long_tag = "x".repeat(600); // > 448 B forces TAG_LONG in Value index!
    let long_body = format!("{} needle-superstring {}", "a".repeat(500), "b".repeat(500));

    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n1".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("Alpha Rust".into())),
                    ("body".into(), CanonicalValue::String("Zero copy LMDB B+tree storage engine".into())),
                    ("score".into(), CanonicalValue::Int(5)),
                    ("tag".into(), CanonicalValue::String("systems".into())),
                    ("updated_at".into(), CanonicalValue::Int(1000)),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n2".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("Beta Search".into())),
                    ("body".into(), CanonicalValue::String("Apple orchard and BM25F WAND ranking".into())),
                    ("score".into(), cf(5.0)), // unified numeric == Int(5)!
                    ("tag".into(), CanonicalValue::String("systems".into())),
                    ("updated_at".into(), CanonicalValue::Int(2000)),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n3".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("Gamma Long".into())),
                    ("body".into(), CanonicalValue::String(long_body)),
                    ("score".into(), CanonicalValue::Int(12)),
                    ("tag".into(), CanonicalValue::String(long_tag.clone())),
                    ("updated_at".into(), CanonicalValue::Int(3000)),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n4".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("Delta Null".into())),
                    ("body".into(), CanonicalValue::String("apple pie recipe".into())),
                    ("updated_at".into(), CanonicalValue::Int(4000)),
                ],
            },
            WireOp::Create {
                type_name: "Author".into(),
                external_id: Some("a1".into()),
                fields: vec![("name".into(), CanonicalValue::String("Ada".into()))],
            },
            WireOp::Link {
                rel_kind: "AUTHORED_BY".into(),
                source: EntityTarget::ExternalId("n1".into()),
                target: EntityTarget::ExternalId("a1".into()),
                weight: None,
                info: None,
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("n2".into()),
                target: EntityTarget::ExternalId("n1".into()),
                weight: Some(2.0f64.to_bits()),
                info: None,
            },
        ])
        .unwrap();
    let ka1 = engine.get_row("a1").unwrap().unwrap().key;
    flush_all(&engine, &reg, &ids);

    let cases: Vec<FilterExpr> = vec![
        // Unified numeric: Float(5.0) must match both Int(5) and Float(5.0)!
        FilterExpr::Eq(ids.score, cf(5.0)),
        FilterExpr::Eq(ids.score, CanonicalValue::Int(5)),
        FilterExpr::Ne(ids.score, CanonicalValue::Int(5)),
        FilterExpr::In(ids.score, vec![CanonicalValue::Int(5), CanonicalValue::Int(12)]),
        FilterExpr::Range {
            field: ids.score,
            low: Some((CanonicalValue::Int(5), true)),
            high: Some((CanonicalValue::Int(12), false)),
        },
        FilterExpr::Range {
            field: ids.score,
            low: Some((cf(5.0), false)),
            high: Some((CanonicalValue::Int(12), true)),
        },
        // Long string (> 448 B) exact Eq + StartsWith on indexed field!
        FilterExpr::Eq(ids.tag, CanonicalValue::String(long_tag)),
        FilterExpr::StartsWith(ids.tag, "sys".into()),
        FilterExpr::StartsWith(ids.tag, "x".repeat(500)),
        // Trigram Contains + Regex HIR (with small character class `[aA]pple`)!
        FilterExpr::Contains(ids.body, "storage".into()),
        FilterExpr::Contains(ids.body, "needle-superstring".into()),
        FilterExpr::Regex(ids.body, "[aA]pple".into()),
        FilterExpr::Regex(ids.body, "BM25F.*WAND".into()),
        // Null / Present
        FilterExpr::IsNull(ids.tag),
        FilterExpr::IsPresent(ids.tag),
        // HasLink drive-from-smaller-side
        FilterExpr::HasLink { rel_kinds: vec![ids.authored_by], dir: EdgeDir::Out, target: Some(ka1) },
        FilterExpr::HasLink { rel_kinds: vec![ids.cites], dir: EdgeDir::In, target: None },
        // Multi-index AND -> Leapfrog intersect!
        FilterExpr::And(vec![
            FilterExpr::Eq(ids.tag, CanonicalValue::String("systems".into())),
            FilterExpr::Eq(ids.score, CanonicalValue::Int(5)),
        ]),
        // OR + NOT
        FilterExpr::Or(vec![
            FilterExpr::Eq(ids.tag, CanonicalValue::String("systems".into())),
            FilterExpr::IsNull(ids.tag),
        ]),
        FilterExpr::Not(Box::new(FilterExpr::Eq(ids.tag, CanonicalValue::String("systems".into())))),
    ];

    for expr in &cases {
        let plan = LogicalPlan::new(LogicalNode::Terminal {
            input: Box::new(LogicalNode::OrderSlice {
                input: Box::new(LogicalNode::Filter {
                    input: Box::new(LogicalNode::Source(SourceSpec::Type {
                        type_id: ids.note_type,
                        include_trashed: false,
                    })),
                    expr: expr.clone(),
                }),
                order: OrderSpec::EntityKey(OrderDir::Asc),
                after: None,
                limit: None,
            }),
            terminal: TerminalSpec::Ids,
        });
        let out = qe.execute(&engine, &plan, ExecBudgets::default()).unwrap();
        let expected = oracle_filter(&engine, ids.note_type, false, expr);
        assert_eq!(out.result, QueryResult::Ids(expected), "mismatch for expr: {expr:?}");
    }

    // Verify regex-HIR trigram extractor unit behavior (small class expansion + literal extraction)
    let trigs = extract_regex_trigrams("[aA]pple");
    assert!(!trigs.is_empty(), "expected trigrams extracted from [aA]pple");
    let trigs_star = extract_regex_trigrams(".*");
    assert!(trigs_star.is_empty());

    // Verify Spread-adaptive leapfrog_intersect against naive exact intersection
    let a: Vec<EntityKey> = (0..10_000).step_by(3).map(EntityKey).collect();
    let b: Vec<EntityKey> = vec![EntityKey(9), EntityKey(99), EntityKey(999), EntityKey(9999)];
    let c: Vec<EntityKey> = (0..10_000).step_by(9).map(EntityKey).collect();
    let got = leapfrog_intersect(vec![a, b, c]);
    assert_eq!(got, vec![EntityKey(9), EntityKey(99), EntityKey(999), EntityKey(9999)]);
}

// ---------------------------------------------------------------------------
// 2. Hybrid Scoring (BM25F, VectorNear/Like, RankIndex, FuzzyLabel, Recency, Fuse)
// ---------------------------------------------------------------------------

#[test]
fn scoring_bm25f_vector_rank_fuzzy_recency_and_hybrid_fusion() {
    let (engine, reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("doc_rust".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("Rust Embedded Database".into())),
                    ("body".into(), CanonicalValue::String("LMDB copy on write btree zero copy search".into())),
                    ("updated_at".into(), CanonicalValue::Int(1_700_000_000_000)),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("doc_graph".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("Graph Analytics".into())),
                    ("body".into(), CanonicalValue::String("PageRank Leiden community detection".into())),
                    ("updated_at".into(), CanonicalValue::Int(1_699_000_000_000)),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("doc_hybrid".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("Rust Search Graph".into())),
                    ("body".into(), CanonicalValue::String("Hybrid search combining BM25F vector and PageRank".into())),
                    ("updated_at".into(), CanonicalValue::Int(1_700_000_000_000)),
                ],
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("doc_rust".into()),
                target: EntityTarget::ExternalId("doc_hybrid".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("doc_graph".into()),
                target: EntityTarget::ExternalId("doc_hybrid".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
        ])
        .unwrap();

    let k_rust = engine.get_row("doc_rust").unwrap().unwrap().key;
    let k_graph = engine.get_row("doc_graph").unwrap().unwrap().key;
    let k_hybrid = engine.get_row("doc_hybrid").unwrap().unwrap().key;

    set_entity_embeddings(&engine, ids.idx_vec, k_rust, &[vec![1.0, 0.0, 0.0, 0.0]]).unwrap();
    set_entity_embeddings(&engine, ids.idx_vec, k_graph, &[vec![0.0, 1.0, 0.0, 0.0]]).unwrap();
    set_entity_embeddings(&engine, ids.idx_vec, k_hybrid, &[vec![0.9, 0.4, 0.0, 0.0]]).unwrap();
    flush_all(&engine, &reg, &ids);

    let src = || LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false });

    // BM25F phrase search
    let bm25_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(src()),
            spec: ScoreSpec::Bm25f {
                index_id: ids.idx_text,
                query: "zero copy".into(),
                require_all: true,
                prefix: false,
                phrase: Some(vec!["zero".into(), "copy".into()]),
                top_k: Some(5),
            },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(hits) = qe.execute(&engine, &bm25_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].key, k_rust);

    // VectorNear
    let vec_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(src()),
            spec: ScoreSpec::VectorNear { index_id: ids.idx_vec, vector: vec![1.0, 0.0, 0.0, 0.0], top_k: 2 },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(vhits) = qe.execute(&engine, &vec_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(vhits[0].key, k_rust);
    assert_eq!(vhits[1].key, k_hybrid);

    // VectorLike (excludes seed itself)
    let vlike_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(src()),
            spec: ScoreSpec::VectorLike { index_id: ids.idx_vec, seed: k_rust, top_k: 2 },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(vlhits) = qe.execute(&engine, &vlike_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert!(vlhits.iter().all(|h| h.key != k_rust));
    assert_eq!(vlhits[0].key, k_hybrid);

    // RankIndex (doc_hybrid cited by both others -> highest PageRank!)
    let rank_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::OrderSlice {
            input: Box::new(LogicalNode::Score {
                input: Box::new(src()),
                spec: ScoreSpec::RankIndex { index_id: ids.idx_rank },
            }),
            order: OrderSpec::Score(OrderDir::Desc),
            after: None,
            limit: Some(3),
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(rhits) = qe.execute(&engine, &rank_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(rhits[0].key, k_hybrid);

    // FuzzyLabel (Levenshtein distance <= 2 against title)
    let fuzzy_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(src()),
            spec: ScoreSpec::FuzzyLabel { field: ids.title, query: "Graph Analytiks".into(), max_distance: 2 },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(fhits) = qe.execute(&engine, &fuzzy_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(fhits.len(), 1);
    assert_eq!(fhits[0].key, k_graph);

    // RecencyDecay (newer timestamp gets higher score)
    let rec_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::OrderSlice {
            input: Box::new(LogicalNode::Score {
                input: Box::new(src()),
                spec: ScoreSpec::RecencyDecay {
                    field: ids.updated_at,
                    now_ms: 1_700_000_000_000,
                    half_life_ms: 500_000_000.0,
                },
            }),
            order: OrderSpec::Score(OrderDir::Desc),
            after: None,
            limit: None,
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(rechits) = qe.execute(&engine, &rec_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(rechits.last().unwrap().key, k_graph);

    // Hybrid Fuse: RRF default
    let fuse_rrf = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(src()),
            spec: ScoreSpec::Fuse {
                inputs: vec![
                    (
                        ScoreSpec::Bm25f {
                            index_id: ids.idx_text,
                            query: "rust search".into(),
                            require_all: false,
                            prefix: false,
                            phrase: None,
                            top_k: Some(10),
                        },
                        1.0,
                    ),
                    (ScoreSpec::RankIndex { index_id: ids.idx_rank }, 1.0),
                ],
                method: FuseMethod::default(),
            },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(rrf_hits) = qe.execute(&engine, &fuse_rrf, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(rrf_hits[0].key, k_hybrid, "doc_hybrid should win RRF fusion");

    // Hybrid Fuse: LinearNormalized with constant-signal guard (max == min -> no NaN!)
    let fuse_lin = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k_rust, k_hybrid]))),
            spec: ScoreSpec::Fuse {
                inputs: vec![
                    (
                        ScoreSpec::RecencyDecay {
                            field: ids.updated_at,
                            now_ms: 1_700_000_000_000,
                            half_life_ms: 1_000.0,
                        },
                        0.5,
                    ),
                    (ScoreSpec::RankIndex { index_id: ids.idx_rank }, 0.5),
                ],
                method: FuseMethod::LinearNormalized,
            },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(lin_hits) = qe.execute(&engine, &fuse_lin, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(lin_hits.len(), 2);
    assert!(lin_hits.iter().all(|h| h.score.is_finite()), "LinearNormalized must never emit NaN on constant signals");
    assert_eq!(lin_hits[0].key, k_hybrid);
}

// ---------------------------------------------------------------------------
// 3. Traversals (Hops, ShortestPath) & ALL 7 Deterministic Graph Analytics
// ---------------------------------------------------------------------------

#[test]
fn traversals_and_all_seven_graph_analytics() {
    let (engine, reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    // Component A: 0 -> 1 -> 2 -> 0 (3-cycle SCC), 0 -> 3, 1 -> 3 (0 and 1 share neighbor 3)
    // Component B: 4 -> 5 (DAG edge)
    let mut ops = Vec::new();
    for i in 0..6 {
        ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("n{i}")),
            fields: vec![("title".into(), CanonicalValue::String(format!("Node {i}")))],
        });
    }
    for &(u, v) in &[(0, 1), (1, 2), (2, 0), (0, 3), (1, 3), (4, 5)] {
        ops.push(WireOp::Link {
            rel_kind: "CITES".into(),
            source: EntityTarget::ExternalId(format!("n{u}")),
            target: EntityTarget::ExternalId(format!("n{v}")),
            weight: Some(1.0f64.to_bits()),
            info: None,
        });
    }
    engine.execute_batch(&ops).unwrap();
    let ks: Vec<EntityKey> = (0..6).map(|i| engine.get_row(&format!("n{i}")).unwrap().unwrap().key).collect();
    flush_all(&engine, &reg, &ids);

    // Traverse: Hops (1..=2 out from ks[0]) -> {ks[1], ks[3], ks[2]}
    let hops_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Traverse {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![ks[0]]))),
            spec: TraverseSpec::Hops { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, min_hops: 1, max_hops: 2 },
        }),
        terminal: TerminalSpec::Ids,
    });
    let QueryResult::Ids(hops) = qe.execute(&engine, &hops_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Ids");
    };
    assert_eq!(hops.into_iter().collect::<BTreeSet<_>>(), BTreeSet::from([ks[1], ks[2], ks[3]]));

    // Traverse: ShortestPath from ks[0] to ks[2] -> [ks[0], ks[1], ks[2]]
    let sp_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Traverse {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![ks[0]]))),
            spec: TraverseSpec::ShortestPath {
                rel_kinds: vec![ids.cites],
                dir: EdgeDir::Out,
                target: ks[2],
                max_hops: 5,
            },
        }),
        terminal: TerminalSpec::Ids,
    });
    let QueryResult::Ids(sp) = qe.execute(&engine, &sp_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Ids");
    };
    assert_eq!(sp, vec![ks[0], ks[1], ks[2]]);

    let all_src = || LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false });

    // 1. WeakComponents -> 2 components: {0,1,2,3} and {4,5}
    let wcc_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(all_src()),
        spec: GraphAnalyzeSpec::WeakComponents { rel_kinds: vec![ids.cites] },
    });
    let QueryResult::GraphGroups(wcc) = qe.execute(&engine, &wcc_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected GraphGroups");
    };
    assert_eq!(wcc, vec![vec![ks[0], ks[1], ks[2], ks[3]], vec![ks[4], ks[5]]]);

    // 2. SCC -> {0,1,2} plus singletons {3}, {4}, {5}
    let scc_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(all_src()),
        spec: GraphAnalyzeSpec::Scc { rel_kinds: vec![ids.cites] },
    });
    let QueryResult::GraphGroups(sccs) = qe.execute(&engine, &scc_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected GraphGroups");
    };
    assert_eq!(sccs, vec![vec![ks[0], ks[1], ks[2]], vec![ks[3]], vec![ks[4]], vec![ks[5]]]);

    // 3. TopoSort over SCC condensation -> 1 cyclic SCC ({0,1,2}), all 6 nodes ordered with {0,1,2} before 3 and 4 before 5
    let topo_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(all_src()),
        spec: GraphAnalyzeSpec::TopoSort { rel_kinds: vec![ids.cites] },
    });
    let QueryResult::GraphTopo(topo) = qe.execute(&engine, &topo_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected GraphTopo");
    };
    assert_eq!(topo.cycles, 1);
    assert_eq!(topo.order.len(), 6);
    let pos0 = topo.order.iter().position(|k| *k == ks[0]).unwrap();
    let pos3 = topo.order.iter().position(|k| *k == ks[3]).unwrap();
    let pos4 = topo.order.iter().position(|k| *k == ks[4]).unwrap();
    let pos5 = topo.order.iter().position(|k| *k == ks[5]).unwrap();
    assert!(pos0 < pos3);
    assert!(pos4 < pos5);

    // 4. PPR seeded at ks[0] -> high mass on component A, zero on component B
    let ppr_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(all_src()),
        spec: GraphAnalyzeSpec::Ppr {
            rel_kinds: vec![ids.cites],
            weighted: false,
            seeds: vec![(ks[0], 1.0)],
            damping: 0.85,
            iterations: 60,
            tolerance: 1e-8,
        },
    });
    let QueryResult::GraphPpr(ppr) = qe.execute(&engine, &ppr_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected GraphPpr");
    };
    assert!(ppr.converged);
    let score_map: BTreeMap<EntityKey, f64> = ppr.scores.iter().copied().collect();
    assert!(score_map[&ks[0]] > 0.1);
    assert!(score_map[&ks[4]] < 1e-9);

    // 5. Leiden communities: strictly deterministic across repeated runs
    let leiden_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(all_src()),
        spec: GraphAnalyzeSpec::Leiden {
            rel_kinds: vec![ids.cites],
            weighted: false,
            resolution: 1.0,
            seed: 12345,
            iterations: 5,
        },
    });
    let QueryResult::GraphGroups(l1) = qe.execute(&engine, &leiden_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected GraphGroups");
    };
    let QueryResult::GraphGroups(l2) = qe.execute(&engine, &leiden_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected GraphGroups");
    };
    assert_eq!(l1, l2, "Leiden must be bit-for-bit deterministic for a given seed");
    assert!(l1.len() >= 2);

    // 6. Jaccard similarity from ks[0] (out neighbors {1,3}; ks[1] has {2,3} -> shared {3}, union {1,2,3} => 1/3)
    let jac_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(all_src()),
        spec: GraphAnalyzeSpec::Jaccard { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, seed: ks[0], top_k: 5 },
    });
    let QueryResult::Hits(jac) = qe.execute(&engine, &jac_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(jac.len(), 1);
    assert_eq!(jac[0].key, ks[1]);
    assert!((jac[0].score - (1.0 / 3.0)).abs() < 1e-9);

    // 7. AdamicAdar similarity from ks[0] -> ks[1] via shared neighbor ks[3]
    let aa_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(all_src()),
        spec: GraphAnalyzeSpec::AdamicAdar { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, seed: ks[0], top_k: 5 },
    });
    let QueryResult::Hits(aa) = qe.execute(&engine, &aa_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(aa.len(), 1);
    assert_eq!(aa[0].key, ks[1]);
    assert!(aa[0].score > 0.0 && aa[0].score.is_finite());
}

// ---------------------------------------------------------------------------
// 4. All 9 Terminals, Shewchuk ExactSum, O(1) Count Pushdown, ICU Order & Cursors
// ---------------------------------------------------------------------------

#[test]
fn terminals_count_pushdown_shewchuk_sum_icu_order_and_cursor_pagination() {
    let (engine, reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    // Shewchuk classic catastrophic cancellation test: 1e16 + 1.0 - 1e16 == 1.0!
    // ICU locale ordering: "éclair" sorts between "delta" and "foxtrot" in en-US (unlike UTF-8 byte order!)
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("r1".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("foxtrot".into())),
                    ("score".into(), cf(1e16)),
                    ("tag".into(), CanonicalValue::String("g1".into())),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("r2".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("éclair".into())),
                    ("score".into(), cf(1.0)),
                    ("tag".into(), CanonicalValue::String("g1".into())),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("r3".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("delta".into())),
                    ("score".into(), cf(-1e16)),
                    ("tag".into(), CanonicalValue::String("g2".into())),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("r4".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("alpha".into())),
                    ("score".into(), CanonicalValue::Int(0)),
                    ("tag".into(), CanonicalValue::String("g1".into())),
                ],
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("r1".into()),
                target: EntityTarget::ExternalId("r2".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("r1".into()),
                target: EntityTarget::ExternalId("r3".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
        ])
        .unwrap();
    let ks: Vec<EntityKey> =
        ["r1", "r2", "r3", "r4"].iter().map(|id| engine.get_row(id).unwrap().unwrap().key).collect();
    flush_all(&engine, &reg, &ids);

    let src = || LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false });

    // O(1) Count pushdown on `tag == "g1"` -> must NOT scan any rows!
    let count_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(src()),
            expr: FilterExpr::Eq(ids.tag, CanonicalValue::String("g1".into())),
        }),
        terminal: TerminalSpec::Count,
    });
    let count_out = qe.execute(&engine, &count_plan, ExecBudgets::default()).unwrap();
    assert_eq!(count_out.result, QueryResult::Count(3));
    assert!(count_out.explain.count_pushdown);
    assert_eq!(count_out.explain.rows_scanned, 0);

    // Exists early stop
    let exists_plan =
        LogicalPlan::new(LogicalNode::Terminal { input: Box::new(src()), terminal: TerminalSpec::Exists });
    assert_eq!(qe.execute(&engine, &exists_plan, ExecBudgets::default()).unwrap().result, QueryResult::Exists(true));

    // Sum: Shewchuk exact sum!
    let sum_plan =
        LogicalPlan::new(LogicalNode::Terminal { input: Box::new(src()), terminal: TerminalSpec::Sum(ids.score) });
    let QueryResult::Sum(total) = qe.execute(&engine, &sum_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Sum");
    };
    assert_eq!(total, 1.0, "Shewchuk exact sum must preserve 1.0 across ±1e16 cancellation");

    let mut es = ExactSum::default();
    for x in [1e16, 1.0, -1e16, 0.25] {
        es.add(x);
    }
    assert_eq!(es.total(), 1.25);

    // MinMax
    let mm_plan =
        LogicalPlan::new(LogicalNode::Terminal { input: Box::new(src()), terminal: TerminalSpec::MinMax(ids.score) });
    let QueryResult::MinMax { min, max } = qe.execute(&engine, &mm_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected MinMax");
    };
    assert_eq!(min, Some(cf(-1e16)));
    assert_eq!(max, Some(cf(1e16)));

    // GroupBy counts_only & full
    let gb_counts_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(src()),
        terminal: TerminalSpec::GroupBy { field: ids.tag, counts_only: true },
    });
    let QueryResult::GroupCounts(gc) = qe.execute(&engine, &gb_counts_plan, ExecBudgets::default()).unwrap().result
    else {
        panic!("expected GroupCounts");
    };
    assert_eq!(gc, vec![(CanonicalValue::String("g1".into()), 3), (CanonicalValue::String("g2".into()), 1),]);

    let gb_full_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(src()),
        terminal: TerminalSpec::GroupBy { field: ids.tag, counts_only: false },
    });
    let QueryResult::Groups(groups) = qe.execute(&engine, &gb_full_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Groups");
    };
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].1.len(), 3);

    // Degrees
    let deg_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::OrderSlice {
            input: Box::new(src()),
            order: OrderSpec::EntityKey(OrderDir::Asc),
            after: None,
            limit: None,
        }),
        terminal: TerminalSpec::Degrees { rel_kinds: vec![ids.cites], dir: EdgeDir::Out },
    });
    let QueryResult::Degrees(degs) = qe.execute(&engine, &deg_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Degrees");
    };
    assert_eq!(degs[0], (ks[0], 2));
    assert_eq!(degs[1], (ks[1], 0));

    // Projected Rows with FieldMask (only `title`)
    let mut fm = FieldMask::empty();
    fm.insert(ids.title);
    let rows_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::OrderSlice {
            input: Box::new(src()),
            order: OrderSpec::Field { field: ids.title, dir: OrderDir::Asc, locale: Some("en-US".into()) },
            after: None,
            limit: Some(2),
        }),
        terminal: TerminalSpec::Rows { fields: Some(fm.clone()) },
    });
    let page1_out = qe.execute(&engine, &rows_plan, ExecBudgets::default()).unwrap();
    assert_eq!(page1_out.explain.order_pushdown.as_deref(), Some("note_title_order"));
    let QueryResult::Rows(page1) = page1_out.result else { panic!("expected Rows") };
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].fields.get(&ids.title), Some(&CanonicalValue::String("alpha".into())));
    assert_eq!(page1[1].fields.get(&ids.title), Some(&CanonicalValue::String("delta".into())));
    assert!(!page1[0].fields.contains_key(&ids.tag), "FieldMask projection must exclude unrequested fields");

    // Cursor pagination page 2 (must return "éclair" BEFORE "foxtrot" in en-US locale!)
    let cursor = Cursor::Field { value: page1[1].fields[&ids.title].clone(), key: page1[1].key };
    let page2_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::OrderSlice {
            input: Box::new(src()),
            order: OrderSpec::Field { field: ids.title, dir: OrderDir::Asc, locale: Some("en-US".into()) },
            after: Some(cursor),
            limit: Some(2),
        }),
        terminal: TerminalSpec::Rows { fields: Some(fm) },
    });
    let QueryResult::Rows(page2) = qe.execute(&engine, &page2_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Rows");
    };
    assert_eq!(page2.len(), 2);
    assert_eq!(page2[0].fields.get(&ids.title), Some(&CanonicalValue::String("éclair".into())));
    assert_eq!(page2[1].fields.get(&ids.title), Some(&CanonicalValue::String("foxtrot".into())));
}

// ---------------------------------------------------------------------------
// 5. Plan Cache, BuildState::Building Fallback, Budgets & ReadRecorder
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CountingRecorder {
    point_entities: usize,
    indexed_eqs: usize,
    type_scans: usize,
    edge_adjs: usize,
}

impl QueryReadRecorder for CountingRecorder {
    fn record_point_entity(&mut self, _key: EntityKey, _fields: &FieldMask) {
        self.point_entities += 1;
    }
    fn record_indexed_eq(&mut self, _index_id: IndexId, _value_hash: u64) {
        self.indexed_eqs += 1;
    }
    fn record_edge_adj(&mut self, _entity: EntityKey, _rel_kinds: &[RelKindId], _dir: EdgeDir) {
        self.edge_adjs += 1;
    }
    fn record_type_scan(&mut self, _type_id: TypeId) {
        self.type_scans += 1;
    }
}

#[test]
fn plan_cache_shape_normalization_and_schema_epoch_invalidation() {
    let (engine, reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("a".into()),
                fields: vec![("tag".into(), CanonicalValue::String("t1".into()))],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("b".into()),
                fields: vec![("tag".into(), CanonicalValue::String("t2".into()))],
            },
        ])
        .unwrap();
    flush_all(&engine, &reg, &ids);

    let make_plan = |tag: &str| {
        LogicalPlan::new(LogicalNode::Terminal {
            input: Box::new(LogicalNode::Filter {
                input: Box::new(LogicalNode::Source(SourceSpec::Type {
                    type_id: ids.note_type,
                    include_trashed: false,
                })),
                expr: FilterExpr::Eq(ids.tag, CanonicalValue::String(tag.into())),
            }),
            terminal: TerminalSpec::Count,
        })
    };

    let mut rec = CountingRecorder::default();
    let o1 = qe.execute_tracked(&engine, &make_plan("t1"), ExecBudgets::default(), &mut rec).unwrap();
    assert!(!o1.explain.plan_cache_hit);
    assert_eq!(rec.indexed_eqs, 1);

    // Different literal ("t2"), identical normalized shape -> cache HIT!
    let o2 = qe.execute_tracked(&engine, &make_plan("t2"), ExecBudgets::default(), &mut rec).unwrap();
    assert!(o2.explain.plan_cache_hit);
    assert_eq!(qe.cache_stats().hits, 1);
    assert_eq!(qe.cache_stats().misses, 1);

    // Structural difference (`Null` vs non-null literal in Eq) -> cache MISS!
    let null_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            expr: FilterExpr::Eq(ids.tag, CanonicalValue::Null),
        }),
        terminal: TerminalSpec::Count,
    });
    let o3 = qe.execute_tracked(&engine, &null_plan, ExecBudgets::default(), &mut rec).unwrap();
    assert!(!o3.explain.plan_cache_hit);

    // Schema epoch advance clears plan cache cleanly!
    let mut s2 = test_schema();
    s2.relations.insert(
        "EXTRA".into(),
        RawRelation {
            cardinality: Cardinality::ManyToMany,
            acyclic: false,
            weighted: false,
            endpoints: vec![RawEndpoint { source: "Note".into(), target: "Note".into() }],
            declared_by: None,
        },
    );
    engine.set_schema(&s2).unwrap();
    let o4 = qe.execute_tracked(&engine, &make_plan("t1"), ExecBudgets::default(), &mut rec).unwrap();
    assert!(!o4.explain.plan_cache_hit, "schema_epoch bump must invalidate cached plans");
}

#[test]
fn building_index_falls_back_safely_to_primary_scan() {
    // Use BackgroundChunked so newly added indexes remain `BuildState::Building` until chunks are driven!
    let (engine, _reg, ids) = open_sim(BuildPolicy::DeferredWatermark);
    let qe = QueryEngine::default();

    let mut ops = Vec::new();
    for i in 0..10 {
        ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("n{i}")),
            fields: vec![
                ("title".into(), CanonicalValue::String(format!("title_{i}"))),
                ("tag".into(), CanonicalValue::String("common".into())),
            ],
        });
    }
    engine.execute_batch(&ops).unwrap();

    // Add a brand-new Value index on `title` without driving its build chunk!
    let mut s2 = test_schema();
    s2.types.get_mut("Note").unwrap().indexes.insert(
        "note_title_val".into(),
        RawIndex { kind: RawIndexKind::Value { fields: vec!["title".into()], unique: false } },
    );
    engine.set_schema(&s2).unwrap();

    let expr = FilterExpr::Eq(ids.title, CanonicalValue::String("title_7".into()));
    let plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            expr: expr.clone(),
        }),
        terminal: TerminalSpec::Ids,
    });
    let out = qe.execute(&engine, &plan, ExecBudgets::default()).unwrap();
    let expected = oracle_filter(&engine, ids.note_type, false, &expr);
    assert_eq!(out.result, QueryResult::Ids(expected));
    assert!(!out.explain.chosen_indexes.contains(&"note_title_val".to_string()));
}

#[test]
fn budgets_enforce_heap_traversal_and_cancellation() {
    let (engine, reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    let mut ops = Vec::new();
    for i in 0..50 {
        ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("k{i}")),
            fields: vec![("title".into(), CanonicalValue::String(format!("Item {i}")))],
        });
    }
    for i in 0..49 {
        ops.push(WireOp::Link {
            rel_kind: "CITES".into(),
            source: EntityTarget::ExternalId(format!("k{i}")),
            target: EntityTarget::ExternalId(format!("k{}", i + 1)),
            weight: Some(1.0f64.to_bits()),
            info: None,
        });
    }
    engine.execute_batch(&ops).unwrap();
    let k0 = engine.get_row("k0").unwrap().unwrap().key;
    flush_all(&engine, &reg, &ids);

    // 1. Cancellation
    let cancel = AtomicBool::new(true);
    let plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
        terminal: TerminalSpec::Ids,
    });
    let err = qe.execute(&engine, &plan, ExecBudgets { cancel: Some(&cancel), ..Default::default() }).unwrap_err();
    assert_eq!(err.code, ErrorCode::Cancelled);

    // 2. Heap budget exceeded
    let err_heap = qe.execute(&engine, &plan, ExecBudgets { max_heap_bytes: 32, ..Default::default() }).unwrap_err();
    assert_eq!(err_heap.code, ErrorCode::ResourceLimit);

    // 3. Traversal visited budget exceeded
    let trav_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Traverse {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k0]))),
            spec: TraverseSpec::Hops { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, min_hops: 1, max_hops: 50 },
        }),
        terminal: TerminalSpec::Ids,
    });
    let err_trav =
        qe.execute(&engine, &trav_plan, ExecBudgets { max_traversal_visited: 5, ..Default::default() }).unwrap_err();
    assert_eq!(err_trav.code, ErrorCode::ResourceLimit);
}

// ---------------------------------------------------------------------------
// 6. Live LMDB End-to-End Verification
// ---------------------------------------------------------------------------

#[test]
fn live_lmdb_end_to_end_query_suite() {
    let dir = tempdir().unwrap();
    let db_dir = dir.path().join("db");
    let store = LmdbStore::open(&db_dir, LmdbConfig::default()).unwrap();
    let reg =
        Arc::new(ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange).with_external(Arc::new(VectorProjection)));
    let (engine, _) = Engine::open_with_projections(
        store,
        OsVfs::new(),
        SystemClock::default(),
        db_dir,
        DurabilityMode::Strict,
        Arc::clone(&reg) as _,
    )
    .unwrap();
    engine.set_schema(&test_schema()).unwrap();
    let ids = resolve_ids(&engine.schema());

    let mut ops = Vec::new();
    for i in 0..20 {
        ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("live_{i}")),
            fields: vec![
                ("title".into(), CanonicalValue::String(format!("Document {i:02}"))),
                ("body".into(), CanonicalValue::String(format!("Live LMDB query engine test body {i}"))),
                ("score".into(), CanonicalValue::Int(i as i64)),
                ("tag".into(), CanonicalValue::String(if i % 2 == 0 { "even" } else { "odd" }.into())),
            ],
        });
    }
    engine.execute_batch(&ops).unwrap();
    let ks: Vec<EntityKey> = (0..20).map(|i| engine.get_row(&format!("live_{i}")).unwrap().unwrap().key).collect();

    for (i, &k) in ks.iter().enumerate() {
        set_entity_embeddings(&engine, ids.idx_vec, k, &[vec![(i as f32) + 1.0, 1.0, 0.0, 0.0]]).unwrap();
    }
    flush_all(&engine, &reg, &ids);

    let qe = QueryEngine::default();
    let plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            expr: FilterExpr::And(vec![
                FilterExpr::Eq(ids.tag, CanonicalValue::String("even".into())),
                FilterExpr::Range {
                    field: ids.score,
                    low: Some((CanonicalValue::Int(4), true)),
                    high: Some((CanonicalValue::Int(14), true)),
                },
            ]),
        }),
        terminal: TerminalSpec::Ids,
    });
    let out = qe.execute(&engine, &plan, ExecBudgets::default()).unwrap();
    let expected = vec![ks[4], ks[6], ks[8], ks[10], ks[12], ks[14]];
    assert_eq!(out.result, QueryResult::Ids(expected));
}

// ---------------------------------------------------------------------------
// 7. Review Regression Tests (P0-1..P0-8, P1-1..P1-10, P2-1..P2-5)
// ---------------------------------------------------------------------------

#[test]
fn review_regression_compound_value_count_cache_and_cross_type_ranges() {
    // Schema with ONLY a compound Value index `["tag", "score"]` (no single-field index on `tag`)!
    let mut s = test_schema();
    s.types.get_mut("Note").unwrap().indexes.remove("note_tag");
    s.types.get_mut("Note").unwrap().indexes.insert(
        "note_tag_score".into(),
        RawIndex { kind: RawIndexKind::Value { fields: vec!["tag".into(), "score".into()], unique: false } },
    );

    let store = SimKvStore::new(SimKvConfig::default());
    let vfs = SimVfs::new(BootId([2; 16]));
    let clock = SimClock::new(1_700_000_000_000);
    let reg =
        Arc::new(ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange).with_external(Arc::new(VectorProjection)));
    let (engine, _) = Engine::open_with_projections(
        store,
        vfs,
        clock,
        PathBuf::from("/sim/compound"),
        DurabilityMode::Strict,
        Arc::clone(&reg) as _,
    )
    .unwrap();
    engine.set_schema(&s).unwrap();
    let ids = resolve_ids(&engine.schema());
    let qe = QueryEngine::default();

    let nul_heavy = "\0".repeat(300); // len == 300 <= 448, but escaped > 502 B -> TAG_LONG!
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("c1".into()),
                fields: vec![
                    ("tag".into(), CanonicalValue::String("sys".into())),
                    ("score".into(), CanonicalValue::Int(5)),
                ],
            },
            // Trailing `score` omitted -> stored in TAG_WITHOUT of compound index!
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("c2".into()),
                fields: vec![("tag".into(), CanonicalValue::String("sys".into()))],
            },
            // Trailing `score` is Array -> stored in TAG_UNORDERED of compound index!
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("c3".into()),
                fields: vec![
                    ("tag".into(), CanonicalValue::String("sys".into())),
                    ("score".into(), CanonicalValue::Array(vec![CanonicalValue::Int(1)])),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("c4".into()),
                fields: vec![
                    ("tag".into(), CanonicalValue::String(nul_heavy.clone())),
                    ("score".into(), CanonicalValue::Int(9)),
                ],
            },
        ])
        .unwrap();
    flush_all(&engine, &reg, &ids);

    let src = || LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false });

    // P0-1(1) & P0-2: Count & Eq & In & StartsWith on leading field of compound index must find all 3 ("sys")!
    let count_sys = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(src()),
            expr: FilterExpr::Eq(ids.tag, CanonicalValue::String("sys".into())),
        }),
        terminal: TerminalSpec::Count,
    });
    assert_eq!(qe.execute(&engine, &count_sys, ExecBudgets::default()).unwrap().result, QueryResult::Count(3));

    for expr in [
        FilterExpr::Eq(ids.tag, CanonicalValue::String("sys".into())),
        FilterExpr::In(ids.tag, vec![CanonicalValue::String("sys".into())]),
        FilterExpr::StartsWith(ids.tag, "sy".into()),
        FilterExpr::Eq(ids.tag, CanonicalValue::String(nul_heavy.clone())),
        // P0-3(1): cross-type Range bounds on Any field -> MUST be empty!
        FilterExpr::Range {
            field: ids.score,
            low: Some((CanonicalValue::Int(5), true)),
            high: Some((CanonicalValue::String("z".into()), true)),
        },
        // P0-3(2): open Range includes Array row!
        FilterExpr::Range { field: ids.score, low: None, high: None },
    ] {
        let plan = LogicalPlan::new(LogicalNode::Terminal {
            input: Box::new(LogicalNode::OrderSlice {
                input: Box::new(LogicalNode::Filter { input: Box::new(src()), expr: expr.clone() }),
                order: OrderSpec::EntityKey(OrderDir::Asc),
                after: None,
                limit: None,
            }),
            terminal: TerminalSpec::Ids,
        });
        let got = qe.execute(&engine, &plan, ExecBudgets::default()).unwrap().result;
        let expected = oracle_filter(&engine, ids.note_type, false, &expr);
        assert_eq!(got, QueryResult::Ids(expected), "compound/range mismatch for {expr:?}");
    }

    // P0-1(2): Cached O1CountPushdown on `score == Int(5)` must NOT collide with `score == Array([1])`!
    let cnt_int = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(src()),
            expr: FilterExpr::Eq(ids.score, CanonicalValue::Int(5)),
        }),
        terminal: TerminalSpec::Count,
    });
    let cnt_arr = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(src()),
            expr: FilterExpr::Eq(ids.score, CanonicalValue::Array(vec![CanonicalValue::Int(1)])),
        }),
        terminal: TerminalSpec::Count,
    });
    let o_int = qe.execute(&engine, &cnt_int, ExecBudgets::default()).unwrap();
    assert_eq!(o_int.result, QueryResult::Count(1));
    assert!(o_int.explain.count_pushdown);
    let o_arr = qe.execute(&engine, &cnt_arr, ExecBudgets::default()).unwrap();
    assert_eq!(o_arr.result, QueryResult::Count(1));
    assert!(!o_arr.explain.count_pushdown);
}

#[test]
fn review_regression_icu_missing_order_traverse_limit_phrase_and_greek_trigrams() {
    let (engine, reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    let mut ops = vec![
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("m_alpha".into()),
            fields: vec![
                ("title".into(), CanonicalValue::String("alpha".into())),
                ("body".into(), CanonicalValue::String("ΛΟΓΟΣ exact zero copy phrase".into())),
                ("tag".into(), CanonicalValue::String("keep".into())),
            ],
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("m_null".into()),
            fields: vec![("tag".into(), CanonicalValue::String("keep".into()))],
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("m_beta".into()),
            fields: vec![
                ("title".into(), CanonicalValue::String("beta".into())),
                ("tag".into(), CanonicalValue::String("keep".into())),
            ],
        },
        WireOp::Create {
            type_name: "Author".into(),
            external_id: Some("auth1".into()),
            fields: vec![("name".into(), CanonicalValue::String("Ada".into()))],
        },
    ];
    // Add 140 high-scoring non-phrase distractors containing "zero" and "copy" non-consecutively!
    for i in 0..140 {
        ops.push(WireOp::Create {
            type_name: "Note".into(),
            external_id: Some(format!("dist_{i}")),
            fields: vec![
                ("title".into(), CanonicalValue::String("zero x copy zero x copy".into())),
                ("body".into(), CanonicalValue::String("zero gap copy zero gap copy".into())),
                ("tag".into(), CanonicalValue::String("dist".into())),
            ],
        });
    }
    // Chain for Traverse Score(Desc) test: m_alpha -> m_null -> m_beta
    ops.push(WireOp::Link {
        source: EntityTarget::ExternalId("m_alpha".into()),
        rel_kind: "CITES".into(),
        target: EntityTarget::ExternalId("m_null".into()),
        weight: Some(1.0f64.to_bits()),
        info: None,
    });
    ops.push(WireOp::Link {
        source: EntityTarget::ExternalId("m_null".into()),
        rel_kind: "CITES".into(),
        target: EntityTarget::ExternalId("m_beta".into()),
        weight: Some(1.0f64.to_bits()),
        info: None,
    });
    engine.execute_batch(&ops).unwrap();
    flush_all(&engine, &reg, &ids);

    let k_alpha = engine.get_row("m_alpha").unwrap().unwrap().key;
    let k_null = engine.get_row("m_null").unwrap().unwrap().key;
    let k_beta = engine.get_row("m_beta").unwrap().unwrap().key;
    let author_type = engine.schema().type_by_name("Author").unwrap().id;

    // P0-4: ICU Order fast path includes missing/null rows first in Asc and last in Desc!
    let src = || LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false });
    let asc_fast = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::OrderSlice {
            input: Box::new(src()),
            order: OrderSpec::Field { field: ids.title, dir: OrderDir::Asc, locale: Some("en-US".into()) },
            after: None,
            limit: Some(3),
        }),
        terminal: TerminalSpec::Ids,
    });
    let QueryResult::Ids(asc_ids) = qe.execute(&engine, &asc_fast, ExecBudgets::default()).unwrap().result else {
        panic!("expected Ids");
    };
    assert_eq!(asc_ids, vec![k_null, k_alpha, k_beta], "Asc ICU fast path must place missing/null first");

    let desc_fast = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::OrderSlice {
            input: Box::new(src()),
            order: OrderSpec::Field { field: ids.title, dir: OrderDir::Desc, locale: Some("en-US".into()) },
            after: None,
            limit: None,
        }),
        terminal: TerminalSpec::Ids,
    });
    let QueryResult::Ids(desc_ids) = qe.execute(&engine, &desc_fast, ExecBudgets::default()).unwrap().result else {
        panic!("expected Ids");
    };
    assert_eq!(desc_ids.last().copied(), Some(k_null), "Desc ICU fast path must place missing/null last");

    // P0-5: OrderSlice(Score(Desc), limit: 1) over Traverse(Hops) must return deepest node (depth 2: k_beta)!
    let trav_desc = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::OrderSlice {
            input: Box::new(LogicalNode::Traverse {
                input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k_alpha]))),
                spec: TraverseSpec::Hops { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, min_hops: 1, max_hops: 2 },
            }),
            order: OrderSpec::Score(OrderDir::Desc),
            after: None,
            limit: Some(1),
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(td) = qe.execute(&engine, &trav_desc, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(td.iter().map(|h| (h.key, h.score)).collect::<Vec<_>>(), vec![(k_beta, 2.0)]);

    // P0-6: Cross-type Score guard + mixed-case BM25F phrase search past 140 higher-scoring distractors!
    let cross_type_score = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: author_type, include_trashed: false })),
            spec: ScoreSpec::Bm25f {
                index_id: ids.idx_text,
                query: "zero copy".into(),
                require_all: true,
                prefix: false,
                phrase: None,
                top_k: Some(5),
            },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    assert_eq!(
        qe.execute(&engine, &cross_type_score, ExecBudgets::default()).unwrap().result,
        QueryResult::Hits(Vec::new())
    );

    let wrong_kind = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(src()),
            spec: ScoreSpec::Bm25f {
                index_id: ids.idx_rank, // Rank index passed to Bm25f!
                query: "zero".into(),
                require_all: false,
                prefix: false,
                phrase: None,
                top_k: Some(5),
            },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    assert_eq!(qe.execute(&engine, &wrong_kind, ExecBudgets::default()).unwrap_err().code, ErrorCode::InvalidArgument);

    let phrase_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(src()),
            spec: ScoreSpec::Bm25f {
                index_id: ids.idx_text,
                query: "zero copy".into(),
                require_all: true,
                prefix: false,
                phrase: Some(vec!["Zero".into(), "Copy".into()]), // mixed-case phrase terms!
                top_k: Some(5),
            },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(ph) = qe.execute(&engine, &phrase_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(ph.len(), 1);
    assert_eq!(ph[0].key, k_alpha);

    // P0-7: Greek final sigma (`"ΛΟΓΟΣ"` -> `"λογος"`) matches `Contains(body, "γος")` via trigram index!
    let greek_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(src()),
            expr: FilterExpr::Contains(ids.body, "γος".into()),
        }),
        terminal: TerminalSpec::Ids,
    });
    assert_eq!(
        qe.execute(&engine, &greek_plan, ExecBudgets::default()).unwrap().result,
        QueryResult::Ids(vec![k_alpha])
    );
}

#[derive(Default)]
struct FullAuditRecorder {
    point_masks: Vec<(EntityKey, FieldMask)>,
    all_field_keys: Vec<EntityKey>,
    edge_adjs: Vec<(EntityKey, EdgeDir)>,
    subdb_ranges: Vec<(u16, Vec<u8>, Vec<u8>)>,
}

impl QueryReadRecorder for FullAuditRecorder {
    fn record_point_entity(&mut self, key: EntityKey, fields: &FieldMask) {
        self.point_masks.push((key, fields.clone()));
    }
    fn record_point_entity_all_fields(&mut self, key: EntityKey) {
        self.all_field_keys.push(key);
    }
    fn record_edge_adj(&mut self, entity: EntityKey, _rel_kinds: &[RelKindId], dir: EdgeDir) {
        self.edge_adjs.push((entity, dir));
    }
    fn record_subdb_range(&mut self, slot: u16, low: &[u8], high: &[u8]) {
        self.subdb_ranges.push((slot, low.to_vec(), high.to_vec()));
    }
}

#[test]
fn review_regression_read_recorder_building_transition_and_p1_p2_hardening() {
    let (engine, reg, ids) = open_sim(BuildPolicy::DeferredWatermark);
    let qe = QueryEngine::default();

    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("a1".into()),
                fields: vec![
                    ("tag".into(), CanonicalValue::String("t".into())),
                    ("updated_at".into(), CanonicalValue::Int(i64::MIN)),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("a2".into()),
                fields: vec![("tag".into(), CanonicalValue::String("t".into()))],
            },
            WireOp::Link {
                source: EntityTarget::ExternalId("a1".into()),
                rel_kind: "CITES".into(),
                target: EntityTarget::ExternalId("a2".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
        ])
        .unwrap();
    let k1 = engine.get_row("a1").unwrap().unwrap().key;
    let k2 = engine.get_row("a2").unwrap().unwrap().key;
    flush_all(&engine, &reg, &ids);

    // P0-8: Verify read recording across LiteralKeys, Rows(missing field), Rows(None), HasLink, GraphAnalyze, empty Score!
    let mut fm = FieldMask::empty();
    fm.insert(ids.title); // missing on `a1`!
    let mut rec = FullAuditRecorder::default();
    let rows_missing = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1]))),
        terminal: TerminalSpec::Rows { fields: Some(fm.clone()) },
    });
    qe.execute_tracked(&engine, &rows_missing, ExecBudgets::default(), &mut rec).unwrap();
    assert!(rec.point_masks.iter().any(|(k, m)| *k == k1 && m.contains(ids.title)));

    let mut rec_all = FullAuditRecorder::default();
    let rows_all = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1]))),
        terminal: TerminalSpec::Rows { fields: None },
    });
    qe.execute_tracked(&engine, &rows_all, ExecBudgets::default(), &mut rec_all).unwrap();
    assert!(rec_all.all_field_keys.contains(&k1));

    let mut rec_link = FullAuditRecorder::default();
    let has_link = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            expr: FilterExpr::HasLink { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, target: Some(k2) },
        }),
        terminal: TerminalSpec::Ids,
    });
    qe.execute_tracked(&engine, &has_link, ExecBudgets::default(), &mut rec_link).unwrap();
    assert!(rec_link.point_masks.iter().any(|(k, _)| *k == k1), "HasLink pushdown must record neighbor liveness");

    let mut rec_graph = FullAuditRecorder::default();
    let wcc_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1, k2]))),
        spec: GraphAnalyzeSpec::WeakComponents { rel_kinds: vec![ids.cites] },
    });
    qe.execute_tracked(&engine, &wcc_plan, ExecBudgets::default(), &mut rec_graph).unwrap();
    assert!(rec_graph.edge_adjs.iter().any(|(k, _)| *k == k1));

    // Empty vector index must still record subdb range!
    let mut rec_vec = FullAuditRecorder::default();
    let empty_vec = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            spec: ScoreSpec::VectorNear { index_id: ids.idx_vec, vector: vec![1.0, 0.0, 0.0, 0.0], top_k: 5 },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    qe.execute_tracked(&engine, &empty_vec, ExecBudgets::default(), &mut rec_vec).unwrap();
    assert!(!rec_vec.subdb_ranges.is_empty());

    // P1-1: Building -> Ready transition must immediately enable O1CountPushdown (not pinned as PrimaryScan!)
    let mut s2 = test_schema();
    s2.types.get_mut("Note").unwrap().indexes.insert(
        "note_updated_val".into(),
        RawIndex { kind: RawIndexKind::Value { fields: vec!["updated_at".into()], unique: false } },
    );
    engine.set_schema(&s2).unwrap();
    let cnt_upd = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            expr: FilterExpr::Eq(ids.updated_at, CanonicalValue::Int(i64::MIN)),
        }),
        terminal: TerminalSpec::Count,
    });
    let before_ready = qe.execute(&engine, &cnt_upd, ExecBudgets::default()).unwrap();
    assert!(!before_ready.explain.count_pushdown);
    while reg.drive_build_chunk(&engine, 1024).unwrap() {}
    let after_ready = qe.execute(&engine, &cnt_upd, ExecBudgets::default()).unwrap();
    assert!(after_ready.explain.count_pushdown, "PlanCache must not pin PrimaryScan across Building -> Ready!");
    assert_eq!(after_ready.result, QueryResult::Count(1));

    // P1-2: RecencyDecay does not overflow on `now_ms = 0, ms = i64::MIN`!
    let recency = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1]))),
            spec: ScoreSpec::RecencyDecay { field: ids.updated_at, now_ms: 0, half_life_ms: 1000.0 },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(rh) = qe.execute(&engine, &recency, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(rh.len(), 1);
    assert!(rh[0].score.is_finite());

    // P1-4: project_graph_terminal handles Count/Exists & rejects unsupported terminals
    let g_cnt = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::GraphAnalyze {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1, k2]))),
            spec: GraphAnalyzeSpec::WeakComponents { rel_kinds: vec![ids.cites] },
        }),
        terminal: TerminalSpec::Count,
    });
    assert_eq!(qe.execute(&engine, &g_cnt, ExecBudgets::default()).unwrap().result, QueryResult::Count(1));

    // P1-5: Mismatched cursor variant returns InvalidArgument
    let bad_cursor = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::OrderSlice {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1]))),
            order: OrderSpec::EntityKey(OrderDir::Asc),
            after: Some(Cursor::Score { score: 1.0, key: k1 }),
            limit: Some(5),
        }),
        terminal: TerminalSpec::Ids,
    });
    assert_eq!(qe.execute(&engine, &bad_cursor, ExecBudgets::default()).unwrap_err().code, ErrorCode::InvalidArgument);

    // P1-6: PPR huge seed weights `[1e308, 1e308]` normalize without overflowing to 0!
    let ppr_huge = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1, k2]))),
        spec: GraphAnalyzeSpec::Ppr {
            rel_kinds: vec![ids.cites],
            weighted: false,
            seeds: vec![(k1, 1e308), (k2, 1e308)],
            damping: 0.85,
            iterations: 20,
            tolerance: 1e-8,
        },
    });
    let QueryResult::GraphPpr(ppr_res) = qe.execute(&engine, &ppr_huge, ExecBudgets::default()).unwrap().result else {
        panic!("expected GraphPpr");
    };
    let ppr_sum: f64 = ppr_res.scores.iter().map(|(_, s)| *s).sum();
    assert!((ppr_sum - 1.0).abs() < 1e-6, "PPR scores must sum to 1.0 even for 1e308 seed weights");

    // P1-9: Negative Fuse weight returns InvalidArgument
    let bad_fuse = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1]))),
            spec: ScoreSpec::Fuse {
                inputs: vec![(ScoreSpec::RankIndex { index_id: ids.idx_rank }, -1.0)],
                method: FuseMethod::default(),
            },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    assert_eq!(qe.execute(&engine, &bad_fuse, ExecBudgets::default()).unwrap_err().code, ErrorCode::InvalidArgument);

    // P2-2: Invalid Regex fails immediately even on empty type!
    let author_type = engine.schema().type_by_name("Author").unwrap().id;
    let author_name = engine.schema().type_by_name("Author").unwrap().field_by_name("name").unwrap().id;
    let bad_re = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: author_type, include_trashed: false })),
            expr: FilterExpr::Regex(author_name, "[unclosed".into()),
        }),
        terminal: TerminalSpec::Ids,
    });
    assert_eq!(qe.execute(&engine, &bad_re, ExecBudgets::default()).unwrap_err().code, ErrorCode::InvalidArgument);

    // P2-3 + Phase 0B: ExactSum is order-independent even across intermediate f64 overflow!
    // `[f64::MAX, f64::MAX, -f64::MAX]` and `[f64::MAX, -f64::MAX, f64::MAX]` both equal `f64::MAX`,
    // while `[f64::MAX, f64::MAX]` overflows cleanly at `try_total()` without poisoning non-finite `add`.
    let mut es = ExactSum::default();
    assert!(!es.add(f64::NAN));
    assert!(!es.add(f64::INFINITY));
    assert!(es.add(f64::MAX));
    assert!(es.add(f64::MAX));
    assert_eq!(es.try_total(), None, "unbalanced 2 * f64::MAX must report overflow");
    assert!(es.add(-f64::MAX));
    assert_eq!(es.try_total(), Some(f64::MAX), "intermediate overflow must cancel order-independently");
    assert_eq!(es.total(), f64::MAX);

    let mut es_tiny = ExactSum::default();
    assert!(es_tiny.add(f64::MAX));
    assert!(es_tiny.add(1e-300));
    assert!(es_tiny.add(-f64::MAX));
    assert_eq!(es_tiny.try_total(), Some(1e-300), "exact cancellation preserves sub-ULP low-band value");

    // Low-band cancellation + quantitative tiny fold + signed overflow + exact tie-break counterexamples!
    let mut es_neg_ov = ExactSum::default();
    assert!(es_neg_ov.add(-f64::MAX));
    assert!(es_neg_ov.add(-f64::MAX));
    assert_eq!(es_neg_ov.try_total(), None);
    assert_eq!(es_neg_ov.total(), f64::NEG_INFINITY);

    // High band cancels with low band to exact zero -> preserves tiny sub-2^-450 value bit-exactly!
    let two_512 = f64::from_bits((1023u64 + 512) << 52);
    let two_511 = f64::from_bits((1023u64 + 511) << 52);
    let mut es_cancel_tiny = ExactSum::default();
    assert!(es_cancel_tiny.add(two_512));
    assert!(es_cancel_tiny.add(-two_511));
    assert!(es_cancel_tiny.add(-two_511));
    assert!(es_cancel_tiny.add(1e-300));
    assert_eq!(es_cancel_tiny.try_total(), Some(1e-300));

    // High + low cancels to a small non-zero residual (`2^-449`) while `low` also keeps a separate
    // sub-`2^-450` part (`2^-502 + 2^-503`) in `tiny` -> MUST enter the unscaled fold branch so the
    // `tiny` part rounds `2^-449` up by 1 ULP (`2^-501`), killing inner `* -> +` on line 140!
    let two_neg_449 = f64::from_bits((1023u64 - 449) << 52);
    let two_neg_501 = f64::from_bits((1023u64 - 501) << 52);
    let two_neg_502 = f64::from_bits((1023u64 - 502) << 52);
    let two_neg_503 = f64::from_bits((1023u64 - 503) << 52);
    let mut es_fold = ExactSum::default();
    assert!(es_fold.add(two_512));
    assert!(es_fold.add(-two_511));
    assert!(es_fold.add(-two_511));
    assert!(es_fold.add(two_neg_449));
    assert!(es_fold.add(two_neg_502));
    assert!(es_fold.add(two_neg_503));
    assert_eq!(es_fold.try_total(), Some(two_neg_449 + two_neg_501));

    // `[f64::MAX, 2^970 (half-ULP), 1e-200]`: genuine positive overflow MUST stay in the scaled
    // branch and return `+inf` (never `NaN` from unscaled `f64::MAX + 2^970` inside `tiny`),
    // killing outer `* -> +` and `* -> /` on line 140!
    let half_ulp_max = f64::from_bits((1023u64 + 970) << 52);
    let mut es_pos_ov_tiny = ExactSum::default();
    assert!(es_pos_ov_tiny.add(f64::MAX));
    assert!(es_pos_ov_tiny.add(half_ulp_max));
    assert!(es_pos_ov_tiny.add(1e-200));
    assert_eq!(es_pos_ov_tiny.try_total(), None);
    assert_eq!(es_pos_ov_tiny.total(), f64::INFINITY);

    // Exact midpoint tie-breaks above 2^1000:
    // 1. `[2^1000, 2^947 (half-ULP), -1.0, 1e-200]`: true sum < midpoint -> rounds DOWN to `2^1000`!
    let two_1000 = f64::from_bits((1023u64 + 1000) << 52);
    let half_ulp_1000 = f64::from_bits((1023u64 + 947) << 52);
    let mut es_tie_down = ExactSum::default();
    assert!(es_tie_down.add(two_1000));
    assert!(es_tie_down.add(half_ulp_1000));
    assert!(es_tie_down.add(-1.0));
    assert!(es_tie_down.add(1e-200));
    assert_eq!(es_tie_down.try_total(), Some(two_1000));

    // 2. `[f64::MAX, 2^970 (half-ULP), -1.0, 1e-200]`: true sum < midpoint -> MUST NOT overflow (`Some(f64::MAX)`)!
    let mut es_no_spurious_ov = ExactSum::default();
    assert!(es_no_spurious_ov.add(f64::MAX));
    assert!(es_no_spurious_ov.add(half_ulp_max));
    assert!(es_no_spurious_ov.add(-1.0));
    assert!(es_no_spurious_ov.add(1e-200));
    assert_eq!(es_no_spurious_ov.try_total(), Some(f64::MAX));

    // 3. `[2^1001, 2^948 (half-ULP), 1e-300]`: true sum > midpoint -> breaks tie UPWARD!
    let mut es_high_plus_tiny = ExactSum::default();
    let two_1001 = f64::from_bits((1023u64 + 1001) << 52);
    assert!(es_high_plus_tiny.add(two_1001));
    assert!(es_high_plus_tiny.add(f64::from_bits((1023u64 + 1001 - 53) << 52)));
    assert!(es_high_plus_tiny.add(1e-300));
    assert!(es_high_plus_tiny.try_total().unwrap() > two_1001);
}

#[test]
fn graph_analyze_filters_soft_trashed_literal_keys_and_records_read_set() {
    let (engine, _reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    // A -> B -> C with B soft-trashed (and C -> A so SCC would be 1 cycle if B leaked).
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("A".into()),
                fields: vec![("title".into(), CanonicalValue::String("A".into()))],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("B".into()),
                fields: vec![("title".into(), CanonicalValue::String("B".into()))],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("C".into()),
                fields: vec![("title".into(), CanonicalValue::String("C".into()))],
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("A".into()),
                target: EntityTarget::ExternalId("B".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("B".into()),
                target: EntityTarget::ExternalId("C".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("C".into()),
                target: EntityTarget::ExternalId("A".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
            WireOp::Trash { target: EntityTarget::ExternalId("B".into()) },
        ])
        .unwrap();

    let ka = engine.get_row("A").unwrap().unwrap().key;
    let kb = engine.get_row("B").unwrap().unwrap().key;
    let kc = engine.get_row("C").unwrap().unwrap().key;
    let lit = || LogicalNode::Source(SourceSpec::LiteralKeys(vec![ka, kb, kc]));

    // 1. WeakComponents over LiteralKeys([A, B, C]) excludes trashed B (leaving {A}, {C} disconnected once C->A is removed)
    engine
        .execute_batch(&[WireOp::Unlink {
            rel_kind: Some("CITES".into()),
            source: EntityTarget::ExternalId("C".into()),
            target: Some(EntityTarget::ExternalId("A".into())),
        }])
        .unwrap();

    let wc_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(lit()),
        spec: GraphAnalyzeSpec::WeakComponents { rel_kinds: vec![ids.cites] },
    });
    let mut rec = FullAuditRecorder::default();
    let wc_out = qe.execute_tracked(&engine, &wc_plan, ExecBudgets::default(), &mut rec).unwrap();
    let QueryResult::GraphGroups(wc) = wc_out.result else { panic!("expected GraphGroups") };
    assert_eq!(wc, vec![vec![ka], vec![kc]], "trashed bridge B must not connect A and C");
    assert!(
        rec.point_masks.iter().any(|(k, _)| *k == kb),
        "ReadSet must track trashed node B for restore invalidation"
    );

    // 2. SCC, TopoSort, PPR, Leiden over LiteralKeys([A, B, C]) must all exclude trashed B.
    let scc_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(lit()),
        spec: GraphAnalyzeSpec::Scc { rel_kinds: vec![ids.cites] },
    });
    let QueryResult::GraphGroups(sccs) = qe.execute(&engine, &scc_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected GraphGroups");
    };
    assert!(!sccs.iter().flatten().any(|&k| k == kb));

    let topo_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(lit()),
        spec: GraphAnalyzeSpec::TopoSort { rel_kinds: vec![ids.cites] },
    });
    let QueryResult::GraphTopo(topo) = qe.execute(&engine, &topo_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected GraphTopo");
    };
    assert_eq!(topo.order, vec![ka, kc]);

    let ppr_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(lit()),
        spec: GraphAnalyzeSpec::Ppr {
            rel_kinds: vec![ids.cites],
            weighted: false,
            seeds: vec![(ka, 1.0), (kb, 1.0)],
            damping: 0.85,
            iterations: 20,
            tolerance: 1e-8,
        },
    });
    let QueryResult::GraphPpr(ppr) = qe.execute(&engine, &ppr_plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected GraphPpr");
    };
    assert!(!ppr.scores.iter().any(|&(k, _)| k == kb));

    let leiden_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(lit()),
        spec: GraphAnalyzeSpec::Leiden {
            rel_kinds: vec![ids.cites],
            weighted: false,
            resolution: 1.0,
            seed: 42,
            iterations: 5,
        },
    });
    let QueryResult::GraphGroups(comms) = qe.execute(&engine, &leiden_plan, ExecBudgets::default()).unwrap().result
    else {
        panic!("expected GraphGroups");
    };
    assert!(!comms.iter().flatten().any(|&k| k == kb));

    // 3. Contrast: SourceSpec::Type { include_trashed: true } DOES include B and bridges A -> B -> C!
    let wc_incl = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: true })),
        spec: GraphAnalyzeSpec::WeakComponents { rel_kinds: vec![ids.cites] },
    });
    let QueryResult::GraphGroups(wc_all) = qe.execute(&engine, &wc_incl, ExecBudgets::default()).unwrap().result else {
        panic!("expected GraphGroups");
    };
    assert_eq!(wc_all, vec![vec![ka, kb, kc]]);
}

#[test]
fn batch_d_bm25f_unbounded_query_succeeds_without_overflow() {
    let (engine, _reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("bm1".into()),
                fields: vec![("title".into(), CanonicalValue::String("nebula drift".into()))],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("bm2".into()),
                fields: vec![("title".into(), CanonicalValue::String("nebula cloud".into()))],
            },
        ])
        .unwrap();

    // ScoreSpec::Bm25f with `top_k: None` and no OrderSlice limit passes `usize::MAX` into BMW search.
    let plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            spec: ScoreSpec::Bm25f {
                index_id: ids.idx_text,
                query: "nebula".into(),
                require_all: false,
                prefix: false,
                phrase: None,
                top_k: None,
            },
        }),
        terminal: TerminalSpec::Hits { fields: None, highlights: false },
    });
    let QueryResult::Hits(hits) = qe.execute(&engine, &plan, ExecBudgets::default()).unwrap().result else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 2);
}

#[test]
fn batch_d_literal_keys_soft_trash_filtered_in_degrees_has_link_and_traversals() {
    let (engine, _reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    engine
        .execute_batch(&[
            WireOp::Create { type_name: "Note".into(), external_id: Some("A".into()), fields: vec![] },
            WireOp::Create { type_name: "Note".into(), external_id: Some("B".into()), fields: vec![] },
            WireOp::Create { type_name: "Note".into(), external_id: Some("C".into()), fields: vec![] },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("A".into()),
                target: EntityTarget::ExternalId("B".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("B".into()),
                target: EntityTarget::ExternalId("C".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("A".into()),
                target: EntityTarget::ExternalId("C".into()),
                weight: Some(1.0f64.to_bits()),
                info: None,
            },
            WireOp::Trash { target: EntityTarget::ExternalId("B".into()) },
        ])
        .unwrap();

    let ka = engine.get_row("A").unwrap().unwrap().key;
    let kb = engine.get_row("B").unwrap().unwrap().key;
    let kc = engine.get_row("C").unwrap().unwrap().key;

    // 1. Degrees over LiteralKeys([A, B, C]) skips trashed B and counts only live edges (A->C = 1 each).
    let deg_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![ka, kb, kc]))),
        terminal: TerminalSpec::Degrees { rel_kinds: vec![ids.cites], dir: EdgeDir::Both },
    });
    let mut rec_deg = FullAuditRecorder::default();
    let deg_out = qe.execute_tracked(&engine, &deg_plan, ExecBudgets::default(), &mut rec_deg).unwrap();
    assert_eq!(deg_out.result, QueryResult::Degrees(vec![(ka, 1), (kc, 1)]));
    assert!(rec_deg.point_masks.iter().any(|(k, _)| *k == kb), "Degrees must track trashed subject for restore");

    // 2. HasLink over LiteralKeys([B]) returns empty even though B->C points to live C!
    let hl_any = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![ka, kb]))),
            expr: FilterExpr::HasLink { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, target: None },
        }),
        terminal: TerminalSpec::Ids,
    });
    let mut rec_hl = FullAuditRecorder::default();
    let hl_out = qe.execute_tracked(&engine, &hl_any, ExecBudgets::default(), &mut rec_hl).unwrap();
    assert_eq!(hl_out.result, QueryResult::Ids(vec![ka]));
    assert!(rec_hl.point_masks.iter().any(|(k, _)| *k == kb), "HasLink must track trashed subject for restore");

    let hl_target = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![ka, kb]))),
            expr: FilterExpr::HasLink { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, target: Some(kc) },
        }),
        terminal: TerminalSpec::Ids,
    });
    assert_eq!(qe.execute(&engine, &hl_target, ExecBudgets::default()).unwrap().result, QueryResult::Ids(vec![ka]));

    // EdgeDir::In with explicit targets (live, trashed, and non-matching) on LiteralKeys([kc]).
    let hl_in_live = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![kc]))),
            expr: FilterExpr::HasLink { rel_kinds: vec![ids.cites], dir: EdgeDir::In, target: Some(ka) },
        }),
        terminal: TerminalSpec::Ids,
    });
    assert_eq!(qe.execute(&engine, &hl_in_live, ExecBudgets::default()).unwrap().result, QueryResult::Ids(vec![kc]));

    let hl_in_trashed = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![kc]))),
            expr: FilterExpr::HasLink { rel_kinds: vec![ids.cites], dir: EdgeDir::In, target: Some(kb) },
        }),
        terminal: TerminalSpec::Ids,
    });
    assert_eq!(qe.execute(&engine, &hl_in_trashed, ExecBudgets::default()).unwrap().result, QueryResult::Ids(vec![]));

    let hl_in_mismatch = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![kc]))),
            expr: FilterExpr::HasLink { rel_kinds: vec![ids.cites], dir: EdgeDir::In, target: Some(kc) },
        }),
        terminal: TerminalSpec::Ids,
    });
    assert_eq!(qe.execute(&engine, &hl_in_mismatch, ExecBudgets::default()).unwrap().result, QueryResult::Ids(vec![]));

    // Live Type source pushes HasLink { target: Some(kc) } into `HasLinkDriveFromTarget`.
    let hl_pushdown = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            expr: FilterExpr::HasLink { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, target: Some(kc) },
        }),
        terminal: TerminalSpec::Ids,
    });
    let hl_push_out = qe.execute(&engine, &hl_pushdown, ExecBudgets::default()).unwrap();
    assert_eq!(hl_push_out.result, QueryResult::Ids(vec![ka]));
    assert!(
        hl_push_out.explain.pushed_filters.iter().any(|s| s == "HasLinkDriveFromTarget"),
        "live Type HasLink with explicit target must push down via HasLinkDriveFromTarget: {:?}",
        hl_push_out.explain.pushed_filters
    );

    // 3. Hops & ShortestPath from LiteralKeys([B]) return empty while include_trashed: true includes B.
    let hops_trashed = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Traverse {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![kb]))),
            spec: TraverseSpec::Hops { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, min_hops: 0, max_hops: 2 },
        }),
        terminal: TerminalSpec::Ids,
    });
    assert_eq!(qe.execute(&engine, &hops_trashed, ExecBudgets::default()).unwrap().result, QueryResult::Ids(vec![]));

    let sp_trashed = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Traverse {
            input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![kb]))),
            spec: TraverseSpec::ShortestPath { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, target: kc, max_hops: 2 },
        }),
        terminal: TerminalSpec::Ids,
    });
    assert_eq!(qe.execute(&engine, &sp_trashed, ExecBudgets::default()).unwrap().result, QueryResult::Ids(vec![]));

    // 4. Explicit `include_trashed: true` DOES include B in Degrees and HasLink.
    let deg_incl = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: true })),
        terminal: TerminalSpec::Degrees { rel_kinds: vec![ids.cites], dir: EdgeDir::Both },
    });
    assert_eq!(
        qe.execute(&engine, &deg_incl, ExecBudgets::default()).unwrap().result,
        QueryResult::Degrees(vec![(ka, 2), (kb, 2), (kc, 2)])
    );

    let hl_incl = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: true })),
            expr: FilterExpr::HasLink { rel_kinds: vec![ids.cites], dir: EdgeDir::Out, target: Some(kc) },
        }),
        terminal: TerminalSpec::Ids,
    });
    assert_eq!(qe.execute(&engine, &hl_incl, ExecBudgets::default()).unwrap().result, QueryResult::Ids(vec![ka, kb]));
}

#[test]
fn batch_d_plan_cache_skips_failed_index_and_invalidates_on_maint_seq() {
    // 1. Direct PlanCache unit tests for `(schema_epoch, maint_seq)` versioning & stale put rejection.
    let cache = elmdb_query::PlanCache::new(16);
    let plan_a = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::AllEntities { include_trashed: false })),
        terminal: TerminalSpec::Count,
    });
    let plan_b = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::AllEntities { include_trashed: false })),
        terminal: TerminalSpec::Ids,
    });
    let strat_a = elmdb_query::CachedStrategy {
        access_label: "A".into(),
        chosen_index: None,
        use_count_pushdown: false,
        use_index_order: None,
        use_trigram_index: None,
    };
    let strat_b = elmdb_query::CachedStrategy {
        access_label: "B".into(),
        chosen_index: None,
        use_count_pushdown: false,
        use_index_order: None,
        use_trigram_index: None,
    };
    let ka_m0 = elmdb_query::PlanShapeKey::from_plan(1, 0, &plan_a);
    let kb_m0 = elmdb_query::PlanShapeKey::from_plan(1, 0, &plan_b);
    cache.put(ka_m0.clone(), strat_a.clone());
    cache.put(kb_m0.clone(), strat_b.clone());
    assert_eq!(cache.stats().entries, 2);
    assert_eq!(cache.get(&ka_m0), Some(strat_a.clone()));
    assert_eq!(cache.get(&kb_m0), Some(strat_b));
    assert_eq!(cache.stats().hits, 2);

    // Advancing `maint_seq` within the same `schema_epoch` clears prior entries automatically.
    let ka_m1 = elmdb_query::PlanShapeKey::from_plan(1, 1, &plan_a);
    assert_eq!(cache.get(&ka_m1), None);
    assert_eq!(cache.stats().entries, 0);
    assert_eq!(cache.stats().maint_seq, 1);
    cache.put(ka_m1.clone(), strat_a.clone());
    assert_eq!(cache.stats().entries, 1);

    // Stale `put` / `get` from an older `maint_seq` must be ignored and must not pollute the cache.
    cache.put(kb_m0.clone(), strat_a.clone());
    assert_eq!(cache.stats().entries, 1);
    assert_eq!(cache.get(&ka_m0), None);
    assert_eq!(cache.get(&ka_m1), Some(strat_a.clone()));

    // Advancing `maint_seq` directly via `put()` clears old entries and updates version.
    let kb_m2 = elmdb_query::PlanShapeKey::from_plan(1, 2, &plan_b);
    cache.put(kb_m2.clone(), strat_a.clone());
    assert_eq!(cache.stats().entries, 1);
    assert_eq!(cache.stats().maint_seq, 2);
    assert_eq!(cache.get(&ka_m1), None);
    assert_eq!(cache.get(&kb_m2), Some(strat_a));

    // 2. End-to-end: BuildState::Failed must not poison PlanCache; rebuild restores O1CountPushdown.
    let (engine, reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();
    let idx_tag = engine.schema().indexes.iter().find(|i| i.name == "note_tag").unwrap().id;
    engine
        .execute_batch(&[WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("f1".into()),
            fields: vec![("tag".into(), CanonicalValue::String("alpha".into()))],
        }])
        .unwrap();

    // Force `note_tag` index into BuildState::Failed at the current schema_epoch.
    engine
        .run_maintenance_write(|txn, _schema, dbs| {
            elmdb_projections::state::write_build_state(
                txn,
                dbs,
                idx_tag,
                &elmdb_projections::BuildState::Failed {
                    code: ErrorCode::InvalidArgument,
                    message: "simulated failure".into(),
                },
            )
        })
        .unwrap();

    let count_eq = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Filter {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            expr: FilterExpr::Eq(ids.tag, CanonicalValue::String("alpha".into())),
        }),
        terminal: TerminalSpec::Count,
    });
    let degraded = qe.execute(&engine, &count_eq, ExecBudgets::default()).unwrap();
    assert!(!degraded.explain.count_pushdown);
    assert_eq!(degraded.result, QueryResult::Count(1));
    assert_eq!(qe.cache_stats().entries, 0, "degraded plan must not be cached while an index is Failed");

    // Rebuild index (same schema_epoch, advances maint_seq) -> O1CountPushdown immediately restored & cached!
    reg.rebuild(&engine, Some(idx_tag)).unwrap();
    let recovered = qe.execute(&engine, &count_eq, ExecBudgets::default()).unwrap();
    assert!(recovered.explain.count_pushdown, "O1CountPushdown must be restored immediately after rebuild");
    assert!(!recovered.explain.plan_cache_hit);
    assert_eq!(recovered.result, QueryResult::Count(1));
    let hit = qe.execute(&engine, &count_eq, ExecBudgets::default()).unwrap();
    assert!(hit.explain.plan_cache_hit);
    assert!(hit.explain.count_pushdown);

    // Maintenance write bumping `maint_seq` without calling `qe.clear_cache()` still invalidates PlanCache!
    engine.run_maintenance_write(|_txn, _schema, _dbs| Ok(())).unwrap();
    let post_maint = qe.execute(&engine, &count_eq, ExecBudgets::default()).unwrap();
    assert!(!post_maint.explain.plan_cache_hit, "PlanCache must self-invalidate on maint_seq advance");
    assert!(post_maint.explain.count_pushdown);
}

#[test]
fn batch_d_leiden_normalizes_extreme_weights_without_overflow() {
    let (engine, _reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    engine
        .execute_batch(&[
            WireOp::Create { type_name: "Note".into(), external_id: Some("L1".into()), fields: vec![] },
            WireOp::Create { type_name: "Note".into(), external_id: Some("L2".into()), fields: vec![] },
            WireOp::Create { type_name: "Note".into(), external_id: Some("L3".into()), fields: vec![] },
            WireOp::Create { type_name: "Note".into(), external_id: Some("L4".into()), fields: vec![] },
            // Mutual f64::MAX edges sum to f64::INFINITY without max_w normalization!
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("L1".into()),
                target: EntityTarget::ExternalId("L2".into()),
                weight: Some(f64::MAX.to_bits()),
                info: None,
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("L2".into()),
                target: EntityTarget::ExternalId("L1".into()),
                weight: Some(f64::MAX.to_bits()),
                info: None,
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("L3".into()),
                target: EntityTarget::ExternalId("L4".into()),
                weight: Some(f64::MAX.to_bits()),
                info: None,
            },
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("L4".into()),
                target: EntityTarget::ExternalId("L3".into()),
                weight: Some(f64::MAX.to_bits()),
                info: None,
            },
            // Tiny bridge underflows to 0.0 under max_w scaling and is safely skipped.
            WireOp::Link {
                rel_kind: "CITES".into(),
                source: EntityTarget::ExternalId("L2".into()),
                target: EntityTarget::ExternalId("L3".into()),
                weight: Some(f64::MIN_POSITIVE.to_bits()),
                info: None,
            },
        ])
        .unwrap();

    let k1 = engine.get_row("L1").unwrap().unwrap().key;
    let k2 = engine.get_row("L2").unwrap().unwrap().key;
    let k3 = engine.get_row("L3").unwrap().unwrap().key;
    let k4 = engine.get_row("L4").unwrap().unwrap().key;

    let weighted_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1, k2, k3, k4]))),
        spec: GraphAnalyzeSpec::Leiden {
            rel_kinds: vec![ids.cites],
            weighted: true,
            resolution: 1.0,
            seed: 7,
            iterations: 5,
        },
    });
    let QueryResult::GraphGroups(groups) = qe.execute(&engine, &weighted_plan, ExecBudgets::default()).unwrap().result
    else {
        panic!("expected GraphGroups");
    };
    assert_eq!(groups, vec![vec![k1, k2], vec![k3, k4]]);

    // Unweighted Leiden still groups connected edges cleanly (guards `max_w > 0.0` fallback to `scale = 1.0`).
    let unweighted_plan = LogicalPlan::new(LogicalNode::GraphAnalyze {
        input: Box::new(LogicalNode::Source(SourceSpec::LiteralKeys(vec![k1, k2, k3, k4]))),
        spec: GraphAnalyzeSpec::Leiden {
            rel_kinds: vec![ids.cites],
            weighted: false,
            resolution: 1.0,
            seed: 7,
            iterations: 5,
        },
    });
    let QueryResult::GraphGroups(unweighted_groups) =
        qe.execute(&engine, &unweighted_plan, ExecBudgets::default()).unwrap().result
    else {
        panic!("expected GraphGroups");
    };
    assert!(
        unweighted_groups.iter().any(|g| g.len() > 1),
        "unweighted Leiden must retain edges instead of returning all singletons: {unweighted_groups:?}"
    );

    // Direct CsrGraph with an injected f64::INFINITY weight must not poison `max_w` and drop finite edges.
    let manual = elmdb_query::CsrGraph {
        keys: vec![k1, k2, k3],
        key_to_idx: [(k1, 0), (k2, 1), (k3, 2)].into_iter().collect(),
        offsets: vec![0, 2, 3, 3],
        targets: vec![1, 2, 0],
        weights: Some(vec![10.0, f64::INFINITY, 10.0]),
    };
    let manual_groups = elmdb_query::leiden_communities(&manual, 1.0, 7, 5).unwrap();
    assert_eq!(manual_groups, vec![vec![k1, k2], vec![k3]]);
}

#[test]
fn batch_d_order_index_checks_collator_stamp_when_limit_within_missing_keys() {
    let (engine, _reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    // 3 rows with missing title (in TAG_MISSING) + 1 row with string title (in TAG_ORDER).
    engine
        .execute_batch(&[
            WireOp::Create { type_name: "Note".into(), external_id: Some("m1".into()), fields: vec![] },
            WireOp::Create { type_name: "Note".into(), external_id: Some("m2".into()), fields: vec![] },
            WireOp::Create { type_name: "Note".into(), external_id: Some("m3".into()), fields: vec![] },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("s1".into()),
                fields: vec![("title".into(), CanonicalValue::String("Zebra".into()))],
            },
        ])
        .unwrap();

    // Corrupt TAG_STAMP (`0x00`) in the Order index sub-DB.
    let order_idx = engine.schema().indexes.iter().find(|i| i.name == "note_title_order").unwrap().clone();
    engine
        .run_maintenance_write(|txn, _schema, _dbs| {
            let db = txn.open_subdb(&order_idx.subdb_slot().name(), elmdb_storage::DbFlags::DEFAULT)?;
            txn.put(db, &[0x00], &[0xAA; 32])?;
            Ok(())
        })
        .unwrap();

    // Asc order with `limit: Some(2)` (<= 3 missing keys) never invokes `scan_ordered`,
    // yet MUST still reject the corrupted collator stamp with `IndexNotReady`!
    let plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::OrderSlice {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            order: OrderSpec::Field { field: ids.title, dir: OrderDir::Asc, locale: Some("en-US".into()) },
            after: None,
            limit: Some(2),
        }),
        terminal: TerminalSpec::Ids,
    });
    let err = qe.execute(&engine, &plan, ExecBudgets::default()).unwrap_err();
    assert_eq!(err.code, ErrorCode::IndexNotReady);
}

#[test]
fn phase0b_sum_order_independent_overflow_and_long_phrase_word() {
    let (engine, _reg, ids) = open_sim(BuildPolicy::EagerInSchemaChange);
    let qe = QueryEngine::default();

    // 1. `TerminalSpec::Sum` with `[f64::MAX, f64::MAX, -f64::MAX]` succeeds with `f64::MAX` regardless of order!
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n1".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("hello world".into())),
                    ("score".into(), CanonicalValue::Float(f64::MAX)),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n2".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("hello".into())),
                    ("score".into(), CanonicalValue::Float(f64::MAX)),
                ],
            },
            WireOp::Create {
                type_name: "Note".into(),
                external_id: Some("n3".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("cancel".into())),
                    ("score".into(), CanonicalValue::Float(-f64::MAX)),
                ],
            },
        ])
        .unwrap();

    let sum_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
        terminal: TerminalSpec::Sum(ids.score),
    });
    match qe.execute(&engine, &sum_plan, ExecBudgets::default()).unwrap().result {
        QueryResult::Sum(v) => assert_eq!(v, f64::MAX),
        other => panic!("expected Sum(f64::MAX), got {other:?}"),
    }

    // Trashing `n3` leaves `[f64::MAX, f64::MAX]` -> genuine overflow -> `ResourceLimit`!
    engine.execute_batch(&[WireOp::Trash { target: EntityTarget::ExternalId("n3".into()) }]).unwrap();
    let ov_err = qe.execute(&engine, &sum_plan, ExecBudgets::default()).unwrap_err();
    assert_eq!(ov_err.code, ErrorCode::ResourceLimit);

    // 2. Phrase query ending on a word > 128 bytes MUST NOT falsely match `"hello"`!
    let long_word = "z".repeat(140);
    let phrase_plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Score {
            input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: ids.note_type, include_trashed: false })),
            spec: ScoreSpec::Bm25f {
                index_id: ids.idx_text,
                query: "hello".into(),
                require_all: false,
                prefix: false,
                phrase: Some(vec![format!("hello {long_word}")]),
                top_k: Some(10),
            },
        }),
        terminal: TerminalSpec::Ids,
    });
    match qe.execute(&engine, &phrase_plan, ExecBudgets::default()).unwrap().result {
        QueryResult::Ids(ids_out) => assert!(ids_out.is_empty(), "phrase ending on >128B word must not match 'hello'"),
        other => panic!("expected empty Ids, got {other:?}"),
    }
}
