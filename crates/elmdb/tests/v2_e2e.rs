use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use elmdb::protocol::{
    FieldDefinition, FieldSpec, IndexDefinition, QueryAnswerBody, QueryFilter, QueryScore, QuerySource, QuerySpec,
    QueryStep, QueryTerminal, RelationDefinition, RelationEndpoint, Schema, TypeDefinition,
};
use elmdb::{
    BackupInto, BuildPolicy, CanonicalValue, CommitReturnFlags, Database, DurabilityMode, EntityTarget, ErrorCode,
    ExecBudgets, OpenOptions, WatchPolicy, WireOp, encode_batch,
};
use tempfile::tempdir;

fn make_schema() -> Schema {
    let mut note_fields = BTreeMap::new();
    note_fields.insert("title".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    note_fields.insert("body".into(), FieldDefinition { kind: FieldSpec::String, optional: true, nullable: false });
    note_fields
        .insert("priority".into(), FieldDefinition { kind: FieldSpec::Integer, optional: true, nullable: false });

    let mut note_indexes = BTreeMap::new();
    note_indexes
        .insert("by_priority".into(), IndexDefinition::Value { fields: vec!["priority".into()], unique: false });
    note_indexes.insert("by_title".into(), IndexDefinition::Order { field: "title".into(), locale: "en".into() });
    note_indexes
        .insert("search".into(), IndexDefinition::Text { fields: vec![("title".into(), 3), ("body".into(), 1)] });
    note_indexes.insert("fuzzy".into(), IndexDefinition::Trigram { fields: vec!["title".into()] });
    note_indexes.insert(
        "vec".into(),
        IndexDefinition::Vector { fields: vec!["body".into()], dims: 4, quantization: "sq8".into() },
    );
    note_indexes.insert("rank".into(), IndexDefinition::Rank { relations: vec!["links".into()], weighted: false });

    let mut types = BTreeMap::new();
    types.insert(
        "note".into(),
        TypeDefinition {
            fields: note_fields,
            indexes: note_indexes,
            additional_fields: false,
            short_code: Some("n".into()),
            change_order: false,
            declared_by: None,
        },
    );

    let mut relations = BTreeMap::new();
    relations.insert(
        "links".into(),
        RelationDefinition {
            endpoints: vec![RelationEndpoint { source: "note".into(), target: "note".into() }],
            ..Default::default()
        },
    );

    Schema { dollar_schema: None, types, relations }
}

#[test]
fn full_v2_e2e_lifecycle_queries_watches_hooks_backup_dump_load() {
    let dir = tempdir().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .unwrap();

    // 1. Schema + beforeCommit hook
    let ch = db.set_schema(&make_schema()).unwrap();
    assert_eq!(ch.added_indexes.len(), 6);

    let mut wants = elmdb::TypeMask::empty();
    let note_tid = db.schema().unwrap().type_by_name("note").unwrap().id;
    wants.insert(note_tid);
    let hook_hits = Arc::new(AtomicUsize::new(0));
    let hook_hits_c = Arc::clone(&hook_hits);
    let hook_id = db
        .register_before_commit(elmdb::BeforeCommitHook {
            wants,
            callback: Arc::new(move |_ctx| {
                hook_hits_c.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }),
        })
        .unwrap();

    // 2. Binary elmdb-wire batch with intra-batch BatchSlot linking!
    let ops = vec![
        WireOp::Create {
            type_name: "note".into(),
            external_id: Some("n-rust".into()),
            fields: vec![
                ("title".into(), CanonicalValue::String("Rust Architecture".into())),
                ("body".into(), CanonicalValue::String("Zero copy LMDB engine design".into())),
                ("priority".into(), CanonicalValue::Int(1)),
            ],
        },
        WireOp::Create {
            type_name: "note".into(),
            external_id: Some("n-graph".into()),
            fields: vec![
                ("title".into(), CanonicalValue::String("Graph Algorithms".into())),
                ("body".into(), CanonicalValue::String("PageRank Leiden and shortest paths".into())),
                ("priority".into(), CanonicalValue::Int(2)),
            ],
        },
        WireOp::Link {
            rel_kind: "links".into(),
            source: EntityTarget::BatchSlot(0),
            target: EntityTarget::BatchSlot(1),
            weight: None,
            info: None,
        },
    ];
    let frame = encode_batch(CommitReturnFlags::default(), &ops);
    let res = db.execute_wire_batch(&frame).unwrap();
    assert_eq!(res.created_ids, vec!["n-rust".to_owned(), "n-graph".to_owned()]);
    assert_eq!(hook_hits.load(Ordering::Relaxed), 1);
    assert!(db.unregister_before_commit(hook_id));

    // 3. Embeddings + flush (drains HNSW + PageRank!)
    db.set_embeddings("vec", "n-rust", &[vec![1.0, 0.0, 0.0, 0.0]]).unwrap();
    db.set_embeddings("vec", "n-graph", &[vec![0.0, 1.0, 0.0, 0.0]]).unwrap();
    db.flush().unwrap();

    // 4. Structured + ICU Order + BM25F + Vector + GraphAnalyze queries!
    let q_all = QuerySpec {
        source: QuerySource::Type { type_name: "note".into(), include_trashed: false },
        steps: vec![
            QueryStep::Filter {
                expr: QueryFilter::Range {
                    field: "priority".into(),
                    low: Some((serde_json::json!(1), true)),
                    high: None,
                },
            },
            QueryStep::OrderSlice {
                by: "field".into(),
                field: Some("title".into()),
                dir: "asc".into(),
                locale: Some("en".into()),
                after_id: None,
                after_score: None,
                after_value: None,
                limit: None,
            },
        ],
        terminal: Some(QueryTerminal::Rows { fields: None }),
        explain: true,
    };
    let ans = db.execute_query_spec(&q_all, ExecBudgets::default()).unwrap();
    assert!(ans.explain.is_some());
    match ans.body {
        QueryAnswerBody::Rows { rows } => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0].id, "n-graph"); // "Graph Algorithms" < "Rust Architecture"
            assert_eq!(rows[1].id, "n-rust");
        }
        other => panic!("expected Rows, got {other:?}"),
    }

    // BM25F search
    let q_bm25 = QuerySpec {
        source: QuerySource::Type { type_name: "note".into(), include_trashed: false },
        steps: vec![QueryStep::Score {
            spec: QueryScore::Bm25f {
                index: "search".into(),
                query: "LMDB engine".into(),
                require_all: false,
                phrase: None,
                top_k: Some(5),
            },
        }],
        terminal: Some(QueryTerminal::Hits),
        explain: false,
    };
    match db.execute_query_spec(&q_bm25, ExecBudgets::default()).unwrap().body {
        QueryAnswerBody::Hits { hits } => {
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].id, "n-rust");
        }
        other => panic!("expected Hits, got {other:?}"),
    }

    // Vector nearest neighbor
    let q_vec = QuerySpec {
        source: QuerySource::Type { type_name: "note".into(), include_trashed: false },
        steps: vec![QueryStep::Score {
            spec: QueryScore::VectorNear { index: "vec".into(), vector: vec![0.95, 0.05, 0.0, 0.0], top_k: 2 },
        }],
        terminal: Some(QueryTerminal::Hits),
        explain: false,
    };
    match db.execute_query_spec(&q_vec, ExecBudgets::default()).unwrap().body {
        QueryAnswerBody::Hits { hits } => {
            assert_eq!(hits[0].id, "n-rust");
        }
        other => panic!("expected Hits, got {other:?}"),
    }

    // Graph analyze (PPR from n-rust reaches n-graph!)
    let q_ppr = QuerySpec {
        source: QuerySource::Type { type_name: "note".into(), include_trashed: false },
        steps: vec![QueryStep::GraphPpr {
            rel_kinds: vec!["links".into()],
            weighted: false,
            seeds: vec![("n-rust".into(), 1.0)],
            damping: 0.85,
            iterations: 20,
            tolerance: 1e-6,
        }],
        terminal: Some(QueryTerminal::Hits),
        explain: false,
    };
    match db.execute_query_spec(&q_ppr, ExecBudgets::default()).unwrap().body {
        QueryAnswerBody::Hits { hits } => {
            assert!(hits.iter().any(|h| h.id == "n-graph"));
        }
        other => panic!("expected Hits, got {other:?}"),
    }

    // 5. Live query watch registration + mutation invalidation + tick!
    let (wid, init_ans) = db.register_query_watch(&q_all, WatchPolicy::default(), 1000).unwrap();
    assert!(matches!(init_ans.body, QueryAnswerBody::Rows { ref rows } if rows.len() == 2));
    assert!(db.watches_settled());

    db.execute_batch(&[WireOp::Create {
        type_name: "note".into(),
        external_id: Some("n-third".into()),
        fields: vec![
            ("title".into(), CanonicalValue::String("Alpha".into())),
            ("priority".into(), CanonicalValue::Int(3)),
        ],
    }])
    .unwrap();
    assert!(!db.watches_settled());

    let tick = db.tick_watches(u64::MAX / 2);
    assert_eq!(tick.due_ids, vec![wid]);
    let (rerun_ans, changed) = db.rerun_query_watch(wid, &q_all, u64::MAX / 2, 1).unwrap();
    assert!(changed); // changed from 2 rows to 3 rows!
    assert!(matches!(rerun_ans.body, QueryAnswerBody::Rows { ref rows } if rows.len() == 3));
    assert!(db.watches_settled());
    db.unregister_watch(wid);

    // 6. Check + rebuild + stats
    db.flush().unwrap();
    let chk = db.check(None).unwrap();
    assert!(chk.is_clean(), "expected clean check, got {:?}", chk.mismatches);
    db.rebuild(None).unwrap();
    let chk2 = db.check(None).unwrap();
    assert!(chk2.is_clean());

    let stats = db.stats().unwrap();
    assert_eq!(stats.type_counts.get("note"), Some(&3));
    assert_eq!(stats.index_count, 6);

    // 7. Backup (New + Backups overwrite guard)
    let backup_dir = tempdir().unwrap();
    let b_target = backup_dir.path().join("snap1");
    db.backup_into(&b_target, BackupInto::New).unwrap();
    assert_eq!(db.backup_into(&b_target, BackupInto::New).unwrap_err().code, ErrorCode::AlreadyExists);
    db.backup_into(&b_target, BackupInto::Backups).unwrap();

    let b_db = Database::open(&b_target, OpenOptions { read_only: true, ..Default::default() }).unwrap();
    assert!(b_db.get_row("n-rust").unwrap().is_some());
    b_db.close().unwrap();

    // 8. Dump + Load into brand-new DB
    let dump_dir = tempdir().unwrap();
    let dump_file = dump_dir.path().join("export.json");
    let d_rep = elmdb::dump::dump(&db, &dump_file).unwrap();
    assert_eq!(d_rep.entities, 3);
    assert_eq!(d_rep.relations, 1);
    assert_eq!(d_rep.embeddings, 2);

    let loaded_dir = dump_dir.path().join("reloaded");
    let l_rep = elmdb::dump::load(&dump_file, &loaded_dir).unwrap();
    assert_eq!(l_rep.entities, 3);
    let reloaded = Database::open(&loaded_dir, OpenOptions { read_only: true, ..Default::default() }).unwrap();
    assert!(reloaded.check(None).unwrap().is_clean());
    assert_eq!(reloaded.get_edges("n-rust", Some("links"), elmdb::EdgeDir::Out).unwrap().len(), 1);
    reloaded.close().unwrap();

    // 9. Archive type + purge_archived
    db.set_schema(&Schema::default()).unwrap();
    let purged = db.purge_archived("note").unwrap();
    assert_eq!(purged, 3);
    assert!(db.get_row("n-rust").unwrap().is_none());

    db.close().unwrap();
    assert_eq!(db.stats().unwrap_err().code, ErrorCode::Closed);
}
