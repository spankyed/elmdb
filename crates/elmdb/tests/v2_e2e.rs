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
                prefix: false,
                phrase: None,
                top_k: Some(5),
            },
        }],
        terminal: Some(QueryTerminal::Hits { fields: None, highlights: false }),
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
        terminal: Some(QueryTerminal::Hits { fields: None, highlights: false }),
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
        terminal: Some(QueryTerminal::Hits { fields: None, highlights: false }),
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

#[test]
fn review_regressions_lossless_dump_load_backup_sidecar_readonly_and_typed_queries() {
    let tmp = tempdir().unwrap();
    let db_dir = tmp.path().join("main");
    let db = Database::open(
        &db_dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .unwrap();

    let mut fields = BTreeMap::new();
    fields.insert("slug".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    fields.insert("raw".into(), FieldDefinition { kind: FieldSpec::Bytes, optional: true, nullable: false });
    fields.insert("ts".into(), FieldDefinition { kind: FieldSpec::Timestamp, optional: true, nullable: false });
    fields.insert("big".into(), FieldDefinition { kind: FieldSpec::Int64, optional: true, nullable: false });
    fields.insert(
        "peer".into(),
        FieldDefinition {
            kind: FieldSpec::Ref {
                to: vec!["Item".into()],
                on_delete: elmdb::protocol::ProtocolOnDelete::AllowDangling,
            },
            optional: true,
            nullable: false,
        },
    );

    let mut indexes = BTreeMap::new();
    indexes.insert("uniq_slug".into(), IndexDefinition::Value { fields: vec!["slug".into()], unique: true });
    indexes.insert("by_ts".into(), IndexDefinition::Value { fields: vec!["ts".into()], unique: false });
    indexes.insert("by_peer".into(), IndexDefinition::Value { fields: vec!["peer".into()], unique: false });
    indexes.insert(
        "vec".into(),
        IndexDefinition::Vector { fields: vec!["slug".into()], dims: 2, quantization: "sq8".into() },
    );

    let mut types = BTreeMap::new();
    types.insert("Item".into(), TypeDefinition { fields, indexes, ..Default::default() });
    let mut legacy_fields = BTreeMap::new();
    legacy_fields.insert("k".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    types.insert("Legacy".into(), TypeDefinition { fields: legacy_fields, ..Default::default() });

    let mut relations = BTreeMap::new();
    relations.insert(
        "rel".into(),
        RelationDefinition {
            cardinality: elmdb::protocol::ProtocolCardinality::ManyToOne,
            endpoints: vec![RelationEndpoint { source: "Item".into(), target: "Item".into() }],
            ..Default::default()
        },
    );

    let full_schema = Schema { dollar_schema: None, types: types.clone(), relations: relations.clone() };
    db.set_schema(&full_schema).unwrap();

    // P0-1: compiled_to_protocol_schema roundtrips relations & Ref onDelete cleanly!
    let roundtrip = elmdb::protocol::compiled_to_protocol_schema(&db.schema().unwrap(), false);
    db.set_schema(&roundtrip).unwrap();

    // Create cyclic refs + Bytes + Timestamp(ns) + Int64(>2^53) + DanglingRef + trashed unique collision w/ embeddings!
    let big_i64: i64 = (1i64 << 58) + 12345;
    db.execute_batch(&[
        WireOp::Create {
            type_name: "Item".into(),
            external_id: Some("i-trashed".into()),
            fields: vec![("slug".into(), CanonicalValue::String("shared-slug".into()))],
        },
        WireOp::Create {
            type_name: "Item".into(),
            external_id: Some("i-a".into()),
            fields: vec![
                ("slug".into(), CanonicalValue::String("a".into())),
                ("raw".into(), CanonicalValue::Bytes(vec![0, 1, 2, 255])),
                ("ts".into(), CanonicalValue::Timestamp { ms: 1_700_000_000_000, ns: 987_654 }),
                ("big".into(), CanonicalValue::Int(big_i64)),
            ],
        },
        WireOp::Create {
            type_name: "Item".into(),
            external_id: Some("i-b".into()),
            fields: vec![
                ("slug".into(), CanonicalValue::String("b".into())),
                ("peer".into(), CanonicalValue::String("i-a".into())),
            ],
        },
        WireOp::Create {
            type_name: "Legacy".into(),
            external_id: Some("leg-1".into()),
            fields: vec![("k".into(), CanonicalValue::String("archived-row".into()))],
        },
    ])
    .unwrap();
    db.set_embeddings("vec", "i-trashed", &[vec![1.0, 0.0]]).unwrap();
    db.set_embeddings("vec", "i-a", &[vec![0.0, 1.0]]).unwrap();
    db.execute_batch(&[
        // Trash i-trashed so i-live can reuse unique slug "shared-slug"!
        WireOp::Trash { target: EntityTarget::ExternalId("i-trashed".into()) },
        WireOp::Create {
            type_name: "Item".into(),
            external_id: Some("i-live".into()),
            fields: vec![("slug".into(), CanonicalValue::String("shared-slug".into()))],
        },
        // Cyclic ref i-a -> i-b & dangling ref on i-live
        WireOp::Patch {
            target: EntityTarget::ExternalId("i-a".into()),
            fields: vec![("peer".into(), CanonicalValue::String("i-b".into()))],
        },
        WireOp::Patch {
            target: EntityTarget::ExternalId("i-live".into()),
            fields: vec![("peer".into(), CanonicalValue::DanglingRef("gone-entity".into()))],
        },
    ])
    .unwrap();

    // Archive `Legacy` type (leaving `leg-1` archived in DB)
    types.remove("Legacy");
    db.set_schema(&Schema { dollar_schema: None, types, relations }).unwrap();
    db.flush().unwrap();

    // P0-9: Query filtering on Timestamp & Ref fields!
    let q_ts = QuerySpec {
        source: QuerySource::Type { type_name: "Item".into(), include_trashed: false },
        steps: vec![QueryStep::Filter {
            expr: QueryFilter::Range {
                field: "ts".into(),
                low: Some((serde_json::json!(1_700_000_000_000i64), true)),
                high: None,
            },
        }],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    match db.execute_query_spec(&q_ts, ExecBudgets::default()).unwrap().body {
        QueryAnswerBody::Ids { ids } => assert_eq!(ids, vec!["i-a".to_owned()]),
        other => panic!("expected Ids, got {other:?}"),
    }

    let q_ref = QuerySpec {
        source: QuerySource::Type { type_name: "Item".into(), include_trashed: false },
        steps: vec![QueryStep::Filter {
            expr: QueryFilter::Eq { field: "peer".into(), value: serde_json::json!("i-b") },
        }],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    match db.execute_query_spec(&q_ref, ExecBudgets::default()).unwrap().body {
        QueryAnswerBody::Ids { ids } => assert_eq!(ids, vec!["i-a".to_owned()]),
        other => panic!("expected Ids, got {other:?}"),
    }

    // P1-4: Deleted/missing cursor afterId returns EntityNotFound instead of restarting at EntityKey(0)
    let q_bad_cursor = QuerySpec {
        source: QuerySource::Type { type_name: "Item".into(), include_trashed: false },
        steps: vec![QueryStep::OrderSlice {
            by: "id".into(),
            field: None,
            dir: "asc".into(),
            locale: None,
            after_id: Some("does-not-exist".into()),
            after_score: None,
            after_value: None,
            limit: Some(10),
        }],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    assert_eq!(
        db.execute_query_spec(&q_bad_cursor, ExecBudgets::default()).unwrap_err().code,
        ErrorCode::EntityNotFound
    );

    // P1-5: get_row_resolved resolves EntityRef to external ID (not "__entity_key:N")
    let resolved_a = db.get_row_resolved("i-a").unwrap().unwrap();
    assert_eq!(resolved_a.fields.get("peer"), Some(&serde_json::json!("i-b")));

    // P0-2: Lossless dump & load preserving Bytes, Timestamp(ns), Int64(>2^53), cyclic Refs, DanglingRef, trashed+unique+emb, archived type!
    let dump_file = tmp.path().join("full.json");
    let d_rep = elmdb::dump::dump(&db, &dump_file).unwrap();
    assert_eq!(d_rep.entities, 5);
    assert_eq!(d_rep.embeddings, 2);

    let loaded_dir = tmp.path().join("loaded");
    let l_rep = elmdb::dump::load(&dump_file, &loaded_dir).unwrap();
    assert_eq!(l_rep.entities, 5);

    let loaded = Database::open(&loaded_dir, OpenOptions { read_only: true, ..Default::default() }).unwrap();
    assert_eq!(loaded.open_report().status, "clean");
    assert!(loaded.check(None).unwrap().is_clean());
    let row_a = loaded.get_row("i-a").unwrap().unwrap();
    assert_eq!(row_a.fields.get("raw"), Some(&CanonicalValue::Bytes(vec![0, 1, 2, 255])));
    assert_eq!(row_a.fields.get("ts"), Some(&CanonicalValue::Timestamp { ms: 1_700_000_000_000, ns: 987_654 }));
    assert_eq!(row_a.fields.get("big"), Some(&CanonicalValue::Int(big_i64)));
    assert_eq!(loaded.get_row_resolved("i-a").unwrap().unwrap().fields.get("peer"), Some(&serde_json::json!("i-b")));
    assert_eq!(
        loaded.get_row("i-live").unwrap().unwrap().fields.get("peer"),
        Some(&CanonicalValue::String("gone-entity".into()))
    );
    assert!(loaded.get_row("i-trashed").unwrap().unwrap().trashed);
    assert!(loaded.get_row("leg-1").unwrap().is_some());
    loaded.close().unwrap();

    // P0-5: Compacted backup includes verified synced.v2 -> opens with status == "clean" (never unsyncedAfterReboot!)
    let bak_dir = tmp.path().join("bak");
    db.backup_into(&bak_dir, BackupInto::New).unwrap();
    let bak = Database::open(&bak_dir, OpenOptions { read_only: true, ..Default::default() }).unwrap();
    assert_eq!(bak.open_report().status, "clean");
    bak.close().unwrap();

    // P0-6: Read-only open works on a 0555/0444 directory!
    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(bak_dir.join("data.mdb"), fs::Permissions::from_mode(0o444)).unwrap();
        let ro_db = Database::open(&bak_dir, OpenOptions { read_only: true, ..Default::default() }).unwrap();
        assert!(ro_db.get_row("i-a").unwrap().is_some());
        ro_db.close().unwrap();
        fs::set_permissions(bak_dir.join("data.mdb"), fs::Permissions::from_mode(0o644)).unwrap();
    }

    db.close().unwrap();
}

#[test]
fn transact_nested_savepoints_ryow_counters_hooks_and_dump_load() {
    let tmp = tempdir().unwrap();
    let db_dir = tmp.path().join("txn_db");
    let db = Arc::new(
        Database::open(
            &db_dir,
            OpenOptions {
                durability: DurabilityMode::Strict,
                build_policy: BuildPolicy::EagerInSchemaChange,
                ..Default::default()
            },
        )
        .unwrap(),
    );
    db.set_schema(&make_schema()).unwrap();

    assert_eq!(db.pending_commit_seq(), None);
    assert_eq!(db.get_counter("DOC").unwrap(), 0);

    // 3-level nested transact with RYOW, partial inner rollback, and counters.
    let hook_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let seen_ryow_in_hook = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_calls_c = Arc::clone(&hook_calls);
    let seen_seq_c = Arc::clone(&seen_seq);
    let seen_ryow_c = Arc::clone(&seen_ryow_in_hook);
    let db_for_hook = Arc::clone(&db);
    let hook_id = db
        .register_host_before_commit(
            elmdb::TypeMask::all(),
            Arc::new(move |delta| {
                hook_calls_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                seen_seq_c.store(delta.commit_seq, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(db_for_hook.pending_commit_seq(), Some(delta.commit_seq));
                // Hook can read uncommitted writes via RYOW and increment counters!
                let n1 = db_for_hook.get_row_resolved("n-outer").unwrap();
                let n2 = db_for_hook.get_row_resolved("n-inner-ok").unwrap();
                let n3 = db_for_hook.get_row_resolved("n-inner-abort").unwrap();
                if n1.is_some() && n2.is_some() && n3.is_none() {
                    seen_ryow_c.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                let _ = db_for_hook.next_counter("HOOK_SEQ")?;
                Ok(None)
            }),
        )
        .unwrap();

    let committed_seq = db
        .transact_scoped(elmdb::TransactOptions::default(), || {
            let pseq = db.pending_commit_seq().expect("pending seq inside transact");
            assert_eq!(db.next_counter("DOC")?, 1);
            db.execute_batch(&[WireOp::Create {
                type_name: "note".into(),
                external_id: Some("n-outer".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("Outer".into())),
                    ("body".into(), CanonicalValue::String("outer body".into())),
                    ("priority".into(), CanonicalValue::Int(1)),
                ],
            }])?;

            // Level 2 commits into Level 1 (must NOT fire beforeCommit yet!)
            db.transact_scoped(elmdb::TransactOptions::default(), || {
                assert_eq!(db.pending_commit_seq(), Some(pseq));
                assert_eq!(db.next_counter("DOC")?, 2);
                db.execute_batch(&[WireOp::Create {
                    type_name: "note".into(),
                    external_id: Some("n-inner-ok".into()),
                    fields: vec![
                        ("title".into(), CanonicalValue::String("Inner OK".into())),
                        ("body".into(), CanonicalValue::String("inner body".into())),
                        ("priority".into(), CanonicalValue::Int(2)),
                    ],
                }])?;

                // Level 3 writes + bumps counter then aborts -> rolled back via native LMDB savepoint!
                let l3: elmdb::Result<()> = db.transact_scoped(elmdb::TransactOptions::default(), || {
                    assert_eq!(db.next_counter("DOC")?, 3);
                    db.execute_batch(&[WireOp::Create {
                        type_name: "note".into(),
                        external_id: Some("n-inner-abort".into()),
                        fields: vec![
                            ("title".into(), CanonicalValue::String("Rolled back".into())),
                            ("body".into(), CanonicalValue::String("gone".into())),
                            ("priority".into(), CanonicalValue::Int(3)),
                        ],
                    }])?;
                    // RYOW sees level 3 write inside level 3!
                    assert!(db.get_row_resolved("n-inner-abort")?.is_some());
                    Err(elmdb_wire::ElmdbError::new(ErrorCode::Cancelled, "rollback L3"))
                });
                assert!(l3.is_err());

                // After L3 rollback: L3 entity & counter gone, L1 + L2 still visible via RYOW & queries!
                assert!(db.get_row_resolved("n-inner-abort")?.is_none());
                assert_eq!(db.get_counter("DOC")?, 2);
                let q_cnt = QuerySpec {
                    source: QuerySource::Type { type_name: "note".into(), include_trashed: false },
                    steps: vec![],
                    terminal: Some(QueryTerminal::Count),
                    explain: false,
                };
                assert!(matches!(
                    db.execute_query_spec(&q_cnt, ExecBudgets::default())?.body,
                    QueryAnswerBody::Count { count: 2 }
                ));
                Ok(())
            })?;

            // beforeCommit MUST NOT have fired yet on inner savepoint release!
            assert_eq!(hook_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            Ok(pseq)
        })
        .unwrap();

    assert_eq!(db.pending_commit_seq(), None);
    assert_eq!(hook_calls.load(std::sync::atomic::Ordering::SeqCst), 1, "beforeCommit fires ONCE on outermost commit");
    assert_eq!(seen_seq.load(std::sync::atomic::Ordering::SeqCst), committed_seq);
    assert!(seen_ryow_in_hook.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(db.get_counter("DOC").unwrap(), 2);
    assert_eq!(db.get_counter("HOOK_SEQ").unwrap(), 1);
    assert!(db.get_row_resolved("n-outer").unwrap().is_some());
    assert!(db.get_row_resolved("n-inner-ok").unwrap().is_some());
    assert!(db.get_row_resolved("n-inner-abort").unwrap().is_none());

    // Outer rollback reverts ALL nested writes & counters!
    let outer_abort: elmdb::Result<()> = db.transact_scoped(elmdb::TransactOptions::default(), || {
        assert_eq!(db.next_counter("DOC")?, 3);
        db.transact_scoped(elmdb::TransactOptions::default(), || {
            db.execute_batch(&[WireOp::Create {
                type_name: "note".into(),
                external_id: Some("n-outer-abort".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("Nope".into())),
                    ("body".into(), CanonicalValue::String("nope".into())),
                    ("priority".into(), CanonicalValue::Int(9)),
                ],
            }])?;
            Ok(())
        })?;
        Err(elmdb_wire::ElmdbError::new(ErrorCode::Cancelled, "abort outer"))
    });
    assert!(outer_abort.is_err());
    assert_eq!(db.get_counter("DOC").unwrap(), 2);
    assert!(db.get_row_resolved("n-outer-abort").unwrap().is_none());
    assert_eq!(hook_calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // run_before_commit: false bypasses hooks cleanly (used by projection catch-up chunks!)
    db.transact_scoped(elmdb::TransactOptions { run_before_commit: false }, || {
        db.set_counter("IMPORT", 42)?;
        db.execute_batch(&[WireOp::Create {
            type_name: "note".into(),
            external_id: Some("n-no-hook".into()),
            fields: vec![
                ("title".into(), CanonicalValue::String("Catchup".into())),
                ("body".into(), CanonicalValue::String("no hook".into())),
                ("priority".into(), CanonicalValue::Int(4)),
            ],
        }])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(hook_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(db.get_counter("IMPORT").unwrap(), 42);
    assert!(db.unregister_before_commit(hook_id));

    // Counters survive dump & load losslessly!
    let dump_file = tmp.path().join("counters_dump.json");
    elmdb::dump::dump(&db, &dump_file).unwrap();
    let loaded_dir = tmp.path().join("counters_loaded");
    elmdb::dump::load(&dump_file, &loaded_dir).unwrap();
    let loaded = Database::open(&loaded_dir, OpenOptions { read_only: true, ..Default::default() }).unwrap();
    assert_eq!(loaded.get_counter("DOC").unwrap(), 2);
    assert_eq!(loaded.get_counter("HOOK_SEQ").unwrap(), 1);
    assert_eq!(loaded.get_counter("IMPORT").unwrap(), 42);
    assert!(loaded.check(None).unwrap().is_clean());
    loaded.close().unwrap();
    db.close().unwrap();
}

#[test]
fn search_highlights_prefix_bm25f_unicode_lists_and_fusion() {
    let tmp = tempdir().unwrap();
    let db_dir = tmp.path().join("hl_db");
    let db = Database::open(
        &db_dir,
        OpenOptions {
            durability: DurabilityMode::Unsafe,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .unwrap();

    let mut fields = BTreeMap::new();
    fields.insert("text".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    fields.insert(
        "tags".into(),
        FieldDefinition { kind: FieldSpec::List { of: Box::new(FieldSpec::String) }, optional: true, nullable: false },
    );
    fields.insert("ts".into(), FieldDefinition { kind: FieldSpec::Integer, optional: false, nullable: false });
    let mut indexes = BTreeMap::new();
    indexes.insert("fts".into(), IndexDefinition::Text { fields: vec![("text".into(), 3), ("tags".into(), 1)] });
    let mut types = BTreeMap::new();
    types.insert("msg".into(), TypeDefinition { fields, indexes, ..Default::default() });
    db.set_schema(&Schema { dollar_schema: None, types, relations: BTreeMap::new() }).unwrap();

    // Surrogate-pair emoji (2 UTF-16 code units each!) + BMP accent + list field
    let emoji_text = "Hi 👋 café authentication 🚀 tokenized!";
    db.execute_batch(&[
        WireOp::Create {
            type_name: "msg".into(),
            external_id: Some("m1".into()),
            fields: vec![
                ("text".into(), CanonicalValue::String(emoji_text.into())),
                (
                    "tags".into(),
                    CanonicalValue::Array(vec![
                        CanonicalValue::String("unrelated".into()),
                        CanonicalValue::String("auth-flow".into()),
                    ]),
                ),
                ("ts".into(), CanonicalValue::Int(1_700_000_000_000)),
            ],
        },
        WireOp::Create {
            type_name: "msg".into(),
            external_id: Some("m2".into()),
            fields: vec![
                ("text".into(), CanonicalValue::String("Authentication without the second word".into())),
                ("ts".into(), CanonicalValue::Int(1_700_000_001_000)),
            ],
        },
    ])
    .unwrap();

    // Prefix BM25F with require_all: true matches only m1 ("auth*" AND "tok*")!
    // Fused with RecencyDecay + lazy highlights + projected row in one snapshot!
    let q = QuerySpec {
        source: QuerySource::Type { type_name: "msg".into(), include_trashed: false },
        steps: vec![QueryStep::Score {
            spec: QueryScore::Fuse {
                inputs: vec![
                    (
                        QueryScore::Bm25f {
                            index: "fts".into(),
                            query: "auth tok".into(),
                            require_all: true,
                            prefix: true,
                            phrase: None,
                            top_k: Some(10),
                        },
                        2.0,
                    ),
                    (
                        QueryScore::RecencyDecay {
                            field: "ts".into(),
                            now_ms: 1_700_000_002_000,
                            half_life_ms: 10_000.0,
                        },
                        0.5,
                    ),
                ],
                method: "rrf".into(),
                rrf_k: Some(60.0),
            },
        }],
        terminal: Some(QueryTerminal::Hits { fields: Some(vec!["text".into(), "ts".into()]), highlights: true }),
        explain: false,
    };

    let ans = db.execute_query_spec(&q, ExecBudgets::default()).unwrap();
    let QueryAnswerBody::Hits { hits } = ans.body else {
        panic!("expected Hits");
    };
    let h1 = hits.iter().find(|h| h.id == "m1").expect("m1 in fused hits");
    let row = h1.row.as_ref().expect("projected row");
    assert_eq!(row.fields.get("text"), Some(&serde_json::json!(emoji_text)));
    assert_eq!(row.fields.get("ts"), Some(&serde_json::json!(1_700_000_000_000i64)));
    assert!(!row.fields.contains_key("tags"), "unprojected field must not be in Hit.row");

    let hl = h1.highlights.as_ref().expect("highlights map");
    let text_spans = hl.get("text").expect("text highlights");
    // Verify exact UTF-16 code-unit slicing matches "authentication" and "tokenized"!
    let utf16: Vec<u16> = emoji_text.encode_utf16().collect();
    let sliced: Vec<String> = text_spans
        .iter()
        .map(|&(el, s, e)| {
            assert_eq!(el, 0);
            String::from_utf16(&utf16[s..e]).unwrap()
        })
        .collect();
    assert_eq!(sliced, vec!["authentication", "tokenized"]);

    // List field elementIndex == 1 ("auth-flow" -> "auth")
    let tag_spans = hl.get("tags").expect("tags highlights");
    assert_eq!(tag_spans.len(), 1);
    assert_eq!(tag_spans[0].0, 1, "highlight must carry list elementIndex = 1");
    let tag1_u16: Vec<u16> = "auth-flow".encode_utf16().collect();
    assert_eq!(String::from_utf16(&tag1_u16[tag_spans[0].1..tag_spans[0].2]).unwrap(), "auth");

    // Prefix require_all: true WITHOUT fusion returns strictly m1 (excludes m2!)
    let q_strict = QuerySpec {
        source: QuerySource::Type { type_name: "msg".into(), include_trashed: false },
        steps: vec![QueryStep::Score {
            spec: QueryScore::Bm25f {
                index: "fts".into(),
                query: "auth tok".into(),
                require_all: true,
                prefix: true,
                phrase: None,
                top_k: Some(10),
            },
        }],
        terminal: Some(QueryTerminal::Hits { fields: None, highlights: false }),
        explain: false,
    };
    let QueryAnswerBody::Hits { hits: strict_hits } =
        db.execute_query_spec(&q_strict, ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Hits");
    };
    assert_eq!(strict_hits.len(), 1);
    assert_eq!(strict_hits[0].id, "m1");
    assert!(strict_hits[0].row.is_none());
    assert!(strict_hits[0].highlights.is_none());

    db.close().unwrap();
}

#[test]
fn review_regressions_atomic_batch_in_transact_lock_guards_noop_seq_cross_proc_and_code_phrase() {
    let tmp = tempdir().unwrap();
    let db_dir = tmp.path().join("rev_db");

    let mut fields = BTreeMap::new();
    fields.insert("code".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    fields.insert("body".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    let mut indexes = BTreeMap::new();
    indexes.insert("uniq_code".into(), IndexDefinition::Value { fields: vec!["code".into()], unique: true });
    indexes.insert("fts".into(), IndexDefinition::Text { fields: vec![("body".into(), 1)] });
    let mut types = BTreeMap::new();
    types.insert("Ticket".into(), TypeDefinition { fields, indexes, ..Default::default() });
    let schema = Schema { dollar_schema: None, types, relations: BTreeMap::new() };

    let db = Database::open(
        &db_dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .unwrap();
    db.set_schema(&schema).unwrap();

    // P1 #7: Empty no-op batch must NOT return a phantom incremented commit_seq!
    let seq_before = db.stats().unwrap().commit_seq;
    let noop_out = db.execute_batch(&[]).unwrap();
    assert_eq!(noop_out.commit_seq, seq_before, "empty batch must not return phantom commit_seq");

    db.execute_batch(&[WireOp::Create {
        type_name: "Ticket".into(),
        external_id: Some("t1".into()),
        fields: vec![
            ("code".into(), CanonicalValue::String("C-1".into())),
            ("body".into(), CanonicalValue::String("Fix THR-12 error immediately".into())),
        ],
    }])
    .unwrap();
    db.execute_batch(&[WireOp::Create {
        type_name: "Ticket".into(),
        external_id: Some("t2".into()),
        fields: vec![
            ("code".into(), CanonicalValue::String("C-2".into())),
            ("body".into(), CanonicalValue::String("Mentioned THR 12 error with space".into())),
        ],
    }])
    .unwrap();

    // P1 #5: Compound code phrase ["THR-12", "error"] matches "THR-12 error" (not "THR 12 error") and highlights it!
    let q_phrase = QuerySpec {
        source: QuerySource::Type { type_name: "Ticket".into(), include_trashed: false },
        steps: vec![QueryStep::Score {
            spec: QueryScore::Bm25f {
                index: "fts".into(),
                query: "THR-12 error".into(),
                require_all: true,
                prefix: false,
                phrase: Some(vec!["THR-12".into(), "error".into()]),
                top_k: Some(5),
            },
        }],
        terminal: Some(QueryTerminal::Hits { fields: Some(vec!["body".into()]), highlights: true }),
        explain: false,
    };
    let QueryAnswerBody::Hits { hits } = db.execute_query_spec(&q_phrase, ExecBudgets::default()).unwrap().body else {
        panic!("expected Hits");
    };
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "t1");
    let body_text = "Fix THR-12 error immediately";
    let body_hl = hits[0].highlights.as_ref().unwrap().get("body").unwrap();
    assert!(
        body_hl.iter().any(|&(el, s, e)| el == 0 && &body_text[s..e] == "THR-12 error"),
        "expected full compound code phrase highlight, got {body_hl:?}"
    );

    // P0 #1 + P0 #2: Failing UniqueViolation execute_batch caught inside transact_scoped must NOT corrupt
    // primary vs derived indexes, and forbidden lock-order methods return InvalidArgument cleanly!
    let dump_tmp = tmp.path().join("nope.json");
    db.transact_scoped(elmdb::TransactOptions::default(), || {
        // Forbidden methods inside transact must fail with InvalidArgument (zero deadlock / lock-order panic!)
        assert_eq!(db.acknowledge_restore().unwrap_err().code, ErrorCode::InvalidArgument);
        assert_eq!(db.stats().unwrap_err().code, ErrorCode::InvalidArgument);
        assert_eq!(db.check(None).unwrap_err().code, ErrorCode::InvalidArgument);
        assert_eq!(db.index_status().unwrap_err().code, ErrorCode::InvalidArgument);
        assert_eq!(
            db.register_query_watch(&q_phrase, WatchPolicy::default(), 0).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(elmdb::dump::dump(&db, &dump_tmp).unwrap_err().code, ErrorCode::InvalidArgument);
        assert_eq!(db.close().unwrap_err().code, ErrorCode::InvalidArgument);

        // Execute a batch whose 2nd op violates unique index `uniq_code` ("C-1") -> catches error cleanly!
        let err = db
            .execute_batch(&[
                WireOp::Create {
                    type_name: "Ticket".into(),
                    external_id: Some("t-partial".into()),
                    fields: vec![
                        ("code".into(), CanonicalValue::String("C-PARTIAL".into())),
                        ("body".into(), CanonicalValue::String("partial".into())),
                    ],
                },
                WireOp::Create {
                    type_name: "Ticket".into(),
                    external_id: Some("t-dup".into()),
                    fields: vec![
                        ("code".into(), CanonicalValue::String("C-1".into())),
                        ("body".into(), CanonicalValue::String("dup".into())),
                    ],
                },
            ])
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::UniqueViolation);
        assert!(db.get_row_resolved("t-partial")?.is_none(), "op[0] must be rolled back on UniqueViolation");
        assert!(db.get_row_resolved("t-dup")?.is_none());

        // Subsequent valid write in the same outer transaction commits cleanly!
        db.execute_batch(&[WireOp::Create {
            type_name: "Ticket".into(),
            external_id: Some("t-ok".into()),
            fields: vec![
                ("code".into(), CanonicalValue::String("C-OK".into())),
                ("body".into(), CanonicalValue::String("ok".into())),
            ],
        }])?;
        Ok(())
    })
    .unwrap();

    assert!(db.get_row_resolved("t-partial").unwrap().is_none());
    assert!(db.get_row_resolved("t-ok").unwrap().is_some());
    assert!(db.check(None).unwrap().is_clean(), "derived indexes must be 100% consistent after caught UniqueViolation");

    // P0-5: Second in-process open of the same directory MUST fail with `DatabaseLocked`!
    let dup_err = Database::open(
        &db_dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .err()
    .expect("second in-process open must fail with DatabaseLocked");
    assert_eq!(dup_err.code, ErrorCode::DatabaseLocked);

    // P0-4 / P1 #10: Real child process adds a new index while THIS process holds `db` open,
    // and THIS already-open handle sees the new index and writes inside a nested savepoint cleanly!
    let mut schema2 = schema.clone();
    schema2
        .types
        .get_mut("Ticket")
        .unwrap()
        .indexes
        .insert("by_body_val".into(), IndexDefinition::Value { fields: vec!["body".into()], unique: false });
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_add_ticket_body_index_helper", "--ignored", "--nocapture"])
        .env("ELMDB_TEST_E2E_DIR", &db_dir)
        .env("ELMDB_TEST_E2E_SCHEMA", serde_json::to_string(&schema2).unwrap())
        .status()
        .unwrap();
    assert!(status.success(), "child schema evolution failed: {status}");

    db.transact_scoped(elmdb::TransactOptions::default(), || {
        db.transact_scoped(elmdb::TransactOptions::default(), || {
            db.execute_batch(&[WireOp::Create {
                type_name: "Ticket".into(),
                external_id: Some("t-xproc".into()),
                fields: vec![
                    ("code".into(), CanonicalValue::String("C-XPROC".into())),
                    ("body".into(), CanonicalValue::String("cross process nested".into())),
                ],
            }])?;
            Ok(())
        })?;
        Ok(())
    })
    .unwrap();
    assert!(db.get_row_resolved("t-xproc").unwrap().is_some());
    assert!(db.check(None).unwrap().is_clean());

    db.close().unwrap();
}

#[test]
#[ignore = "child process helper invoked by review_regressions_atomic_batch_in_transact_lock_guards_noop_seq_cross_proc_and_code_phrase"]
fn child_add_ticket_body_index_helper() {
    let db_dir = std::path::PathBuf::from(std::env::var("ELMDB_TEST_E2E_DIR").expect("ELMDB_TEST_E2E_DIR"));
    let schema2: elmdb::protocol::Schema =
        serde_json::from_str(&std::env::var("ELMDB_TEST_E2E_SCHEMA").expect("ELMDB_TEST_E2E_SCHEMA")).unwrap();
    let db = Database::open(
        &db_dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .unwrap();
    db.set_schema(&schema2).unwrap();
    db.close().unwrap();
}

#[test]
fn batch1_dump_load_refs_list_refs_archived_fields_timestamps_and_protocol_guards() {
    use elmdb::protocol::{
        FieldDefinition, FieldSpec, ProtocolOnDelete, QueryFilter, QuerySource, QuerySpec, QueryStep, QueryTerminal,
        RelationDefinition, RelationEndpoint, Schema, TypeDefinition, compiled_to_raw_schema, json_to_canonical,
    };

    let tmp = tempfile::tempdir().unwrap();
    let db_dir = tmp.path().join("src_db");
    let dump_path = tmp.path().join("full.dump.json");
    let loaded_dir = tmp.path().join("loaded_db");

    let mut schema = Schema::default();
    let mut project = TypeDefinition::default();
    project.fields.insert("name".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    schema.types.insert("Project".into(), project);

    let mut other = TypeDefinition::default();
    other.fields.insert("label".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    schema.types.insert("Other".into(), other);

    let mut task = TypeDefinition::default();
    task.fields.insert("title".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    task.fields.insert("legacy".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    task.fields.insert(
        "parent".into(),
        FieldDefinition {
            kind: FieldSpec::Ref { to: vec!["Project".into()], on_delete: ProtocolOnDelete::Restrict },
            optional: false,
            nullable: false,
        },
    );
    task.fields.insert(
        "optParent".into(),
        FieldDefinition {
            kind: FieldSpec::Ref { to: vec!["Project".into()], on_delete: ProtocolOnDelete::SetNull },
            optional: true,
            nullable: true,
        },
    );
    task.fields.insert(
        "danglingRef".into(),
        FieldDefinition {
            kind: FieldSpec::Ref { to: vec!["Project".into()], on_delete: ProtocolOnDelete::AllowDangling },
            optional: true,
            nullable: true,
        },
    );
    // Required List<Ref> field (P0-3)
    task.fields.insert(
        "deps".into(),
        FieldDefinition {
            kind: FieldSpec::List {
                of: Box::new(FieldSpec::Ref { to: vec!["Task".into()], on_delete: ProtocolOnDelete::Restrict }),
            },
            optional: false,
            nullable: false,
        },
    );
    schema.types.insert("Task".into(), task);
    schema.relations.insert(
        "REL_ACTIVE".into(),
        RelationDefinition {
            endpoints: vec![RelationEndpoint { source: "Task".into(), target: "Project".into() }],
            ..Default::default()
        },
    );
    schema.relations.insert(
        "REL_ARCHIVED".into(),
        RelationDefinition {
            endpoints: vec![RelationEndpoint { source: "Task".into(), target: "Project".into() }],
            ..Default::default()
        },
    );

    let db = Database::open(
        &db_dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .unwrap();
    db.set_schema(&schema).unwrap();

    // Create entities including forward/cyclic required List<Ref>, Restrict/SetNull refs, and a DanglingRef
    // whose external ID "reused-id" is later reused by an entity of a DIFFERENT type (`Other`).
    db.execute_batch(&[
        WireOp::Create {
            type_name: "Project".into(),
            external_id: Some("p-1".into()),
            fields: vec![("name".into(), CanonicalValue::String("Core".into()))],
        },
        WireOp::Create {
            type_name: "Project".into(),
            external_id: Some("reused-id".into()),
            fields: vec![("name".into(), CanonicalValue::String("Soon Deleted".into()))],
        },
        WireOp::Create {
            type_name: "Task".into(),
            external_id: Some("t-1".into()),
            fields: vec![
                ("title".into(), CanonicalValue::String("First".into())),
                ("legacy".into(), CanonicalValue::String("old-val".into())),
                ("parent".into(), CanonicalValue::String("p-1".into())),
                ("optParent".into(), CanonicalValue::String("p-1".into())),
                ("danglingRef".into(), CanonicalValue::String("reused-id".into())),
                ("deps".into(), CanonicalValue::Array(vec![])),
            ],
        },
        WireOp::Create {
            type_name: "Task".into(),
            external_id: Some("t-2".into()),
            fields: vec![
                ("title".into(), CanonicalValue::String("Second".into())),
                ("legacy".into(), CanonicalValue::String("old-2".into())),
                ("parent".into(), CanonicalValue::String("p-1".into())),
                ("deps".into(), CanonicalValue::Array(vec![CanonicalValue::String("t-1".into())])),
            ],
        },
        // Cyclic required List<Ref>: t-1 -> [t-2], t-2 -> [t-1]
        WireOp::Patch {
            target: EntityTarget::ExternalId("t-1".into()),
            fields: vec![("deps".into(), CanonicalValue::Array(vec![CanonicalValue::String("t-2".into())]))],
        },
        WireOp::Link {
            source: EntityTarget::ExternalId("t-1".into()),
            rel_kind: "REL_ACTIVE".into(),
            target: EntityTarget::ExternalId("p-1".into()),
            weight: None,
            info: None,
        },
    ])
    .unwrap();

    // Destroy Project "reused-id" (turning t-1.danglingRef into DanglingRef("reused-id")),
    // then create an Other entity with the exact same external ID "reused-id"!
    db.execute_batch(&[
        WireOp::Destroy { target: EntityTarget::ExternalId("reused-id".into()) },
        WireOp::Create {
            type_name: "Other".into(),
            external_id: Some("reused-id".into()),
            fields: vec![("label".into(), CanonicalValue::String("Different Type".into()))],
        },
    ])
    .unwrap();

    // Archive REQUIRED field `legacy` and relation `REL_ARCHIVED` (with ZERO archived types!),
    // then create `t-3` WITHOUT `legacy` to verify dump::load staging relaxes archived required fields!
    let mut evolved = schema.clone();
    evolved.types.get_mut("Task").unwrap().fields.remove("legacy");
    evolved.relations.remove("REL_ARCHIVED");
    db.set_schema(&evolved).unwrap();
    db.execute_batch(&[WireOp::Create {
        type_name: "Task".into(),
        external_id: Some("t-3".into()),
        fields: vec![
            ("title".into(), CanonicalValue::String("AfterArchive".into())),
            ("parent".into(), CanonicalValue::String("p-1".into())),
            ("deps".into(), CanonicalValue::Array(vec![])),
        ],
    }])
    .unwrap();

    // P1-5: `compiled_to_raw_schema` must omit archived fields & relations cleanly.
    let raw_roundtrip = compiled_to_raw_schema(&db.schema().unwrap());
    assert!(!raw_roundtrip.types["Task"].fields.contains_key("legacy"));
    assert!(!raw_roundtrip.relations.contains_key("REL_ARCHIVED"));

    let orig_t1 = db.get_row("t-1").unwrap().unwrap();
    let orig_edges = db.get_edges("t-1", Some("REL_ACTIVE"), elmdb::EdgeDir::Out).unwrap();
    assert_eq!(orig_edges.len(), 1);

    // Sleep slightly so any non-preserved timestamp during load would differ!
    std::thread::sleep(std::time::Duration::from_millis(15));

    elmdb::dump::dump(&db, &dump_path).unwrap();
    db.close().unwrap();

    elmdb::dump::load(&dump_path, &loaded_dir).unwrap();
    let loaded = Database::open(&loaded_dir, OpenOptions::default()).unwrap();

    // P0-2: Restrict & SetNull Ref fields MUST still be present with their exact values!
    let loaded_t1 = loaded.get_row("t-1").unwrap().unwrap();
    let loaded_t1_resolved = loaded.get_row_resolved("t-1").unwrap().unwrap();
    assert_eq!(loaded_t1_resolved.fields.get("parent"), Some(&serde_json::json!("p-1")));
    assert_eq!(loaded_t1_resolved.fields.get("optParent"), Some(&serde_json::json!("p-1")));
    assert_eq!(loaded_t1_resolved.fields.get("danglingRef"), Some(&serde_json::json!("reused-id")));
    assert_eq!(loaded_t1_resolved.fields.get("deps"), Some(&serde_json::json!(["t-2"])));
    // P2-5: Timestamps preserved losslessly across dump/load!
    assert_eq!(loaded_t1.created_at_ms, orig_t1.created_at_ms);
    assert_eq!(loaded_t1.updated_at_ms, orig_t1.updated_at_ms);
    assert_eq!(loaded_t1.trashed_at_ms, orig_t1.trashed_at_ms);
    let loaded_edges = loaded.get_edges("t-1", Some("REL_ACTIVE"), elmdb::EdgeDir::Out).unwrap();
    assert_eq!(loaded_edges.len(), 1);
    assert_eq!(loaded_edges[0].created_at_ms, orig_edges[0].created_at_ms);

    // P0-2: Restrict enforcement still active after load!
    let restrict_err =
        loaded.execute_batch(&[WireOp::Destroy { target: EntityTarget::ExternalId("p-1".into()) }]).unwrap_err();
    assert_eq!(restrict_err.code, ErrorCode::Referenced);

    // P0-3: Archived field & relation stay archived (not active!) in loaded schema!
    let loaded_schema = elmdb::protocol::compiled_to_protocol_schema(&loaded.schema().unwrap(), false);
    assert!(!loaded_schema.types["Task"].fields.contains_key("legacy"));
    assert!(!loaded_schema.relations.contains_key("REL_ARCHIVED"));

    // P1-5: Archiving `Other` still allows querying `Other` via QuerySource::Type AND QuerySource::Types + filter!
    let mut no_other = evolved.clone();
    no_other.types.remove("Other");
    loaded.set_schema(&no_other).unwrap();
    let ans = loaded
        .execute_query_spec(
            &QuerySpec {
                source: QuerySource::Type { type_name: "Other".into(), include_trashed: false },
                steps: vec![],
                terminal: Some(QueryTerminal::Ids),
                explain: false,
            },
            elmdb::ExecBudgets::default(),
        )
        .unwrap();
    match ans.body {
        elmdb::protocol::QueryAnswerBody::Ids { ids } => assert_eq!(ids, vec!["reused-id".to_owned()]),
        other => panic!("expected Ids, got {other:?}"),
    }
    let ans_multi = loaded
        .execute_query_spec(
            &QuerySpec {
                source: QuerySource::Types { type_names: vec!["Task".into(), "Other".into()], include_trashed: false },
                steps: vec![QueryStep::Filter {
                    expr: QueryFilter::Eq { field: "label".into(), value: serde_json::json!("Different Type") },
                }],
                terminal: Some(QueryTerminal::Ids),
                explain: false,
            },
            elmdb::ExecBudgets::default(),
        )
        .unwrap();
    match ans_multi.body {
        elmdb::protocol::QueryAnswerBody::Ids { ids } => assert_eq!(ids, vec!["reused-id".to_owned()]),
        other => panic!("expected Ids, got {other:?}"),
    }

    // P2-2: Deeply nested JSON (> MAX_VALUE_DEPTH) fails with ValidationFailed without stack overflow.
    let mut deep = serde_json::json!(1);
    for _ in 0..80 {
        deep = serde_json::json!([deep]);
    }
    let depth_err = json_to_canonical(&deep).unwrap_err();
    assert_eq!(depth_err.code, ErrorCode::ValidationFailed);
    let filter_err = loaded
        .execute_query_spec(
            &QuerySpec {
                source: QuerySource::Type { type_name: "Task".into(), include_trashed: false },
                steps: vec![QueryStep::Filter { expr: QueryFilter::Eq { field: "title".into(), value: deep } }],
                terminal: Some(QueryTerminal::Ids),
                explain: false,
            },
            elmdb::ExecBudgets::default(),
        )
        .unwrap_err();
    assert_eq!(filter_err.code, ErrorCode::ValidationFailed);

    assert!(loaded.check(None).unwrap().is_clean());
    loaded.close().unwrap();
}

#[test]
#[ignore = "child process helper invoked by batch2_map_growth_cross_proc_resize_sidecar_race_opguard_hooks_and_index_disambiguation"]
fn child_batch2_worker() {
    use elmdb::protocol::{FieldDefinition, FieldSpec, IndexDefinition, TypeDefinition, compiled_to_raw_schema};

    let db_dir = std::path::PathBuf::from(std::env::var("ELMDB_TEST_B2_DIR").expect("ELMDB_TEST_B2_DIR"));
    let action = std::env::var("ELMDB_TEST_B2_ACTION").expect("ELMDB_TEST_B2_ACTION");
    match action.as_str() {
        "grow_map" => {
            let db = Database::open(
                &db_dir,
                OpenOptions {
                    durability: DurabilityMode::Strict,
                    build_policy: BuildPolicy::EagerInSchemaChange,
                    map_size: Some(256 * 1024),
                    ..Default::default()
                },
            )
            .unwrap();
            // Deterministic incompressible LCG hex payload so zstd cannot shrink it below 256 KiB on 4 KiB Windows pages.
            let ops: Vec<WireOp> = (0..40u64)
                .map(|i| {
                    let mut state = 0xD1B5_4A32_D192_ED03u64.wrapping_mul(i + 1);
                    let mut body = String::with_capacity(16 * 1024);
                    for _ in 0..(16 * 1024 / 16) {
                        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                        use std::fmt::Write;
                        let _ = write!(&mut body, "{state:016x}");
                    }
                    WireOp::Create {
                        type_name: "Doc".into(),
                        external_id: Some(format!("child-big-{i}")),
                        fields: vec![
                            ("tag".into(), CanonicalValue::String(format!("t-{i}"))),
                            ("body".into(), CanonicalValue::String(body)),
                        ],
                    }
                })
                .collect();
            db.execute_batch(&ops).unwrap();
            assert!(db.stats().unwrap().map_size_bytes > 256 * 1024);
            db.close().unwrap();
        }
        "strict_race" => {
            let worker_id = std::env::var("ELMDB_TEST_B2_WORKER").expect("ELMDB_TEST_B2_WORKER");
            let db = Database::open(
                &db_dir,
                OpenOptions {
                    durability: DurabilityMode::Strict,
                    build_policy: BuildPolicy::EagerInSchemaChange,
                    ..Default::default()
                },
            )
            .unwrap();
            for i in 0..15 {
                db.execute_batch(&[WireOp::Create {
                    type_name: "Doc".into(),
                    external_id: Some(format!("race-{worker_id}-{i}")),
                    fields: vec![
                        ("tag".into(), CanonicalValue::String(format!("w{worker_id}"))),
                        ("body".into(), CanonicalValue::String(format!("payload-{i}"))),
                    ],
                }])
                .unwrap();
            }
            db.close().unwrap();
        }
        "schema_bump" => {
            let db = Database::open(
                &db_dir,
                OpenOptions {
                    durability: DurabilityMode::Strict,
                    build_policy: BuildPolicy::EagerInSchemaChange,
                    ..Default::default()
                },
            )
            .unwrap();
            let raw = compiled_to_raw_schema(&db.schema().unwrap());
            let mut schema = elmdb::protocol::Schema::default();
            for (tname, tdef) in raw.types {
                let mut td = TypeDefinition::default();
                for (fname, fdef) in tdef.fields {
                    let kind = match fdef.kind {
                        elmdb_engine::RawFieldKind::String => FieldSpec::String,
                        _ => continue,
                    };
                    td.fields.insert(fname, FieldDefinition { kind, optional: fdef.optional, nullable: fdef.nullable });
                }
                for (iname, idef) in tdef.indexes {
                    let def = match idef.kind {
                        elmdb_engine::RawIndexKind::Value { fields, unique } => {
                            IndexDefinition::Value { fields, unique }
                        }
                        elmdb_engine::RawIndexKind::Vector { dims, quantization, fields } => {
                            IndexDefinition::Vector { dims, quantization, fields }
                        }
                        _ => continue,
                    };
                    td.indexes.insert(iname, def);
                }
                schema.types.insert(tname, td);
            }
            let mut extra = TypeDefinition::default();
            extra
                .fields
                .insert("label".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
            extra
                .indexes
                .insert("by_label".into(), IndexDefinition::Value { fields: vec!["label".into()], unique: false });
            schema.types.insert("Extra".into(), extra);
            db.set_schema(&schema).unwrap();
            db.close().unwrap();
        }
        other => panic!("unknown child_batch2_worker action: {other}"),
    }
}

#[test]
fn batch2_map_growth_cross_proc_resize_sidecar_race_opguard_hooks_and_index_disambiguation() {
    use elmdb::protocol::{FieldDefinition, FieldSpec, IndexDefinition, Schema, TypeDefinition};
    use elmdb_engine::{BeforeCommitHook, TypeMask};
    use std::sync::{Arc, Barrier};

    let tmp = tempfile::tempdir().unwrap();

    // Build schema with two types (`Doc` and `Note`) sharing identical index names (`emb` and `by_tag`)
    // to exercise P1-3 cross-type disambiguation and TOCTOU-free `set_embeddings`.
    let mut schema = Schema::default();
    for tname in ["Doc", "Note"] {
        let mut td = TypeDefinition::default();
        td.fields.insert("tag".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
        td.fields.insert("body".into(), FieldDefinition { kind: FieldSpec::String, optional: true, nullable: false });
        td.indexes.insert("by_tag".into(), IndexDefinition::Value { fields: vec!["tag".into()], unique: false });
        td.indexes.insert(
            "emb".into(),
            IndexDefinition::Vector { dims: 4, quantization: "f32".into(), fields: vec!["body".into()] },
        );
        schema.types.insert(tname.into(), td);
    }

    // -------------------------------------------------------------------------
    // 1. P0-8: Tiny 256 KiB map auto-grows cleanly on single huge batch AND
    //    parent adopts cross-process `MDB_MAP_RESIZED` on both reads & writes!
    // -------------------------------------------------------------------------
    let growth_dir = tmp.path().join("growth_db");
    let db = Database::open(
        &growth_dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            map_size: Some(256 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    db.set_schema(&schema).unwrap();
    assert_eq!(db.stats().unwrap().map_size_bytes, 256 * 1024);

    // Spawn real child process that grows the map well past 256 KiB while parent stays open at 256 KiB!
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_batch2_worker", "--ignored", "--nocapture"])
        .env("ELMDB_TEST_B2_DIR", &growth_dir)
        .env("ELMDB_TEST_B2_ACTION", "grow_map")
        .status()
        .unwrap();
    assert!(status.success(), "child_batch2_worker grow_map failed: {status}");

    // Parent immediately runs `backup_into` BEFORE any other read — MUST transparently adopt `MDB_MAP_RESIZED` (P1-5)!
    let resized_backup = tmp.path().join("resized_backup");
    db.backup_into(&resized_backup, BackupInto::New).unwrap();
    // Parent also reads AND writes — MUST transparently adopt `MDB_MAP_RESIZED`!
    assert!(db.get_row_resolved("child-big-39").unwrap().is_some(), "parent read must adopt MDB_MAP_RESIZED");
    assert!(db.stats().unwrap().map_size_bytes > 256 * 1024);

    // Now force ANOTHER local `MDB_MAP_FULL` growth inside the parent in one large incompressible batch!
    let prev_map = db.stats().unwrap().map_size_bytes;
    let parent_big_ops: Vec<WireOp> = (0..64u64)
        .map(|i| {
            // Deterministic LCG hex payload so zstd cannot compress it down to a few bytes.
            let mut state = 0x9E37_79B9_7F4A_7C15u64.wrapping_mul(i + 1);
            let mut body = String::with_capacity(32 * 1024);
            for _ in 0..(32 * 1024 / 16) {
                state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                use std::fmt::Write;
                let _ = write!(&mut body, "{state:016x}");
            }
            WireOp::Create {
                type_name: "Doc".into(),
                external_id: Some(format!("parent-big-{i}")),
                fields: vec![
                    ("tag".into(), CanonicalValue::String("parent".into())),
                    ("body".into(), CanonicalValue::String(body)),
                ],
            }
        })
        .collect();
    db.execute_batch(&parent_big_ops).unwrap();
    assert!(db.stats().unwrap().map_size_bytes > prev_map, "parent must grow map_size on MDB_MAP_FULL");
    assert!(db.get_row_resolved("parent-big-63").unwrap().is_some());

    // -------------------------------------------------------------------------
    // 2. P1-3: Cross-type `set_embeddings` disambiguation, `check`/`rebuild`
    //    ambiguity rejection, and cross-process `schema()` / `index_status()` refresh!
    // -------------------------------------------------------------------------
    db.execute_batch(&[
        WireOp::Create {
            type_name: "Doc".into(),
            external_id: Some("doc-1".into()),
            fields: vec![("tag".into(), CanonicalValue::String("alpha".into()))],
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("note-1".into()),
            fields: vec![("tag".into(), CanonicalValue::String("beta".into()))],
        },
    ])
    .unwrap();
    // Unqualified `emb` resolves against each entity's own type atomically inside the write txn!
    db.set_embeddings("emb", "doc-1", &[vec![1.0, 0.0, 0.0, 0.0]]).unwrap();
    db.set_embeddings("emb", "note-1", &[vec![0.0, 1.0, 0.0, 0.0]]).unwrap();
    // Type-qualified wrong type fails cleanly instead of corrupting the wrong index!
    let mismatch = db.set_embeddings("Doc.emb", "note-1", &[vec![0.0, 0.0, 1.0, 0.0]]).unwrap_err();
    assert_eq!(mismatch.code, ErrorCode::EndpointMismatch);
    // Ambiguous unqualified `check`/`rebuild` is rejected; type-qualified succeeds!
    assert_eq!(db.check(Some("by_tag")).unwrap_err().code, ErrorCode::InvalidArgument);
    assert_eq!(db.rebuild(Some("by_tag")).unwrap_err().code, ErrorCode::InvalidArgument);
    assert!(db.check(Some("Doc.by_tag")).unwrap().is_clean());
    db.rebuild(Some("Note.by_tag")).unwrap();

    // Child bumps schema to add `Extra` type + `by_label` index; parent immediately sees it via `schema()` & `index_status()`!
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_batch2_worker", "--ignored", "--nocapture"])
        .env("ELMDB_TEST_B2_DIR", &growth_dir)
        .env("ELMDB_TEST_B2_ACTION", "schema_bump")
        .status()
        .unwrap();
    assert!(status.success(), "child_batch2_worker schema_bump failed: {status}");
    assert!(db.schema().unwrap().type_by_name("Extra").is_some(), "db.schema() must refresh cross-process epoch");
    assert!(
        db.index_status().unwrap().iter().any(|s| s.name == "by_label"),
        "db.index_status() must refresh cross-process epoch"
    );

    // -------------------------------------------------------------------------
    // 3. P1-2: Rust `beforeCommit` hook touching `&Database` returns clean
    //    `InvalidArgument` (zero BorrowMutError panic, zero WriterLock deadlock)!
    // -------------------------------------------------------------------------
    let db = Arc::new(db);
    let db_hook = Arc::clone(&db);
    let hook_checked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_checked_cl = Arc::clone(&hook_checked);
    let mut wants = TypeMask::empty();
    wants.insert(db.schema().unwrap().type_by_name("Doc").unwrap().id);
    let hook_id = db
        .register_before_commit(BeforeCommitHook {
            wants,
            callback: Arc::new(move |ctx| {
                // Re-entering `&Database` must return InvalidArgument, never panic or deadlock!
                assert_eq!(db_hook.get_row("doc-1").unwrap_err().code, ErrorCode::InvalidArgument);
                assert_eq!(
                    db_hook
                        .execute_batch(&[WireOp::Create {
                            type_name: "Doc".into(),
                            external_id: Some("reenter-fail".into()),
                            fields: vec![("tag".into(), CanonicalValue::String("x".into()))],
                        }])
                        .unwrap_err()
                        .code,
                    ErrorCode::InvalidArgument
                );
                assert_eq!(
                    db_hook.transact_scoped(elmdb::TransactOptions::default(), || Ok(())).unwrap_err().code,
                    ErrorCode::InvalidArgument
                );
                assert_eq!(db_hook.close().unwrap_err().code, ErrorCode::InvalidArgument);
                // `pending_commit_seq()` remains safely readable and matches `ctx.commit_seq()`!
                assert_eq!(db_hook.pending_commit_seq(), Some(ctx.commit_seq()));
                hook_checked_cl.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
        })
        .unwrap();
    db.execute_batch(&[WireOp::Create {
        type_name: "Doc".into(),
        external_id: Some("hook-trigger".into()),
        fields: vec![("tag".into(), CanonicalValue::String("hooked".into()))],
    }])
    .unwrap();
    assert!(hook_checked.load(std::sync::atomic::Ordering::SeqCst));
    assert!(db.unregister_before_commit(hook_id));

    // P1-7: Re-entering `&Database` inside `Database::transact(|tx| ...)` returns InvalidArgument (zero deadlock/panic).
    let db_reenter = Arc::clone(&db);
    db.transact(|_tx| {
        assert_eq!(db_reenter.get_row("doc-1").unwrap_err().code, ErrorCode::InvalidArgument);
        assert_eq!(db_reenter.close().unwrap_err().code, ErrorCode::InvalidArgument);
        assert_eq!(db_reenter.pending_commit_seq(), None);
        Ok(())
    })
    .unwrap();

    // P2-4: Backup writes & fsyncs `.elmdb-backup` marker and allows BackupInto::Backups overwrite.
    let backup_dir = tmp.path().join("b2_backup");
    db.backup_into(&backup_dir, BackupInto::New).unwrap();
    assert_eq!(std::fs::read_to_string(backup_dir.join(".elmdb-backup")).unwrap(), "elmdb-v2-backup\n");
    db.backup_into(&backup_dir, BackupInto::Backups).unwrap();

    db.close().unwrap();

    // -------------------------------------------------------------------------
    // 4. P0-6: Concurrent multi-process Strict writers racing on `synced.v2`
    //    sidecar never corrupt slots or regress `txnid`!
    // -------------------------------------------------------------------------
    let race_dir = tmp.path().join("sidecar_race_db");
    let race_db = Database::open(
        &race_dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .unwrap();
    race_db.set_schema(&schema).unwrap();

    let mut child1 = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_batch2_worker", "--ignored", "--nocapture"])
        .env("ELMDB_TEST_B2_DIR", &race_dir)
        .env("ELMDB_TEST_B2_ACTION", "strict_race")
        .env("ELMDB_TEST_B2_WORKER", "1")
        .spawn()
        .unwrap();
    let mut child2 = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_batch2_worker", "--ignored", "--nocapture"])
        .env("ELMDB_TEST_B2_DIR", &race_dir)
        .env("ELMDB_TEST_B2_ACTION", "strict_race")
        .env("ELMDB_TEST_B2_WORKER", "2")
        .spawn()
        .unwrap();
    for i in 0..15 {
        race_db
            .execute_batch(&[WireOp::Create {
                type_name: "Doc".into(),
                external_id: Some(format!("race-parent-{i}")),
                fields: vec![("tag".into(), CanonicalValue::String("parent".into()))],
            }])
            .unwrap();
    }
    assert!(child1.wait().unwrap().success());
    assert!(child2.wait().unwrap().success());
    race_db.close().unwrap();

    // Reopen MUST recover `Clean` with all 45 entities intact!
    let verified =
        Database::open(&race_dir, OpenOptions { durability: DurabilityMode::Strict, ..Default::default() }).unwrap();
    assert_eq!(verified.open_report().status, "clean");
    for prefix in ["race-parent", "race-1", "race-2"] {
        for i in 0..15 {
            assert!(verified.get_row_resolved(&format!("{prefix}-{i}")).unwrap().is_some());
        }
    }
    verified.close().unwrap();

    // -------------------------------------------------------------------------
    // 5. P0-7: `OpGuard::drop` drops `Arc<EngineHandle>` BEFORE `active_ops -= 1`,
    //    so immediate reopen after `close()` NEVER races with `DatabaseLocked`!
    // -------------------------------------------------------------------------
    let close_dir = tmp.path().join("opguard_close_db");
    for iter in 0..20 {
        let handle = Arc::new(
            Database::open(&close_dir, OpenOptions { durability: DurabilityMode::Unsafe, ..Default::default() })
                .unwrap_or_else(|e| panic!("reopen failed on iter {iter}: {e:?}")),
        );
        if iter == 0 {
            handle.set_schema(&schema).unwrap();
        }
        let barrier = Arc::new(Barrier::new(2));
        let h_read = Arc::clone(&handle);
        let b_read = Arc::clone(&barrier);
        let reader = std::thread::spawn(move || {
            b_read.wait();
            for _ in 0..5 {
                let _ = h_read.stats();
            }
        });
        barrier.wait();
        handle.close().unwrap();
        // Immediately reopen on the VERY NEXT LINE — `mdb_env_close()` MUST already have finished!
        let reopened =
            Database::open(&close_dir, OpenOptions { durability: DurabilityMode::Unsafe, ..Default::default() })
                .unwrap_or_else(|e| panic!("immediate post-close reopen raced on iter {iter}: {e:?}"));
        reader.join().unwrap();
        reopened.close().unwrap();
    }
}

#[test]
#[ignore = "child process helper invoked by batch3_watch_settlement_xproc_maint_future_ids_trashed_graph_trigram_phrase_and_order_schema"]
fn child_batch3_worker() {
    let dir = std::path::PathBuf::from(std::env::var("ELMDB_TEST_B3_DIR").expect("ELMDB_TEST_B3_DIR"));
    let mode = std::env::var("ELMDB_TEST_B3_MODE").unwrap_or_else(|_| "combined".into());
    let db = Database::open(
        &dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .unwrap();
    if mode == "maint_only" {
        // P0-1 review regression: maintenance-only write in child, followed by unrelated local commit in parent!
        db.set_embeddings("emb", "n2", &[vec![1.0, 0.0, 0.0, 0.0]]).unwrap();
    } else {
        // P0-11: Combine a normal data commit (bumps commit_seq += 1) AND a maintenance
        // write (`set_embeddings`, bumps maint_seq += 1 without bumping commit_seq) in one polling window!
        db.execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("a1".into()),
            fields: vec![("name".into(), CanonicalValue::String("Updated By Child".into()))],
        }])
        .unwrap();
        db.set_embeddings("emb", "n1", &[vec![1.0, 0.0, 0.0, 0.0]]).unwrap();
    }
    db.close().unwrap();
}

#[test]
fn batch3_watch_settlement_xproc_maint_future_ids_trashed_graph_trigram_phrase_and_order_schema() {
    let tmp = tempdir().unwrap();
    let db_dir = tmp.path().join("batch3_db");
    let db = Database::open(
        &db_dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .unwrap();

    // P2-7: Order index on non-String field (Any / Integer) MUST be rejected with InvalidSchema!
    let mut bad_any_types = BTreeMap::new();
    let mut bad_any_fields = BTreeMap::new();
    bad_any_fields.insert("payload".into(), FieldDefinition { kind: FieldSpec::Any, optional: true, nullable: true });
    let mut bad_any_indexes = BTreeMap::new();
    bad_any_indexes
        .insert("ord_payload".into(), IndexDefinition::Order { field: "payload".into(), locale: "en-US".into() });
    bad_any_types.insert(
        "Bad".into(),
        TypeDefinition { fields: bad_any_fields, indexes: bad_any_indexes, ..Default::default() },
    );
    let bad_order_any = Schema { dollar_schema: None, types: bad_any_types, relations: BTreeMap::new() };
    assert_eq!(db.set_schema(&bad_order_any).unwrap_err().code, ErrorCode::InvalidSchema);

    let mut bad_int_types = BTreeMap::new();
    let mut bad_int_fields = BTreeMap::new();
    bad_int_fields.insert("score".into(), FieldDefinition { kind: FieldSpec::Integer, optional: true, nullable: true });
    let mut bad_int_indexes = BTreeMap::new();
    bad_int_indexes
        .insert("ord_score".into(), IndexDefinition::Order { field: "score".into(), locale: "en-US".into() });
    bad_int_types.insert(
        "Bad".into(),
        TypeDefinition { fields: bad_int_fields, indexes: bad_int_indexes, ..Default::default() },
    );
    let bad_order_int = Schema { dollar_schema: None, types: bad_int_types, relations: BTreeMap::new() };
    assert_eq!(db.set_schema(&bad_order_int).unwrap_err().code, ErrorCode::InvalidSchema);

    // Build rich schema covering Author, Note (Text + Trigram + Vector + Ref), and Node (graph links).
    let mut types = BTreeMap::new();
    let mut author_fields = BTreeMap::new();
    author_fields.insert("name".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    types.insert("Author".into(), TypeDefinition { fields: author_fields, ..Default::default() });

    let mut note_fields = BTreeMap::new();
    note_fields.insert("body".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    note_fields.insert(
        "author".into(),
        FieldDefinition {
            kind: FieldSpec::Ref { to: vec!["Author".into()], on_delete: elmdb::protocol::ProtocolOnDelete::SetNull },
            optional: true,
            nullable: true,
        },
    );
    let mut note_indexes = BTreeMap::new();
    note_indexes.insert("txt".into(), IndexDefinition::Text { fields: vec![("body".into(), 1)] });
    note_indexes.insert("tri".into(), IndexDefinition::Trigram { fields: vec!["body".into()] });
    note_indexes.insert(
        "emb".into(),
        IndexDefinition::Vector { fields: vec!["body".into()], dims: 4, quantization: "sq8".into() },
    );
    types.insert("Note".into(), TypeDefinition { fields: note_fields, indexes: note_indexes, ..Default::default() });

    let mut node_fields = BTreeMap::new();
    node_fields.insert("label".into(), FieldDefinition { kind: FieldSpec::String, optional: false, nullable: false });
    types.insert("Node".into(), TypeDefinition { fields: node_fields, ..Default::default() });

    let mut relations = BTreeMap::new();
    relations.insert(
        "link".into(),
        RelationDefinition {
            endpoints: vec![RelationEndpoint { source: "Node".into(), target: "Node".into() }],
            ..Default::default()
        },
    );
    let schema = Schema { dollar_schema: None, types, relations };
    db.set_schema(&schema).unwrap();

    db.execute_batch(&[
        WireOp::Create {
            type_name: "Author".into(),
            external_id: Some("a1".into()),
            fields: vec![("name".into(), CanonicalValue::String("Alice".into()))],
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n1".into()),
            fields: vec![("body".into(), CanonicalValue::String("the quick brown fox jumps".into()))],
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n2".into()),
            fields: vec![("body".into(), CanonicalValue::String("quick unrelated brown ... fox".into()))],
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n_greek_mid".into()),
            fields: vec![("body".into(), CanonicalValue::String("ΑΟΣΑ".into()))],
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n_greek_end".into()),
            fields: vec![("body".into(), CanonicalValue::String("ΑΟΣ".into()))],
        },
        WireOp::Create {
            type_name: "Node".into(),
            external_id: Some("A".into()),
            fields: vec![("label".into(), CanonicalValue::String("A".into()))],
        },
        WireOp::Create {
            type_name: "Node".into(),
            external_id: Some("B".into()),
            fields: vec![("label".into(), CanonicalValue::String("B".into()))],
        },
        WireOp::Create {
            type_name: "Node".into(),
            external_id: Some("C".into()),
            fields: vec![("label".into(), CanonicalValue::String("C".into()))],
        },
        WireOp::Link {
            rel_kind: "link".into(),
            source: EntityTarget::ExternalId("A".into()),
            target: EntityTarget::ExternalId("B".into()),
            weight: None,
            info: None,
        },
        WireOp::Link {
            rel_kind: "link".into(),
            source: EntityTarget::ExternalId("B".into()),
            target: EntityTarget::ExternalId("C".into()),
            weight: None,
            info: None,
        },
    ])
    .unwrap();

    // -------------------------------------------------------------------------
    // 1. P1-8: Trigram Regex + Contains on Greek contextual final sigma (`ΑΟΣ` vs `ΑΟΣΑ`)
    // -------------------------------------------------------------------------
    for expr in [
        QueryFilter::Regex { field: "body".into(), pattern: "ΑΟΣ".into() },
        QueryFilter::Regex { field: "body".into(), pattern: "(?i)ΑΟΣ".into() },
        QueryFilter::Contains { field: "body".into(), needle: "ΑΟΣ".into() },
    ] {
        let q = QuerySpec {
            source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
            steps: vec![QueryStep::Filter { expr }],
            terminal: Some(QueryTerminal::Ids),
            explain: true,
        };
        let ans = db.execute_query_spec(&q, ExecBudgets::default()).unwrap();
        let QueryAnswerBody::Ids { mut ids } = ans.body else { panic!("expected Ids") };
        ids.sort();
        assert_eq!(ids, vec!["n_greek_end", "n_greek_mid"], "Greek trigram query must match both ΑΟΣ and ΑΟΣΑ");
    }

    // -------------------------------------------------------------------------
    // 2. P1-9: Phrase-only BM25F search (`query: ""` + `phrase: [...]`) returns matching docs
    // -------------------------------------------------------------------------
    for empty_q in ["", "   "] {
        let q = QuerySpec {
            source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
            steps: vec![QueryStep::Score {
                spec: QueryScore::Bm25f {
                    index: "txt".into(),
                    query: empty_q.into(),
                    require_all: false,
                    prefix: false,
                    phrase: Some(vec!["quick".into(), "brown".into()]),
                    top_k: Some(10),
                },
            }],
            terminal: Some(QueryTerminal::Ids),
            explain: false,
        };
        let QueryAnswerBody::Ids { ids } = db.execute_query_spec(&q, ExecBudgets::default()).unwrap().body else {
            panic!("expected Ids");
        };
        assert_eq!(ids, vec!["n1"], "phrase-only BM25F search must return exact phrase match");
    }

    // -------------------------------------------------------------------------
    // 3. P0-10: Maintenance writes (`setEmbeddings`, `driveBackgroundWork`, `flush`, `rebuild`)
    //    invalidate affected watches ONCE and settle cleanly (`watches_settled() == true`)!
    // -------------------------------------------------------------------------
    let vec_spec = QuerySpec {
        source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
        steps: vec![QueryStep::Score {
            spec: QueryScore::VectorNear { index: "emb".into(), vector: vec![1.0, 0.0, 0.0, 0.0], top_k: 5 },
        }],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    let fast_policy = WatchPolicy { min_gap_ms: 0, adaptive_multiplier: 0, ..Default::default() };
    let (w_vec, init_vec) = db.register_query_watch(&vec_spec, fast_policy, 1000).unwrap();
    let QueryAnswerBody::Ids { ids } = init_vec.body else { panic!("expected Ids") };
    assert!(ids.is_empty());
    assert!(db.watches_settled());

    let mut now = 2000u64;
    // (a) set_embeddings
    db.set_embeddings("emb", "n2", &[vec![0.0, 1.0, 0.0, 0.0]]).unwrap();
    assert!(!db.watches_settled());
    assert!(db.begin_watch_run(w_vec));
    let _ = db.rerun_query_watch(w_vec, &vec_spec, now, 1).unwrap();
    assert!(db.watches_settled(), "watch MUST settle after single rerun following setEmbeddings (no livelock!)");
    assert!(db.tick_watches(now + 100).due_ids.is_empty());

    // (b) drive_background_work
    now += 1000;
    let _ = db.drive_background_work(64).unwrap();
    if !db.watches_settled() {
        assert!(db.begin_watch_run(w_vec));
        let _ = db.rerun_query_watch(w_vec, &vec_spec, now, 1).unwrap();
    }
    assert!(db.watches_settled(), "watch MUST settle after driveBackgroundWork");

    // (c) flush
    now += 1000;
    db.flush().unwrap();
    if !db.watches_settled() {
        assert!(db.begin_watch_run(w_vec));
        let _ = db.rerun_query_watch(w_vec, &vec_spec, now, 1).unwrap();
    }
    assert!(db.watches_settled(), "watch MUST settle after flush");

    // (d) rebuild
    now += 1000;
    db.rebuild(None).unwrap();
    assert!(!db.watches_settled());
    assert!(db.begin_watch_run(w_vec));
    let _ = db.rerun_query_watch(w_vec, &vec_spec, now, 1).unwrap();
    assert!(db.watches_settled(), "watch MUST settle after single rerun following rebuild (no livelock!)");

    // -------------------------------------------------------------------------
    // 4. P0-11: Cross-process normal commit + maintenance write in same poll window!
    // -------------------------------------------------------------------------
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_batch3_worker", "--ignored", "--nocapture"])
        .env("ELMDB_TEST_B3_DIR", &db_dir)
        .status()
        .unwrap();
    assert!(status.success(), "child_batch3_worker failed: {status}");

    now += 1000;
    let dirtied = db.catch_up_cross_process(now).unwrap();
    assert!(dirtied.contains(&w_vec), "cross-process normal commit + setEmbeddings MUST dirty Note vector watch!");
    assert!(db.begin_watch_run(w_vec));
    let (ans_after_child, delivered) = db.rerun_query_watch(w_vec, &vec_spec, now + 10, 1).unwrap();
    assert!(delivered);
    let QueryAnswerBody::Ids { ids } = ans_after_child.body else { panic!("expected Ids") };
    assert!(ids.contains(&"n1".to_string()), "reran vector watch must see child's embedding for n1");
    assert!(db.watches_settled(), "watch MUST settle cleanly after cross-process maintenance rerun");

    // Review P0-1 regression: child performs maintenance-only `set_embeddings("emb", "n2", [1,0,0,0])`,
    // then parent performs an UNRELATED local write (`Patch Author("a1")`) BEFORE `catch_up_cross_process()`.
    // `notify_commit` MUST detect the maint_seq gap and dirty `w_vec` instead of swallowing it!
    let status2 = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_batch3_worker", "--ignored", "--nocapture"])
        .env("ELMDB_TEST_B3_DIR", &db_dir)
        .env("ELMDB_TEST_B3_MODE", "maint_only")
        .status()
        .unwrap();
    assert!(status2.success(), "child_batch3_worker(maint_only) failed: {status2}");

    db.execute_batch(&[WireOp::Patch {
        target: EntityTarget::ExternalId("a1".into()),
        fields: vec![("name".into(), CanonicalValue::String("Local Unrelated After Child Maint".into()))],
    }])
    .unwrap();
    now += 1000;
    let due_after_local = db.tick_watches(now).due_ids;
    assert!(
        due_after_local.contains(&w_vec),
        "unrelated local commit after cross-process maintenance write MUST detect maint_seq gap and dirty Note vector watch!"
    );
    assert!(db.begin_watch_run(w_vec));
    let _ = db.rerun_query_watch(w_vec, &vec_spec, now + 10, 1).unwrap();
    assert!(db.watches_settled(), "watch MUST settle cleanly after maint_seq-gap rerun");
    db.unregister_watch(w_vec);

    // -------------------------------------------------------------------------
    // 5. P0-12: Watches referencing not-yet-created external IDs (`ids`, `where(ref)`, `hasLink(target)`)
    // -------------------------------------------------------------------------
    let spec_future_ids = QuerySpec {
        source: QuerySource::Ids { ids: vec!["future_author".into()] },
        steps: vec![],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    let spec_future_ref = QuerySpec {
        source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
        steps: vec![QueryStep::Filter {
            expr: QueryFilter::Eq { field: "author".into(), value: serde_json::json!("future_author") },
        }],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    let spec_future_link = QuerySpec {
        source: QuerySource::Type { type_name: "Node".into(), include_trashed: false },
        steps: vec![QueryStep::Filter {
            expr: QueryFilter::HasLink {
                rel_kinds: vec!["link".into()],
                dir: "out".into(),
                target_id: Some("future_node".into()),
            },
        }],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };

    now += 1000;
    let (w_f_ids, _) = db.register_query_watch(&spec_future_ids, fast_policy, now).unwrap();
    let (w_f_ref, _) = db.register_query_watch(&spec_future_ref, fast_policy, now).unwrap();
    let (w_f_link, _) = db.register_query_watch(&spec_future_link, fast_policy, now).unwrap();
    assert!(db.watches_settled());

    db.execute_batch(&[
        WireOp::Create {
            type_name: "Author".into(),
            external_id: Some("future_author".into()),
            fields: vec![("name".into(), CanonicalValue::String("Future".into()))],
        },
        WireOp::Create {
            type_name: "Note".into(),
            external_id: Some("n_future_ref".into()),
            fields: vec![
                ("body".into(), CanonicalValue::String("note for future author".into())),
                ("author".into(), CanonicalValue::String("future_author".into())),
            ],
        },
        WireOp::Create {
            type_name: "Node".into(),
            external_id: Some("future_node".into()),
            fields: vec![("label".into(), CanonicalValue::String("FutureNode".into()))],
        },
        WireOp::Link {
            rel_kind: "link".into(),
            source: EntityTarget::ExternalId("C".into()),
            target: EntityTarget::ExternalId("future_node".into()),
            weight: None,
            info: None,
        },
    ])
    .unwrap();

    now += 1000;
    let due = db.tick_watches(now).due_ids;
    assert!(due.contains(&w_f_ids), "ids(['future_author']) watch must fire on creation");
    assert!(due.contains(&w_f_ref), "where(author == 'future_author') watch must fire on creation");
    assert!(due.contains(&w_f_link), "hasLink(target: 'future_node') watch must fire on creation");

    assert!(db.begin_watch_run(w_f_ids));
    let (ans_f_ids, _) = db.rerun_query_watch(w_f_ids, &spec_future_ids, now, 1).unwrap();
    let QueryAnswerBody::Ids { ids: got_ids } = ans_f_ids.body else { panic!("expected Ids") };
    assert_eq!(got_ids, vec!["future_author"]);

    assert!(db.begin_watch_run(w_f_ref));
    let (ans_f_ref, _) = db.rerun_query_watch(w_f_ref, &spec_future_ref, now, 1).unwrap();
    let QueryAnswerBody::Ids { ids: got_ref } = ans_f_ref.body else { panic!("expected Ids") };
    assert_eq!(got_ref, vec!["n_future_ref"]);

    assert!(db.begin_watch_run(w_f_link));
    let (ans_f_link, _) = db.rerun_query_watch(w_f_link, &spec_future_link, now, 1).unwrap();
    let QueryAnswerBody::Ids { ids: got_link } = ans_f_link.body else { panic!("expected Ids") };
    assert_eq!(got_link, vec!["C"]);

    db.unregister_watch(w_f_ids);
    db.unregister_watch(w_f_ref);
    db.unregister_watch(w_f_link);

    // -------------------------------------------------------------------------
    // 6. P0-14: Soft-trashed entities excluded from `hops`, `shortestPath`, `hasLink`, `degrees`
    //    when `includeTrashed: false` AND live watches fire when neighbor `B` is trashed!
    // -------------------------------------------------------------------------
    let make_hops = |inc_trashed: bool| QuerySpec {
        source: QuerySource::Type { type_name: "Node".into(), include_trashed: inc_trashed },
        steps: vec![
            QueryStep::Filter { expr: QueryFilter::Eq { field: "label".into(), value: serde_json::json!("A") } },
            QueryStep::Hops { rel_kinds: vec!["link".into()], dir: "out".into(), min_hops: 1, max_hops: 2 },
        ],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    let make_sp = |inc_trashed: bool, target: &str| QuerySpec {
        source: QuerySource::Type { type_name: "Node".into(), include_trashed: inc_trashed },
        steps: vec![
            QueryStep::Filter { expr: QueryFilter::Eq { field: "label".into(), value: serde_json::json!("A") } },
            QueryStep::ShortestPath {
                rel_kinds: vec!["link".into()],
                dir: "out".into(),
                target_id: target.into(),
                max_hops: 3,
            },
        ],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    let make_has_link_pushdown = |inc_trashed: bool| QuerySpec {
        source: QuerySource::Type { type_name: "Node".into(), include_trashed: inc_trashed },
        steps: vec![QueryStep::Filter {
            expr: QueryFilter::HasLink {
                rel_kinds: vec!["link".into()],
                dir: "out".into(),
                target_id: Some("B".into()),
            },
        }],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    let make_has_link_any = |inc_trashed: bool| QuerySpec {
        source: QuerySource::Type { type_name: "Node".into(), include_trashed: inc_trashed },
        steps: vec![
            QueryStep::Filter { expr: QueryFilter::Eq { field: "label".into(), value: serde_json::json!("A") } },
            QueryStep::Filter {
                expr: QueryFilter::HasLink { rel_kinds: vec!["link".into()], dir: "out".into(), target_id: None },
            },
        ],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    let make_degrees = |inc_trashed: bool| QuerySpec {
        source: QuerySource::Type { type_name: "Node".into(), include_trashed: inc_trashed },
        steps: vec![QueryStep::Filter {
            expr: QueryFilter::Eq { field: "label".into(), value: serde_json::json!("A") },
        }],
        terminal: Some(QueryTerminal::Degrees { rel_kinds: vec!["link".into()], dir: "out".into() }),
        explain: false,
    };

    now += 1000;
    let (w_hops, _) = db.register_query_watch(&make_hops(false), fast_policy, now).unwrap();
    let (w_sp, _) = db.register_query_watch(&make_sp(false, "C"), fast_policy, now).unwrap();
    let (w_hl, _) = db.register_query_watch(&make_has_link_pushdown(false), fast_policy, now).unwrap();
    let (w_deg, _) = db.register_query_watch(&make_degrees(false), fast_policy, now).unwrap();

    // Trash intermediate node B (`A -> B -> C`)!
    db.execute_batch(&[WireOp::Trash { target: EntityTarget::ExternalId("B".into()) }]).unwrap();

    now += 1000;
    let due_graph = db.tick_watches(now).due_ids;
    assert!(due_graph.contains(&w_hops), "trashing neighbor B must invalidate hops watch");
    assert!(due_graph.contains(&w_sp), "trashing neighbor B must invalidate shortestPath watch");
    assert!(due_graph.contains(&w_hl), "trashing target B must invalidate hasLink watch");
    assert!(due_graph.contains(&w_deg), "trashing neighbor B must invalidate degrees watch");
    db.unregister_watch(w_hops);
    db.unregister_watch(w_sp);
    db.unregister_watch(w_hl);
    db.unregister_watch(w_deg);

    // With includeTrashed: false -> B is excluded AND not traversed through to C!
    let QueryAnswerBody::Ids { ids: h_live } =
        db.execute_query_spec(&make_hops(false), ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Ids");
    };
    assert!(h_live.is_empty(), "hops(includeTrashed=false) must exclude trashed B and not traverse to C: {h_live:?}");

    let QueryAnswerBody::Ids { ids: sp_c_live } =
        db.execute_query_spec(&make_sp(false, "C"), ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Ids");
    };
    assert!(sp_c_live.is_empty(), "shortestPath(A->C) must not traverse through trashed B");

    let QueryAnswerBody::Ids { ids: sp_b_live } =
        db.execute_query_spec(&make_sp(false, "B"), ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Ids");
    };
    assert!(sp_b_live.is_empty(), "shortestPath(A->B) must exclude trashed target B");

    let QueryAnswerBody::Ids { ids: hl_push_live } =
        db.execute_query_spec(&make_has_link_pushdown(false), ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Ids");
    };
    assert!(hl_push_live.is_empty(), "hasLink(target: B) pushdown must exclude trashed B");

    let QueryAnswerBody::Ids { ids: hl_any_live } =
        db.execute_query_spec(&make_has_link_any(false), ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Ids");
    };
    assert!(hl_any_live.is_empty(), "hasLink(dir: out) must ignore edges to trashed B");

    let QueryAnswerBody::Degrees { degrees: deg_live } =
        db.execute_query_spec(&make_degrees(false), ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Degrees");
    };
    assert_eq!(deg_live, vec![("A".to_string(), 0)], "degrees(A) must not count trashed neighbor B");

    // With includeTrashed: true -> B IS visible and traversed to C!
    let QueryAnswerBody::Ids { ids: mut h_all } =
        db.execute_query_spec(&make_hops(true), ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Ids");
    };
    h_all.sort();
    assert_eq!(h_all, vec!["B", "C"]);

    let QueryAnswerBody::Ids { ids: sp_all } =
        db.execute_query_spec(&make_sp(true, "C"), ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Ids");
    };
    assert_eq!(sp_all, vec!["A", "B", "C"]);

    let QueryAnswerBody::Ids { ids: hl_all } =
        db.execute_query_spec(&make_has_link_pushdown(true), ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Ids");
    };
    assert_eq!(hl_all, vec!["A"]);

    let QueryAnswerBody::Degrees { degrees: deg_all } =
        db.execute_query_spec(&make_degrees(true), ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Degrees");
    };
    assert_eq!(deg_all, vec![("A".to_string(), 1)]);

    // Review P1-2 regression: `LiteralKeys` (`ids(["B"])`) where seed `B` is trashed must neither emit
    // `B` nor traverse out of `B` to `C` in `hops` or `shortestPath`!
    let trashed_seed_hops = QuerySpec {
        source: QuerySource::Ids { ids: vec!["B".into()] },
        steps: vec![QueryStep::Hops { rel_kinds: vec!["link".into()], dir: "out".into(), min_hops: 0, max_hops: 2 }],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    let QueryAnswerBody::Ids { ids: ts_hops } =
        db.execute_query_spec(&trashed_seed_hops, ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Ids");
    };
    assert!(ts_hops.is_empty(), "hops from trashed LiteralKeys seed must be empty, got {ts_hops:?}");

    let trashed_seed_sp = QuerySpec {
        source: QuerySource::Ids { ids: vec!["B".into()] },
        steps: vec![QueryStep::ShortestPath {
            rel_kinds: vec!["link".into()],
            dir: "out".into(),
            target_id: "C".into(),
            max_hops: 2,
        }],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    let QueryAnswerBody::Ids { ids: ts_sp } =
        db.execute_query_spec(&trashed_seed_sp, ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Ids");
    };
    assert!(ts_sp.is_empty(), "shortestPath from trashed LiteralKeys seed must be empty, got {ts_sp:?}");

    // -------------------------------------------------------------------------
    // 7. P1-11: Dropping and re-adding a Vector index produces clean empty recall
    //    (physical `dbs.embeddings` purge is directly asserted in `vector_and_rank_dst.rs`!)
    // -------------------------------------------------------------------------
    let mut dropped_vec_schema = schema.clone();
    dropped_vec_schema.types.get_mut("Note").unwrap().indexes.remove("emb");
    db.set_schema(&dropped_vec_schema).unwrap();
    db.set_schema(&schema).unwrap();
    let QueryAnswerBody::Ids { ids: after_readd } =
        db.execute_query_spec(&vec_spec, ExecBudgets::default()).unwrap().body
    else {
        panic!("expected Ids");
    };
    assert!(after_readd.is_empty(), "dropped vector index must have purged primary embeddings: {after_readd:?}");

    // 8. P2-3: `db.flush()` dirties a Vector watch ONLY when pending vector work actually flushed,
    //    and leaves settled watches untouched when idle!
    db.set_embeddings("emb", "n1", &[vec![1.0, 0.0, 0.0, 0.0]]).unwrap();
    let (idle_wid, _) = db
        .register_query_watch(
            &vec_spec,
            WatchPolicy { min_gap_ms: 0, adaptive_multiplier: 0, ..Default::default() },
            5000,
        )
        .unwrap();
    assert!(db.watches_settled());
    db.flush().unwrap();
    assert!(!db.watches_settled(), "flush() that drains pending vector work must dirty Vector watch");
    let _ = db.rerun_query_watch(idle_wid, &vec_spec, 5100, 1).unwrap();
    assert!(db.watches_settled());
    db.flush().unwrap();
    assert!(db.watches_settled(), "idle flush() with no vector work must not dirty settled watches");
    db.unregister_watch(idle_wid);

    // 9. Standalone counter writes (non-query LMDB txns) must NOT spuriously dirty settled watches
    //    either via catch_up_cross_process or on the next unrelated commit!
    let note_ids_spec = QuerySpec {
        source: QuerySource::Type { type_name: "Note".into(), include_trashed: false },
        steps: vec![],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    let (note_wid, _) = db
        .register_query_watch(
            &note_ids_spec,
            WatchPolicy { min_gap_ms: 0, adaptive_multiplier: 0, ..Default::default() },
            5200,
        )
        .unwrap();
    assert!(db.watches_settled());
    assert_eq!(db.next_counter("STANDALONE").unwrap(), 1);
    db.set_counter("STANDALONE", 42).unwrap();
    assert!(db.catch_up_cross_process(5250).unwrap().is_empty());
    assert!(db.watches_settled());
    db.execute_batch(&[WireOp::Patch {
        target: EntityTarget::ExternalId("a1".into()),
        fields: vec![("name".into(), CanonicalValue::String("Alice-updated".into()))],
    }])
    .unwrap();
    assert!(
        db.watches_settled(),
        "unrelated Author write after standalone counter writes must not trigger false-positive Note watch invalidation"
    );
    db.unregister_watch(note_wid);

    // 10. PR #261 coverage: `drive_background_work` (idle, Vector, Rank), `flush` (Rank), and
    //     `rename_type` / `rename_field` / `rename_relation` epoch advancement & watch invalidation.
    let mut rank_schema = schema.clone();
    rank_schema
        .types
        .get_mut("Node")
        .unwrap()
        .indexes
        .insert("pr".into(), IndexDefinition::Rank { relations: vec!["link".into()], weighted: false });
    db.set_schema(&rank_schema).unwrap();
    db.flush().unwrap();
    assert!(!db.drive_background_work(64).unwrap(), "idle drive_background_work must return false");

    // Vector pending -> drive_background_work drains it and returns true, then false when idle.
    db.set_embeddings("emb", "n1", &[vec![0.0, 1.0, 0.0, 0.0]]).unwrap();
    assert!(db.drive_background_work(64).unwrap(), "pending vector embedding must be drained by drive_background_work");
    assert!(!db.drive_background_work(64).unwrap(), "second drive_background_work after vector drain must be idle");

    // Rank dirty -> drive_background_work drains it and returns true, then false when converged.
    db.execute_batch(&[
        WireOp::Create {
            type_name: "Node".into(),
            external_id: Some("rk1".into()),
            fields: vec![("label".into(), CanonicalValue::String("RK1".into()))],
        },
        WireOp::Create {
            type_name: "Node".into(),
            external_id: Some("rk2".into()),
            fields: vec![("label".into(), CanonicalValue::String("RK2".into()))],
        },
        WireOp::Link {
            source: EntityTarget::ExternalId("rk1".into()),
            rel_kind: "link".into(),
            target: EntityTarget::ExternalId("rk2".into()),
            weight: None,
            info: None,
        },
    ])
    .unwrap();
    assert!(db.drive_background_work(64).unwrap(), "dirty Rank index must be drained by drive_background_work");
    assert!(!db.drive_background_work(64).unwrap(), "converged Rank index must return false");

    // Rank dirty + settled Rank watch -> `db.flush()` drains Rank and dirties the Rank watch!
    let rank_spec = QuerySpec {
        source: QuerySource::Type { type_name: "Node".into(), include_trashed: false },
        steps: vec![QueryStep::Score { spec: QueryScore::RankIndex { index: "pr".into() } }],
        terminal: Some(QueryTerminal::Ids),
        explain: false,
    };
    let (rk_wid, _) = db
        .register_query_watch(
            &rank_spec,
            WatchPolicy { min_gap_ms: 0, adaptive_multiplier: 0, ..Default::default() },
            5300,
        )
        .unwrap();
    assert!(db.watches_settled());
    db.execute_batch(&[WireOp::Link {
        source: EntityTarget::ExternalId("rk2".into()),
        rel_kind: "link".into(),
        target: EntityTarget::ExternalId("A".into()),
        weight: None,
        info: None,
    }])
    .unwrap();
    // Settle the watch before flush() so only Rank push drain in flush() can re-dirty it!
    let _ = db.rerun_query_watch(rk_wid, &rank_spec, 5350, 1).unwrap();
    assert!(db.watches_settled());
    db.flush().unwrap();
    assert!(!db.watches_settled(), "flush() draining Rank pushes must dirty Rank watch");
    let _ = db.rerun_query_watch(rk_wid, &rank_spec, 5400, 1).unwrap();
    assert!(db.watches_settled());

    // O(1) rename_type / rename_field / rename_relation advance schema epoch & invalidate active watches!
    let e0 = db.schema().unwrap().epoch;
    let e1 = db.rename_type("Author", "Writer", false).unwrap();
    assert!(e1 > e0, "rename_type must advance schema epoch");
    assert!(!db.watches_settled(), "rename_type must invalidate active watches");
    let _ = db.rerun_query_watch(rk_wid, &rank_spec, 5450, 1).unwrap();
    assert!(db.watches_settled());

    let e2 = db.rename_field("Writer", "name", "fullName").unwrap();
    assert!(e2 > e1, "rename_field must advance schema epoch");
    assert!(!db.watches_settled(), "rename_field must invalidate active watches");
    let _ = db.rerun_query_watch(rk_wid, &rank_spec, 5500, 1).unwrap();
    assert!(db.watches_settled());

    let e3 = db.rename_relation("link", "linksTo").unwrap();
    assert!(e3 > e2, "rename_relation must advance schema epoch");
    assert!(!db.watches_settled(), "rename_relation must invalidate active watches");
    assert!(db.schema().unwrap().types.iter().any(|t| t.name == "Writer"));
    assert!(db.schema().unwrap().relations.iter().any(|r| r.name == "linksTo"));
    db.unregister_watch(rk_wid);

    db.close().unwrap();
}
