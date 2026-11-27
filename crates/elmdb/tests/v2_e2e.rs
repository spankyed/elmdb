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

    // P1 #10: Second already-open handle sees new index added by first handle and writes inside nested savepoint!
    let db2 = Database::open(
        &db_dir,
        OpenOptions {
            durability: DurabilityMode::Strict,
            build_policy: BuildPolicy::EagerInSchemaChange,
            ..Default::default()
        },
    )
    .unwrap();
    let mut schema2 = schema.clone();
    schema2
        .types
        .get_mut("Ticket")
        .unwrap()
        .indexes
        .insert("by_body_val".into(), IndexDefinition::Value { fields: vec!["body".into()], unique: false });
    db.set_schema(&schema2).unwrap();

    db2.transact_scoped(elmdb::TransactOptions::default(), || {
        db2.transact_scoped(elmdb::TransactOptions::default(), || {
            db2.execute_batch(&[WireOp::Create {
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
    assert!(db2.get_row_resolved("t-xproc").unwrap().is_some());
    assert!(db2.check(None).unwrap().is_clean());

    db2.close().unwrap();
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
    task.fields.insert("legacy".into(), FieldDefinition { kind: FieldSpec::String, optional: true, nullable: true });
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

    // Archive field `legacy` and relation `REL_ARCHIVED` (with ZERO archived types!) to test P0-3 & P1-5.
    let mut evolved = schema.clone();
    evolved.types.get_mut("Task").unwrap().fields.remove("legacy");
    evolved.relations.remove("REL_ARCHIVED");
    db.set_schema(&evolved).unwrap();

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

    // P1-5: Archiving `Other` still allows querying `Other` via QuerySource::Type & QuerySource::Types!
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
