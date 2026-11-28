use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use elmdb_engine::{
    BeforeCommitHook, Cardinality, Engine, OnDeletePolicy, RawEndpoint, RawField, RawFieldKind, RawRelation, RawSchema,
    RawType, TypeMask,
};
use elmdb_lmdb::{LmdbConfig, LmdbStore};
use elmdb_sim::{SimClock, SimKvConfig, SimKvStore, SimRng, SimVfs};
use elmdb_storage::{BootId, DurabilityMode, OsVfs, RecoveryStatus};
use elmdb_wire::{CanonicalValue, CommitReturnFlags, ElmdbError, EntityTarget, ErrorCode, WireOp, encode_batch};
use tempfile::TempDir;

fn sample_schema() -> RawSchema {
    let mut types = BTreeMap::new();

    let mut project_fields = BTreeMap::new();
    project_fields.insert("name".into(), RawField { kind: RawFieldKind::String, optional: false, nullable: false });
    types.insert("Project".into(), RawType { fields: project_fields, additional_fields: false, ..RawType::default() });

    let mut task_fields = BTreeMap::new();
    task_fields.insert("title".into(), RawField { kind: RawFieldKind::String, optional: false, nullable: false });
    task_fields.insert("body".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: false });
    task_fields.insert("priority".into(), RawField { kind: RawFieldKind::Number, optional: true, nullable: true });
    task_fields.insert(
        "projectRestrict".into(),
        RawField {
            kind: RawFieldKind::Ref { to: vec!["Project".into()], on_delete: OnDeletePolicy::Restrict },
            optional: true,
            nullable: true,
        },
    );
    task_fields.insert(
        "projectSetNull".into(),
        RawField {
            kind: RawFieldKind::Ref { to: vec!["Project".into()], on_delete: OnDeletePolicy::SetNull },
            optional: true,
            nullable: true,
        },
    );
    task_fields.insert(
        "projectDangling".into(),
        RawField {
            kind: RawFieldKind::Ref { to: vec!["Project".into()], on_delete: OnDeletePolicy::AllowDangling },
            optional: true,
            nullable: true,
        },
    );
    types.insert("Task".into(), RawType { fields: task_fields, additional_fields: true, ..RawType::default() });

    let mut audit_fields = BTreeMap::new();
    audit_fields.insert("msg".into(), RawField { kind: RawFieldKind::String, optional: false, nullable: false });
    types.insert("Audit".into(), RawType { fields: audit_fields, ..RawType::default() });

    let mut relations = BTreeMap::new();
    relations.insert(
        "DEPENDS_ON".into(),
        RawRelation {
            cardinality: Cardinality::ManyToMany,
            acyclic: true,
            weighted: true,
            endpoints: vec![RawEndpoint { source: "Task".into(), target: "Task".into() }],
            declared_by: Some("core".into()),
        },
    );
    relations.insert(
        "IN_PROJECT".into(),
        RawRelation {
            cardinality: Cardinality::ManyToOne,
            acyclic: false,
            weighted: false,
            endpoints: vec![RawEndpoint { source: "Task".into(), target: "Project".into() }],
            declared_by: Some("core".into()),
        },
    );

    RawSchema { types, relations }
}

/// Minimal pure-Rust reference oracle verifying primary entity + field + blob + trash state.
#[derive(Debug, Default)]
struct ReferenceOracle {
    rows: BTreeMap<String, (String, bool, BTreeMap<String, CanonicalValue>)>,
}

#[test]
fn dst_engine_matches_reference_oracle_across_crud_blobs_and_reboots() {
    let mut rng = SimRng::seeded(0xC0FF_EE42);
    let dir = Path::new("/sim/engine_oracle");
    let store = SimKvStore::new(SimKvConfig::default());
    let vfs = SimVfs::new(BootId([0x55; 16]));
    let clock = SimClock::new(1_700_000_000_000);

    let (engine, _) = Engine::open(store.clone(), vfs.clone(), clock, dir, DurabilityMode::Strict).unwrap();
    engine.set_schema(&sample_schema()).unwrap();

    let mut oracle = ReferenceOracle::default();
    let mut ids: Vec<String> = Vec::new();

    for step in 0..80usize {
        engine.clock().advance_ms(10);
        let choice = rng.gen_range(5);
        if ids.is_empty() || choice == 0 {
            // Create a Task (alternating inline vs >4 KiB spilled blob body).
            let body = if step % 3 == 0 { "B".repeat(5200 + (step % 500)) } else { format!("short body {step}") };
            let priority = if step % 2 == 0 {
                CanonicalValue::Int(step as i64)
            } else {
                CanonicalValue::float(step as f64 + 0.5).unwrap()
            };
            let out = engine
                .execute_batch(&[WireOp::Create {
                    type_name: "Task".into(),
                    external_id: None,
                    fields: vec![
                        ("title".into(), CanonicalValue::String(format!("T{step}"))),
                        ("body".into(), CanonicalValue::String(body.clone())),
                        ("priority".into(), priority.clone()),
                        ("extraTag".into(), CanonicalValue::Bool(step % 2 == 0)),
                    ],
                }])
                .unwrap();
            let id = out.created_ids[0].clone();
            let mut map = BTreeMap::new();
            map.insert("title".into(), CanonicalValue::String(format!("T{step}")));
            map.insert("body".into(), CanonicalValue::String(body));
            map.insert("priority".into(), priority);
            map.insert("extraTag".into(), CanonicalValue::Bool(step % 2 == 0));
            oracle.rows.insert(id.clone(), ("Task".into(), false, map));
            ids.push(id);
        } else if choice == 1 {
            // Patch a live Task (including shrinking a >4 KiB blob back inline or growing inline -> blob).
            let id = ids[rng.gen_range(ids.len())].clone();
            if let Some((_, trashed, map)) = oracle.rows.get_mut(&id)
                && !*trashed
            {
                let new_body = if step % 2 == 0 { "tiny".into() } else { "L".repeat(6000) };
                engine
                    .execute_batch(&[WireOp::Patch {
                        target: EntityTarget::ExternalId(id.clone()),
                        fields: vec![("body".into(), CanonicalValue::String(new_body.clone()))],
                    }])
                    .unwrap();
                map.insert("body".into(), CanonicalValue::String(new_body));
            }
        } else if choice == 2 {
            // Unset optional field.
            let id = ids[rng.gen_range(ids.len())].clone();
            if let Some((_, trashed, map)) = oracle.rows.get_mut(&id)
                && !*trashed
            {
                engine
                    .execute_batch(&[WireOp::Unset {
                        target: EntityTarget::ExternalId(id.clone()),
                        fields: vec!["body".into()],
                    }])
                    .unwrap();
                map.remove("body");
            }
        } else if choice == 3 {
            // Trash or Restore toggle.
            let id = ids[rng.gen_range(ids.len())].clone();
            if let Some((_, trashed, _)) = oracle.rows.get_mut(&id) {
                if *trashed {
                    engine.execute_batch(&[WireOp::Restore { target: EntityTarget::ExternalId(id.clone()) }]).unwrap();
                    *trashed = false;
                } else {
                    engine.execute_batch(&[WireOp::Trash { target: EntityTarget::ExternalId(id.clone()) }]).unwrap();
                    *trashed = true;
                }
            }
        } else {
            // Destroy.
            let idx = rng.gen_range(ids.len());
            let id = ids.remove(idx);
            engine.execute_batch(&[WireOp::Destroy { target: EntityTarget::ExternalId(id.clone()) }]).unwrap();
            oracle.rows.remove(&id);
        }

        // Periodically crash + reboot and verify full state matches ReferenceOracle bit-for-bit.
        if step % 20 == 19 {
            store.crash_power_loss();
            vfs.crash_power_loss(&mut rng, true);
            let (reopened, report) =
                Engine::open(store.clone(), vfs.clone(), SimClock::new(1_700_000_000_000), dir, DurabilityMode::Strict)
                    .unwrap();
            assert_eq!(report.status, RecoveryStatus::RebootVerified);
            let rows = reopened.list_by_type("Task", true).unwrap();
            assert_eq!(rows.len(), oracle.rows.len());
            for row in rows {
                let (ty, trashed, expected_fields) = oracle.rows.get(&row.id).unwrap();
                assert_eq!(&row.type_name, ty);
                assert_eq!(row.trashed, *trashed);
                assert_eq!(&row.fields, expected_fields);
            }
        }
    }
}

#[test]
fn referential_integrity_restrict_set_null_and_allow_dangling() {
    let tmp = TempDir::new().unwrap();
    let lmdb = LmdbStore::open(tmp.path(), LmdbConfig { map_size: 8 * 1024 * 1024, ..LmdbConfig::default() }).unwrap();
    let (engine, _) =
        Engine::open(lmdb, OsVfs::new(), SimClock::default(), tmp.path(), DurabilityMode::Strict).unwrap();
    engine.set_schema(&sample_schema()).unwrap();

    // Create 3 projects + 1 task referencing all 3 under different onDelete policies.
    let p_out = engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Project".into(),
                external_id: Some("Project-restrict".into()),
                fields: vec![("name".into(), CanonicalValue::String("R".into()))],
            },
            WireOp::Create {
                type_name: "Project".into(),
                external_id: Some("Project-setnull".into()),
                fields: vec![("name".into(), CanonicalValue::String("N".into()))],
            },
            WireOp::Create {
                type_name: "Project".into(),
                external_id: Some("Project-dangling".into()),
                fields: vec![("name".into(), CanonicalValue::String("D".into()))],
            },
        ])
        .unwrap();
    assert_eq!(p_out.created_ids.len(), 3);

    let t_out = engine
        .execute_batch(&[WireOp::Create {
            type_name: "Task".into(),
            external_id: Some("Task-1".into()),
            fields: vec![
                ("title".into(), CanonicalValue::String("Check refs".into())),
                ("projectRestrict".into(), CanonicalValue::String("Project-restrict".into())),
                ("projectSetNull".into(), CanonicalValue::String("Project-setnull".into())),
                ("projectDangling".into(), CanonicalValue::String("Project-dangling".into())),
            ],
        }])
        .unwrap();
    assert_eq!(t_out.created_ids, vec!["Task-1"]);

    // 1. Destroying `Project-restrict` MUST fail with ErrorCode::Referenced!
    let err = engine
        .execute_batch(&[WireOp::Destroy { target: EntityTarget::ExternalId("Project-restrict".into()) }])
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Referenced);
    assert!(engine.get_row("Project-restrict").unwrap().is_some());

    // 2. Destroying `Project-setnull` atomically nulls `Task-1.projectSetNull`!
    engine.execute_batch(&[WireOp::Destroy { target: EntityTarget::ExternalId("Project-setnull".into()) }]).unwrap();
    let row = engine.get_row("Task-1").unwrap().unwrap();
    assert_eq!(row.fields.get("projectSetNull"), Some(&CanonicalValue::Null));

    // 3. Destroying `Project-dangling` atomically rewrites EntityRef -> DanglingRef("Project-dangling")
    //    so the external ID string is NEVER lost even after destroy!
    engine.execute_batch(&[WireOp::Destroy { target: EntityTarget::ExternalId("Project-dangling".into()) }]).unwrap();
    let row = engine.get_row("Task-1").unwrap().unwrap();
    assert_eq!(row.fields.get("projectDangling"), Some(&CanonicalValue::String("Project-dangling".into())));
}

#[test]
fn intra_batch_slot_create_plus_link_and_graph_constraints() {
    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let (engine, _) =
        Engine::open(store, vfs, SimClock::default(), Path::new("/sim/batch"), DurabilityMode::Unsafe).unwrap();
    engine.set_schema(&sample_schema()).unwrap();

    // Single binary wire frame: create Project (slot 0), Task A (slot 1), Task B (slot 2),
    // link Task A -> Project (IN_PROJECT), link Task A -> Task B (DEPENDS_ON)!
    let frame = encode_batch(
        CommitReturnFlags::RESULTS,
        &[
            WireOp::Create {
                type_name: "Project".into(),
                external_id: Some("Project-alpha".into()),
                fields: vec![("name".into(), CanonicalValue::String("Alpha".into()))],
            },
            WireOp::Create {
                type_name: "Task".into(),
                external_id: Some("Task-a".into()),
                fields: vec![("title".into(), CanonicalValue::String("A".into()))],
            },
            WireOp::Create {
                type_name: "Task".into(),
                external_id: Some("Task-b".into()),
                fields: vec![("title".into(), CanonicalValue::String("B".into()))],
            },
            WireOp::Link {
                source: EntityTarget::BatchSlot(1),
                rel_kind: "IN_PROJECT".into(),
                target: EntityTarget::BatchSlot(0),
                weight: None,
                info: None,
            },
            WireOp::Link {
                source: EntityTarget::BatchSlot(1),
                rel_kind: "DEPENDS_ON".into(),
                target: EntityTarget::BatchSlot(2),
                weight: Some(2.5f64.to_bits()),
                info: None,
            },
        ],
    );
    let outcome = engine.execute_wire_batch(&frame).unwrap();
    assert_eq!(outcome.created_ids, vec!["Project-alpha", "Task-a", "Task-b"]);
    assert_eq!(engine.relations_out("Task-a", Some("IN_PROJECT")).unwrap().len(), 1);
    assert_eq!(engine.relations_out("Task-a", Some("DEPENDS_ON")).unwrap().len(), 1);

    // Acyclic enforcement: Task-b -> Task-a MUST fail with CycleDetected!
    let err = engine
        .execute_batch(&[WireOp::Link {
            source: EntityTarget::ExternalId("Task-b".into()),
            rel_kind: "DEPENDS_ON".into(),
            target: EntityTarget::ExternalId("Task-a".into()),
            weight: None,
            info: None,
        }])
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::CycleDetected);

    // ManyToOne enforcement: linking Task-a to a second Project MUST fail with CardinalityViolation!
    engine
        .execute_batch(&[WireOp::Create {
            type_name: "Project".into(),
            external_id: Some("Project-beta".into()),
            fields: vec![("name".into(), CanonicalValue::String("Beta".into()))],
        }])
        .unwrap();
    let err = engine
        .execute_batch(&[WireOp::Link {
            source: EntityTarget::ExternalId("Task-a".into()),
            rel_kind: "IN_PROJECT".into(),
            target: EntityTarget::ExternalId("Project-beta".into()),
            weight: None,
            info: None,
        }])
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::CardinalityViolation);
}

#[test]
fn schema_o1_renames_archived_types_and_commit_log() {
    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let (engine, _) =
        Engine::open(store, vfs, SimClock::default(), Path::new("/sim/schema"), DurabilityMode::Unsafe).unwrap();
    engine.set_schema(&sample_schema()).unwrap();

    engine
        .execute_batch(&[WireOp::Create {
            type_name: "Task".into(),
            external_id: Some("Task-orig".into()),
            fields: vec![("title".into(), CanonicalValue::String("Hello".into()))],
        }])
        .unwrap();

    // O(1) renameField: row immediately reads back under new name without rewriting rows!
    engine.rename_field("Task", "title", "headline").unwrap();
    let row = engine.get_row("Task-orig").unwrap().unwrap();
    assert_eq!(row.fields.get("headline"), Some(&CanonicalValue::String("Hello".into())));

    // O(1) renameType with default rewrite_ids = false keeps external ID "Task-orig" immutable!
    engine.rename_type("Task", "Todo", false).unwrap();
    let row = engine.get_row("Task-orig").unwrap().unwrap();
    assert_eq!(row.id, "Task-orig");
    assert_eq!(row.type_name, "Todo");

    // Removing "Todo" from schema archives it: rows stay readable, writes fail with TypeArchived!
    let mut reduced = sample_schema();
    reduced.types.remove("Task");
    reduced.relations.clear();
    engine.set_schema(&reduced).unwrap();
    assert_eq!(engine.list_by_type("Todo", true).unwrap().len(), 1);
    let err = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("Task-orig".into()),
            fields: vec![("headline".into(), CanonicalValue::String("Denied".into()))],
        }])
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::TypeArchived);

    // Cross-process commit_log recorded all summaries cleanly.
    let summaries = engine.read_commit_summaries_since(0).unwrap().unwrap();
    assert!(summaries.len() >= 4);
}

#[test]
fn before_commit_hooks_see_writes_abort_atomically_and_enforce_cycle_limit() {
    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let (engine, _) =
        Engine::open(store, vfs, SimClock::default(), Path::new("/sim/hooks"), DurabilityMode::Unsafe).unwrap();
    let schema = engine.set_schema(&sample_schema()).unwrap().0;
    let task_tid = schema.type_by_name("Task").unwrap().id;
    let audit_tid = schema.type_by_name("Audit").unwrap().id;

    // Hook 1: whenever Task changes, read the Task inside the write txn and create an Audit row.
    let mut wants_task = TypeMask::empty();
    wants_task.insert(task_tid);
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_ref = Arc::clone(&calls);
    let hook1 = engine.register_before_commit(BeforeCommitHook {
        wants: wants_task.clone(),
        callback: Arc::new(move |ctx| {
            calls_ref.fetch_add(1, Ordering::SeqCst);
            let tasks = ctx.list_by_type("Task", false)?;
            if tasks.iter().any(|t| t.fields.get("title") == Some(&CanonicalValue::String("FORBIDDEN".into()))) {
                return Err(ElmdbError::new(ErrorCode::HookAborted, "forbidden title"));
            }
            ctx.exec(&WireOp::Create {
                type_name: "Audit".into(),
                external_id: None,
                fields: vec![("msg".into(), CanonicalValue::String(format!("saw {} tasks", tasks.len())))],
            })?;
            Ok(())
        }),
    });

    // Creating a Task triggers hook1 once and commits both Task and Audit atomically.
    engine
        .execute_batch(&[WireOp::Create {
            type_name: "Task".into(),
            external_id: Some("Task-ok".into()),
            fields: vec![("title".into(), CanonicalValue::String("Good".into()))],
        }])
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(engine.list_by_type("Audit", false).unwrap().len(), 1);

    // Creating a forbidden Task aborts atomically via native savepoint!
    let err = engine
        .execute_batch(&[WireOp::Create {
            type_name: "Task".into(),
            external_id: Some("Task-bad".into()),
            fields: vec![("title".into(), CanonicalValue::String("FORBIDDEN".into()))],
        }])
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::HookAborted);
    assert!(engine.get_row("Task-bad").unwrap().is_none());
    assert_eq!(engine.list_by_type("Audit", false).unwrap().len(), 1);

    // Register a second hook watching Audit that writes back to Task -> cycle MUST hit HookCycleLimit!
    let mut wants_audit = TypeMask::empty();
    wants_audit.insert(audit_tid);
    let _hook2 = engine.register_before_commit(BeforeCommitHook {
        wants: wants_audit,
        callback: Arc::new(move |ctx| {
            ctx.exec(&WireOp::Create {
                type_name: "Task".into(),
                external_id: None,
                fields: vec![("title".into(), CanonicalValue::String("Loop".into()))],
            })?;
            Ok(())
        }),
    });

    let err = engine
        .execute_batch(&[WireOp::Create {
            type_name: "Task".into(),
            external_id: Some("Task-cycle".into()),
            fields: vec![("title".into(), CanonicalValue::String("Start".into()))],
        }])
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::HookCycleLimit);
    assert!(engine.get_row("Task-cycle").unwrap().is_none());

    assert!(engine.unregister_before_commit(hook1));
}

#[test]
fn review_hardening_savepoint_delta_rollback_nested_proto_and_archived_purge() {
    use elmdb_wire::{ResultDecoder, ResultEncoder};

    // 1. Nested `__proto__` inside CanonicalValue::Object must set `has_proto_key == true`.
    let mut enc = ResultEncoder::new();
    let nested = CanonicalValue::object(vec![("__proto__".into(), CanonicalValue::Int(1))]).unwrap();
    enc.push_row("Task-1", "Task", &[("meta", &nested)], |_| unreachable!()).unwrap();
    let frame = enc.finish();
    let decoded = ResultDecoder::decode(&frame).unwrap();
    assert!(decoded.has_proto_key);

    // 2. Savepoint rollback inside `transact` must roll back BOTH LMDB and delta/hook accumulators!
    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let (engine, _) =
        Engine::open(store, vfs, SimClock::default(), Path::new("/sim/sp"), DurabilityMode::Unsafe).unwrap();
    let schema = engine.set_schema(&sample_schema()).unwrap().0;
    let task_tid = schema.type_by_name("Task").unwrap().id;

    let hook_fires = Arc::new(AtomicUsize::new(0));
    let fires_ref = Arc::clone(&hook_fires);
    let mut wants = TypeMask::empty();
    wants.insert(task_tid);
    engine.register_before_commit(BeforeCommitHook {
        wants,
        callback: Arc::new(move |_| {
            fires_ref.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
    });

    engine
        .transact(|tx| {
            // Create a Project, then inside a failing savepoint create a Task -> Task rolls back!
            tx.exec(&WireOp::Create {
                type_name: "Project".into(),
                external_id: Some("Project-keep".into()),
                fields: vec![("name".into(), CanonicalValue::String("Keep".into()))],
            })?;
            let _: Result<(), _> = tx.savepoint(|sp| {
                sp.exec(&WireOp::Create {
                    type_name: "Task".into(),
                    external_id: Some("Task-rolledback".into()),
                    fields: vec![("title".into(), CanonicalValue::String("Gone".into()))],
                })?;
                Err(ElmdbError::new(ErrorCode::HookAborted, "rollback"))
            });
            Ok(())
        })
        .unwrap();

    // Hook watching Task MUST NOT have fired because Task creation was rolled back!
    assert_eq!(hook_fires.load(Ordering::SeqCst), 0);
    assert!(engine.get_row("Project-keep").unwrap().is_some());
    assert!(engine.get_row("Task-rolledback").unwrap().is_none());

    // 3. `get_field` resolves EntityRef to external ID string cleanly.
    engine
        .execute_batch(&[WireOp::Create {
            type_name: "Task".into(),
            external_id: Some("Task-ref".into()),
            fields: vec![
                ("title".into(), CanonicalValue::String("Ref".into())),
                ("projectRestrict".into(), CanonicalValue::String("Project-keep".into())),
            ],
        }])
        .unwrap();
    assert_eq!(
        engine.get_field("Task-ref", "projectRestrict").unwrap(),
        Some(CanonicalValue::String("Project-keep".into()))
    );

    // 4. Duplicate parallel link on ManyToOne fails with RelationAlreadyExists.
    engine
        .execute_batch(&[WireOp::Link {
            source: EntityTarget::ExternalId("Task-ref".into()),
            rel_kind: "IN_PROJECT".into(),
            target: EntityTarget::ExternalId("Project-keep".into()),
            weight: None,
            info: None,
        }])
        .unwrap();
    let dup = engine
        .execute_batch(&[WireOp::Link {
            source: EntityTarget::ExternalId("Task-ref".into()),
            rel_kind: "IN_PROJECT".into(),
            target: EntityTarget::ExternalId("Project-keep".into()),
            weight: None,
            info: None,
        }])
        .unwrap_err();
    assert_eq!(dup.code, ErrorCode::RelationAlreadyExists);

    // 5. Archiving Task still lets us Destroy/purge archived Task-ref AND prevents renaming Audit -> Task!
    let mut reduced = sample_schema();
    reduced.types.remove("Task");
    reduced.relations.clear();
    engine.set_schema(&reduced).unwrap();
    assert_eq!(engine.rename_type("Audit", "Task", false).unwrap_err().code, ErrorCode::InvalidSchema);
    engine.execute_batch(&[WireOp::Destroy { target: EntityTarget::ExternalId("Task-ref".into()) }]).unwrap();
    assert!(engine.get_row("Task-ref").unwrap().is_none());
}

#[test]
fn batch1_spilled_extra_patch_live_kind_change_refusal_and_ephemeral_delta_folding() {
    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let (engine, _) =
        Engine::open(store, vfs, SimClock::default(), Path::new("/sim/batch1"), DurabilityMode::Unsafe).unwrap();
    let mut schema = sample_schema();
    schema.types.get_mut("Task").unwrap().additional_fields = true;
    engine.set_schema(&schema).unwrap();

    // P0-1: Updating an entity whose `extra` is already spilled (>4 KiB) MUST rewrite EXTRA_BLOB_FIELD_ID!
    let big_v1 = "a".repeat(5_000);
    let big_v2 = "b".repeat(5_200);
    engine
        .execute_batch(&[WireOp::Create {
            type_name: "Task".into(),
            external_id: Some("Task-extra".into()),
            fields: vec![
                ("title".into(), CanonicalValue::String("Extra Test".into())),
                ("dyn_big".into(), CanonicalValue::String(big_v1)),
                ("dyn_flag".into(), CanonicalValue::Int(1)),
            ],
        }])
        .unwrap();

    engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("Task-extra".into()),
            fields: vec![
                ("dyn_big".into(), CanonicalValue::String(big_v2.clone())),
                ("dyn_flag".into(), CanonicalValue::Int(2)),
            ],
        }])
        .unwrap();
    let row = engine.get_row("Task-extra").unwrap().expect("row exists");
    assert_eq!(row.fields.get("dyn_big"), Some(&CanonicalValue::String(big_v2.clone())));
    assert_eq!(row.fields.get("dyn_flag"), Some(&CanonicalValue::Int(2)));

    // Unsetting one extra key while remaining spilled (>4 KiB) must also rewrite the extra blob.
    engine
        .execute_batch(&[WireOp::Unset {
            target: EntityTarget::ExternalId("Task-extra".into()),
            fields: vec!["dyn_flag".into()],
        }])
        .unwrap();
    let row2 = engine.get_row("Task-extra").unwrap().expect("row exists");
    assert_eq!(row2.fields.get("dyn_big"), Some(&CanonicalValue::String(big_v2)));
    assert_eq!(row2.fields.get("dyn_flag"), None);

    // P0-2: Incompatible kind change on an ACTIVE field MUST fail with InvalidSchema (not silently archive!).
    let mut bad_schema = schema.clone();
    bad_schema.types.get_mut("Task").unwrap().fields.get_mut("title").unwrap().kind = RawFieldKind::Integer;
    let err = engine.set_schema(&bad_schema).unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidSchema);

    // P0-2: Changing Ref.on_delete on an active Ref field preserves FieldId and loaded data!
    let mut relaxed = schema.clone();
    relaxed.types.get_mut("Task").unwrap().fields.get_mut("projectRestrict").unwrap().kind =
        RawFieldKind::Ref { to: vec!["Project".into()], on_delete: OnDeletePolicy::AllowDangling };
    engine.set_schema(&relaxed).unwrap();
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Project".into(),
                external_id: Some("Proj-1".into()),
                fields: vec![("name".into(), CanonicalValue::String("P1".into()))],
            },
            WireOp::Patch {
                target: EntityTarget::ExternalId("Task-extra".into()),
                fields: vec![("projectRestrict".into(), CanonicalValue::String("Proj-1".into()))],
            },
        ])
        .unwrap();
    // Tighten AllowDangling -> Restrict: field stays intact on same FieldId and Restrict enforces!
    engine.set_schema(&schema).unwrap();
    assert_eq!(
        engine.get_field("Task-extra", "projectRestrict").unwrap(),
        Some(CanonicalValue::String("Proj-1".into()))
    );
    let ref_err =
        engine.execute_batch(&[WireOp::Destroy { target: EntityTarget::ExternalId("Proj-1".into()) }]).unwrap_err();
    assert_eq!(ref_err.code, ErrorCode::Referenced);

    // P1-6: Create-then-destroy and link-then-unlink in the same transaction fold into net no-op
    // (no phantom `(None, None)` delta in beforeCommit hooks and no tombstone!).
    let saw_hook = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let saw_hook_c = std::sync::Arc::clone(&saw_hook);
    let task_tid = engine.schema().type_by_name("Task").unwrap().id;
    let mut wants = elmdb_engine::TypeMask::empty();
    wants.insert(task_tid);
    engine.register_before_commit(BeforeCommitHook {
        wants,
        callback: Arc::new(move |ctx| {
            saw_hook_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            for d in ctx.entity_deltas() {
                assert!(d.before.is_some() || d.after.is_some(), "must never emit (None, None) entity delta");
            }
            for e in ctx.edge_deltas() {
                assert!(e.before.is_some() || e.after.is_some(), "must never emit (None, None) edge delta");
            }
            Ok(())
        }),
    });

    engine
        .transact(|ctx| {
            ctx.exec_batch_and_flush(&[
                WireOp::Create {
                    type_name: "Task".into(),
                    external_id: Some("Task-ephemeral".into()),
                    fields: vec![("title".into(), CanonicalValue::String("Temp".into()))],
                },
                WireOp::Link {
                    source: EntityTarget::BatchSlot(0),
                    rel_kind: "IN_PROJECT".into(),
                    target: EntityTarget::ExternalId("Proj-1".into()),
                    weight: None,
                    info: None,
                },
                WireOp::Unlink {
                    source: EntityTarget::BatchSlot(0),
                    rel_kind: Some("IN_PROJECT".into()),
                    target: Some(EntityTarget::ExternalId("Proj-1".into())),
                },
                WireOp::Destroy { target: EntityTarget::BatchSlot(0) },
            ])?;
            Ok(())
        })
        .unwrap();
    // Also verify mixed batch: ephemeral Task create+destroy + real Audit create MUST NOT spuriously fire Task hook!
    engine
        .transact(|ctx| {
            ctx.exec_batch_and_flush(&[
                WireOp::Create {
                    type_name: "Task".into(),
                    external_id: Some("Task-ephemeral-mixed".into()),
                    fields: vec![("title".into(), CanonicalValue::String("TempMixed".into()))],
                },
                WireOp::Destroy { target: EntityTarget::BatchSlot(0) },
                WireOp::Create {
                    type_name: "Audit".into(),
                    external_id: Some("Audit-real".into()),
                    fields: vec![("msg".into(), CanonicalValue::String("real audit".into()))],
                },
            ])?;
            Ok(())
        })
        .unwrap();
    assert_eq!(saw_hook.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(engine.get_row("Task-ephemeral").unwrap().is_none());
    assert!(engine.get_row("Task-ephemeral-mixed").unwrap().is_none());
    assert!(engine.get_row("Audit-real").unwrap().is_some());
}

#[test]
fn batch_c_declared_over_extra_precedence_and_no_phantom_commit_seq() {
    use elmdb_engine::{read_commit_seq, read_raw_stored_row, read_stored_row};
    use elmdb_storage::KvRead;

    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let (engine, _) =
        Engine::open(store, vfs, SimClock::default(), Path::new("/sim/batch_c"), DurabilityMode::Unsafe).unwrap();
    let mut schema = sample_schema();
    schema.types.get_mut("Task").unwrap().additional_fields = true;
    engine.set_schema(&schema).unwrap();

    // 1. Create entities with dynamic `extra` entries matching future declared / renamed field names.
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Task".into(),
                external_id: Some("e-patch-only".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("T1".into())),
                    ("tag".into(), CanonicalValue::String("zombie_patch".into())),
                    ("keep_extra".into(), CanonicalValue::String("legit_extra".into())),
                ],
            },
            WireOp::Create {
                type_name: "Task".into(),
                external_id: Some("e-unset-only".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("T2".into())),
                    ("tag".into(), CanonicalValue::String("zombie_unset".into())),
                ],
            },
            WireOp::Create {
                type_name: "Task".into(),
                external_id: Some("e-rename".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("RealTitle".into())),
                    ("headline".into(), CanonicalValue::String("zombie_headline".into())),
                ],
            },
        ])
        .unwrap();

    // 2. Evolve schema: add optional declared field `tag` AND rename declared `title` -> `headline`.
    let mut evolved = schema.clone();
    evolved
        .types
        .get_mut("Task")
        .unwrap()
        .fields
        .insert("tag".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: false });
    engine.set_schema(&evolved).unwrap();
    engine.rename_field("Task", "title", "headline").unwrap();

    // C1: Even before patching, `read_stored_row`, `read_raw_stored_row`, and `get_field` MUST enforce
    // `Active Declared > Extra > Archived Declared`.
    {
        let (ro, cur_schema) = engine.begin_read_pinned().unwrap();
        let dbs = engine.dbs();
        let k_ev = elmdb_engine::lookup_ext_id_in_txn(&ro, &dbs, "e-patch-only").unwrap().unwrap();
        let k_rn = elmdb_engine::lookup_ext_id_in_txn(&ro, &dbs, "e-rename").unwrap().unwrap();
        for row in [
            read_stored_row(&ro, &cur_schema, &dbs, k_ev).unwrap().unwrap(),
            read_raw_stored_row(&ro, &cur_schema, &dbs, k_ev).unwrap().unwrap(),
        ] {
            assert_eq!(row.fields.get("tag"), None, "unset declared field must not resurrect stale extra");
            assert_eq!(row.fields.get("headline"), Some(&CanonicalValue::String("T1".into())));
            assert_eq!(row.fields.get("keep_extra"), Some(&CanonicalValue::String("legit_extra".into())));
        }
        for row in [
            read_stored_row(&ro, &cur_schema, &dbs, k_rn).unwrap().unwrap(),
            read_raw_stored_row(&ro, &cur_schema, &dbs, k_rn).unwrap().unwrap(),
        ] {
            assert_eq!(
                row.fields.get("headline"),
                Some(&CanonicalValue::String("RealTitle".into())),
                "renamed declared field must beat stale extra"
            );
        }
    }
    assert_eq!(engine.get_field("e-patch-only", "tag").unwrap(), None);
    assert_eq!(engine.get_field("e-rename", "headline").unwrap(), Some(CanonicalValue::String("RealTitle".into())));

    // C1: Isolated `Patch` scrubbing on `e-patch-only` (NEVER unset) AND isolated `Unset` scrubbing on
    // `e-unset-only` (NEVER patched) must BOTH physically remove stale `extra["tag"]`!
    engine
        .execute_batch(&[
            WireOp::Patch {
                target: EntityTarget::ExternalId("e-patch-only".into()),
                fields: vec![("tag".into(), CanonicalValue::String("declared_v1".into()))],
            },
            WireOp::Unset { target: EntityTarget::ExternalId("e-unset-only".into()), fields: vec!["tag".into()] },
        ])
        .unwrap();
    assert_eq!(
        engine.get_row("e-patch-only").unwrap().unwrap().fields.get("tag"),
        Some(&CanonicalValue::String("declared_v1".into()))
    );
    assert_eq!(engine.get_row("e-unset-only").unwrap().unwrap().fields.get("tag"), None);

    // Archive `tag` (removing it from active schema):
    // - `e-unset-only` has neither declared `tag` nor `extra["tag"]` -> `None` (proves `Unset` scrubbed `extra`!)
    // - `e-patch-only` retains archived declared `tag = "declared_v1"` (proves `Patch` scrubbed `zombie_patch`!)
    // - Patching `e-patch-only` with live `extra["tag"] = "extra_v2"` MUST beat archived declared `"declared_v1"`
    //   across `get_field`, `read_stored_row`, and `read_raw_stored_row` (`Active Declared > Extra > Archived Declared`)!
    engine.set_schema(&schema).unwrap();
    assert_eq!(
        engine.get_row("e-unset-only").unwrap().unwrap().fields.get("tag"),
        None,
        "Unset must scrub extra['tag'] independently of Patch"
    );
    assert_eq!(
        engine.get_row("e-patch-only").unwrap().unwrap().fields.get("tag"),
        Some(&CanonicalValue::String("declared_v1".into())),
        "Patch must scrub zombie_patch so archived declared value is not clobbered"
    );
    engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("e-patch-only".into()),
            fields: vec![("tag".into(), CanonicalValue::String("extra_v2".into()))],
        }])
        .unwrap();
    assert_eq!(engine.get_field("e-patch-only", "tag").unwrap(), Some(CanonicalValue::String("extra_v2".into())));
    {
        let (ro, cur_schema) = engine.begin_read_pinned().unwrap();
        let dbs = engine.dbs();
        let k_p = elmdb_engine::lookup_ext_id_in_txn(&ro, &dbs, "e-patch-only").unwrap().unwrap();
        for row in [
            read_stored_row(&ro, &cur_schema, &dbs, k_p).unwrap().unwrap(),
            read_raw_stored_row(&ro, &cur_schema, &dbs, k_p).unwrap().unwrap(),
        ] {
            assert_eq!(
                row.fields.get("tag"),
                Some(&CanonicalValue::String("extra_v2".into())),
                "live extra patched after archiving must beat stale archived declared field"
            );
        }
    }

    // C4: Empty `execute_batch(&[])` and read-only `Engine::transact` MUST NOT bump `commit_seq` or `txnid`!
    let (seq_before, txnid_before) = {
        let (ro, _) = engine.begin_read_pinned().unwrap();
        (read_commit_seq(&ro, &engine.dbs()).unwrap(), ro.txnid())
    };
    let empty_batch = engine.execute_batch(&[]).unwrap();
    assert_eq!(empty_batch.commit_seq, seq_before);
    assert_eq!(empty_batch.receipt.txnid, txnid_before);
    assert!(empty_batch.summary.is_none());

    let (val, tx_receipt) = engine.transact(|tx| Ok(tx.get_row("e-patch-only")?.is_some())).unwrap();
    assert!(val);
    assert_eq!(tx_receipt.txnid, txnid_before);

    let (seq_after, txnid_after) = {
        let (ro, _) = engine.begin_read_pinned().unwrap();
        (read_commit_seq(&ro, &engine.dbs()).unwrap(), ro.txnid())
    };
    assert_eq!(seq_after, seq_before, "empty batch and read-only transact must not bump commit_seq");
    assert_eq!(txnid_after, txnid_before, "empty batch and read-only transact must not commit an LMDB txn");

    // P2-4: Raw write via `tx.txn_mut()` marks the transaction dirty automatically so it commits cleanly.
    let (_, raw_receipt) = engine
        .transact(|tx| {
            let dbs = tx.dbs();
            tx.txn_mut().put(dbs.sequences, b"raw_test_marker", b"ok")?;
            Ok(())
        })
        .unwrap();
    assert!(raw_receipt.txnid > txnid_after);
    let (ro, _) = engine.begin_read_pinned().unwrap();
    assert_eq!(ro.get(engine.dbs().sequences, b"raw_test_marker").unwrap(), Some(b"ok".as_slice()));
}

#[test]
fn pr268_batch2_atomic_exec_batch_archived_fields_restore_extras_and_abort_savepoint_zero() {
    use elmdb_storage::{KvCursor, KvRead};
    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let clock = SimClock::new(10_000);
    let (engine, _) = Engine::open(store, vfs, clock, "/sim/pr268_batch2", DurabilityMode::Strict).unwrap();

    // Schema v1: Target + Item(title required, legacy_blob optional String, legacy_ref optional Ref(Target)).
    let mut s1 = RawSchema::default();
    let mut target_fields = BTreeMap::new();
    target_fields.insert("label".into(), RawField { kind: RawFieldKind::String, optional: false, nullable: false });
    s1.types.insert(
        "Target".into(),
        RawType {
            fields: target_fields,
            indexes: BTreeMap::new(),
            additional_fields: false,
            short_code: None,
            change_order: false,
            declared_by: None,
        },
    );
    let mut item_fields = BTreeMap::new();
    item_fields.insert("title".into(), RawField { kind: RawFieldKind::String, optional: false, nullable: false });
    item_fields.insert("legacy_blob".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: false });
    item_fields.insert(
        "legacy_ref".into(),
        RawField {
            kind: RawFieldKind::Ref { to: vec!["Target".into()], on_delete: OnDeletePolicy::Restrict },
            optional: true,
            nullable: true,
        },
    );
    s1.types.insert(
        "Item".into(),
        RawType {
            fields: item_fields,
            indexes: BTreeMap::new(),
            additional_fields: false,
            short_code: None,
            change_order: false,
            declared_by: None,
        },
    );
    engine.set_schema(&s1).unwrap();

    // Item 2.2: `TxnCtx::exec_batch` is atomic AND `WireOp::Create` validates before writing `ext_id`.
    engine
        .transact(|tx| {
            // 2.6: `abort_savepoint(0)` returns immediately at depth 0 and does not abort active depth 1!
            tx.abort_savepoint(0);
            let d1 = tx.push_savepoint()?;
            assert_eq!(d1, 1);
            tx.abort_savepoint(0);
            tx.exec(&WireOp::Create {
                type_name: "Target".into(),
                external_id: Some("t-keep".into()),
                fields: vec![("label".into(), CanonicalValue::String("kept".into()))],
            })?;
            tx.commit_savepoint(d1)?;
            assert!(tx.get_row("t-keep")?.is_some());

            // `exec_batch` with op 1 valid and op 2 missing required `title` rolls back BOTH ops cleanly!
            let err = tx
                .exec_batch(&[
                    WireOp::Create {
                        type_name: "Item".into(),
                        external_id: Some("ok-1".into()),
                        fields: vec![("title".into(), CanonicalValue::String("ok".into()))],
                    },
                    WireOp::Create { type_name: "Item".into(), external_id: Some("bad-2".into()), fields: vec![] },
                ])
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::ValidationFailed);
            assert!(tx.get_row("ok-1")?.is_none());
            assert!(tx.get_row("bad-2")?.is_none());
            let dbs = tx.dbs();
            assert!(
                elmdb_engine::lookup_ext_id_in_txn(tx.txn_mut(), &dbs, "ok-1")?.is_none(),
                "failed exec_batch must not leave op 1 ext_id"
            );
            assert!(
                elmdb_engine::lookup_ext_id_in_txn(tx.txn_mut(), &dbs, "bad-2")?.is_none(),
                "failed Create must not leave orphan ext_id"
            );

            // Direct `tx.exec(Create)` failing validation also leaves zero orphan `ext_id`!
            let single_err = tx
                .exec(&WireOp::Create {
                    type_name: "Item".into(),
                    external_id: Some("bad-single".into()),
                    fields: vec![],
                })
                .unwrap_err();
            assert_eq!(single_err.code, ErrorCode::ValidationFailed);
            assert!(elmdb_engine::lookup_ext_id_in_txn(tx.txn_mut(), &dbs, "bad-single")?.is_none());

            // Re-creating `"ok-1"`, `"bad-2"`, and `"bad-single"` cleanly in the SAME txn succeeds!
            tx.exec_batch(&[
                WireOp::Create {
                    type_name: "Item".into(),
                    external_id: Some("ok-1".into()),
                    fields: vec![("title".into(), CanonicalValue::String("one".into()))],
                },
                WireOp::Create {
                    type_name: "Item".into(),
                    external_id: Some("bad-2".into()),
                    fields: vec![("title".into(), CanonicalValue::String("two".into()))],
                },
                WireOp::Create {
                    type_name: "Item".into(),
                    external_id: Some("bad-single".into()),
                    fields: vec![("title".into(), CanonicalValue::String("single".into()))],
                },
            ])?;

            // Explicit `external_id` length boundaries (`0` rejected before field validation, `MAX_EXT_ID_BYTES` accepted, `+1` rejected before field validation).
            let empty_id_err = tx
                .exec(&WireOp::Create { type_name: "Item".into(), external_id: Some(String::new()), fields: vec![] })
                .unwrap_err();
            assert_eq!(empty_id_err.code, ErrorCode::InvalidArgument);
            let over_id_err = tx
                .exec(&WireOp::Create {
                    type_name: "Item".into(),
                    external_id: Some("x".repeat(elmdb_engine::MAX_EXT_ID_BYTES + 1)),
                    fields: vec![],
                })
                .unwrap_err();
            assert_eq!(over_id_err.code, ErrorCode::InvalidArgument);
            let exact_max_id = "m".repeat(elmdb_engine::MAX_EXT_ID_BYTES);
            tx.exec(&WireOp::Create {
                type_name: "Item".into(),
                external_id: Some(exact_max_id.clone()),
                fields: vec![("title".into(), CanonicalValue::String("max-id".into()))],
            })?;
            assert!(tx.get_row(&exact_max_id)?.is_some());

            // Explicit `external_id` collision still reports `EntityAlreadyExists` even with invalid fields.
            let dup_err = tx
                .exec(&WireOp::Create { type_name: "Item".into(), external_id: Some("ok-1".into()), fields: vec![] })
                .unwrap_err();
            assert_eq!(dup_err.code, ErrorCode::EntityAlreadyExists);
            Ok(())
        })
        .unwrap();

    // Auto-generated `Item-{next_key}` collision against a pre-existing explicit `ext_id` fails closed.
    let next_auto_key = {
        let (ro, _) = engine.begin_read_pinned().unwrap();
        let dbs = engine.dbs();
        let raw = ro.get(dbs.sequences, b"\x01entity").unwrap().unwrap();
        u64::from_le_bytes(raw.try_into().unwrap()) + 1
    };
    let colliding_ext = format!("Item-{}", next_auto_key + 1);
    engine
        .execute_batch(&[WireOp::Create {
            type_name: "Item".into(),
            external_id: Some(colliding_ext),
            fields: vec![("title".into(), CanonicalValue::String("pre-occupy".into()))],
        }])
        .unwrap();
    let auto_err = engine
        .execute_batch(&[WireOp::Create {
            type_name: "Item".into(),
            external_id: None,
            fields: vec![("title".into(), CanonicalValue::String("auto-collide".into()))],
        }])
        .unwrap_err();
    assert_eq!(auto_err.code, ErrorCode::EntityAlreadyExists);

    // Item 2.3: Unified 3-tier archived field semantics (`read_single_field`, `Unset`, `Patch`+`Unset`, blob + ref_in).
    let big_legacy = "L".repeat(6 * 1024);
    engine
        .execute_batch(&[
            WireOp::Create {
                type_name: "Item".into(),
                external_id: Some("arch-strict".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("strict".into())),
                    ("legacy_blob".into(), CanonicalValue::String(big_legacy.clone())),
                    ("legacy_ref".into(), CanonicalValue::String("t-keep".into())),
                ],
            },
            WireOp::Create {
                type_name: "Item".into(),
                external_id: Some("arch-extra".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("extra".into())),
                    ("legacy_blob".into(), CanonicalValue::String("original-archived".into())),
                ],
            },
        ])
        .unwrap();

    // Archive `legacy_blob` and `legacy_ref` while `additional_fields == false`.
    let mut s2 = s1.clone();
    s2.types.get_mut("Item").unwrap().fields.remove("legacy_blob");
    s2.types.get_mut("Item").unwrap().fields.remove("legacy_ref");
    engine.set_schema(&s2).unwrap();

    // `get_field` (`read_single_field`) MUST agree with `get_row` on archived declared fields (including Ref & Blob).
    assert_eq!(engine.get_field("arch-strict", "legacy_blob").unwrap(), Some(CanonicalValue::String(big_legacy)));
    assert_eq!(engine.get_field("arch-strict", "legacy_ref").unwrap(), Some(CanonicalValue::String("t-keep".into())));
    assert_eq!(engine.get_field("arch-strict", "nonexistent").unwrap(), None);

    // Unset on truly unknown field when `additional_fields == false` still rejects with `UnknownField`.
    let unk_err = engine
        .execute_batch(&[WireOp::Unset {
            target: EntityTarget::ExternalId("arch-strict".into()),
            fields: vec!["never_existed".into()],
        }])
        .unwrap_err();
    assert_eq!(unk_err.code, ErrorCode::UnknownField);

    // Unset on archived declared fields MUST succeed even when `additional_fields == false`,
    // cleaning up spilled blobs and `ref_in` and recording the archived FieldIds in `changed_fields`!
    let archived_legacy_id =
        engine.schema().type_by_name("Item").unwrap().fields.iter().find(|f| f.name == "legacy_blob").unwrap().id;
    let strict_unset_sum = engine
        .execute_batch(&[WireOp::Unset {
            target: EntityTarget::ExternalId("arch-strict".into()),
            fields: vec!["legacy_blob".into(), "legacy_ref".into()],
        }])
        .unwrap()
        .summary
        .unwrap();
    assert!(strict_unset_sum.entity_deltas[0].changed_fields.contains(archived_legacy_id));
    assert_eq!(engine.get_field("arch-strict", "legacy_blob").unwrap(), None);
    assert_eq!(engine.get_field("arch-strict", "legacy_ref").unwrap(), None);
    let strict_row = engine.get_row("arch-strict").unwrap().unwrap();
    assert!(!strict_row.fields.contains_key("legacy_blob"));
    assert!(!strict_row.fields.contains_key("legacy_ref"));
    {
        let (ro, _) = engine.begin_read_pinned().unwrap();
        let dbs = engine.dbs();
        let mut cur = ro.cursor(dbs.blobs).unwrap();
        assert!(cur.first().unwrap().is_none(), "unset of spilled archived blob must remove blob entry");
    }

    // Enable `additional_fields == true`: verify Tier 2 vs Tier 3 fallback AND `Patch` + `Unset` never resurrects!
    let mut s3 = s2.clone();
    s3.types.get_mut("Item").unwrap().additional_fields = true;
    engine.set_schema(&s3).unwrap();

    // Add unrelated extra field on `arch-extra` so `hdr.has_extra() == true`, then read archived `legacy_blob`!
    engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("arch-extra".into()),
            fields: vec![("other_extra".into(), CanonicalValue::Int(7))],
        }])
        .unwrap();
    assert_eq!(
        engine.get_field("arch-extra", "legacy_blob").unwrap(),
        Some(CanonicalValue::String("original-archived".into())),
        "Tier 3 archived fallback must still apply when row has unrelated extra fields"
    );

    // Patch `legacy_blob` (writes to extra AND scrubs archived FieldId), then Unset -> MUST NOT resurrect!
    // Also verify `changed_fields` records both the scrubbed archived FieldId and `EXTRA_BLOB_FIELD_ID`!
    let archived_legacy_id =
        engine.schema().type_by_name("Item").unwrap().fields.iter().find(|f| f.name == "legacy_blob").unwrap().id;
    let patch_sum = engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("arch-extra".into()),
            fields: vec![("legacy_blob".into(), CanonicalValue::String("patched-extra".into()))],
        }])
        .unwrap()
        .summary
        .unwrap();
    assert!(patch_sum.entity_deltas[0].changed_fields.contains(archived_legacy_id));
    assert!(patch_sum.entity_deltas[0].changed_fields.contains(elmdb_engine::row::EXTRA_BLOB_FIELD_ID));
    assert_eq!(
        engine.get_field("arch-extra", "legacy_blob").unwrap(),
        Some(CanonicalValue::String("patched-extra".into()))
    );
    let unset_sum = engine
        .execute_batch(&[WireOp::Unset {
            target: EntityTarget::ExternalId("arch-extra".into()),
            fields: vec!["legacy_blob".into()],
        }])
        .unwrap()
        .summary
        .unwrap();
    assert!(unset_sum.entity_deltas[0].changed_fields.contains(elmdb_engine::row::EXTRA_BLOB_FIELD_ID));
    assert_eq!(engine.get_field("arch-extra", "legacy_blob").unwrap(), None);
    assert!(!engine.get_row("arch-extra").unwrap().unwrap().fields.contains_key("legacy_blob"));

    // Exercise `restore_loaded_extra_fields` empty no-op (no maint_seq bump), validation guards,
    // large spilled extra, and orphan blob cleanup when shrinking spilled extra inline (P2-1)!
    let maint_before = {
        let (ro, _) = engine.begin_read_pinned().unwrap();
        elmdb_engine::read_maint_seq(&ro, &engine.dbs()).unwrap()
    };
    engine.restore_loaded_extra_fields(&[]).unwrap();
    let maint_after = {
        let (ro, _) = engine.begin_read_pinned().unwrap();
        elmdb_engine::read_maint_seq(&ro, &engine.dbs()).unwrap()
    };
    assert_eq!(maint_after, maint_before, "empty restore_loaded_extra_fields must not bump maint_seq");

    let mut bad_proto = BTreeMap::new();
    bad_proto.insert("__proto__".into(), CanonicalValue::Int(1));
    assert_eq!(
        engine.restore_loaded_extra_fields(&[("arch-extra".into(), bad_proto)]).unwrap_err().code,
        ErrorCode::ValidationFailed
    );
    let mut bad_nan = BTreeMap::new();
    bad_nan.insert("bad_f".into(), CanonicalValue::Float(f64::NAN));
    assert_eq!(
        engine.restore_loaded_extra_fields(&[("arch-extra".into(), bad_nan)]).unwrap_err().code,
        ErrorCode::ValidationFailed
    );
    let mut bad_ref = BTreeMap::new();
    bad_ref.insert("dangling".into(), CanonicalValue::DanglingRef("x".into()));
    assert_eq!(
        engine.restore_loaded_extra_fields(&[("arch-extra".into(), bad_ref)]).unwrap_err().code,
        ErrorCode::ValidationFailed
    );
    let big_extra = "E".repeat(6 * 1024);
    let mut good_extra = BTreeMap::new();
    good_extra.insert("big_extra".into(), CanonicalValue::String(big_extra.clone()));
    engine.restore_loaded_extra_fields(&[("arch-extra".into(), good_extra)]).unwrap();
    assert_eq!(engine.get_field("arch-extra", "big_extra").unwrap(), Some(CanonicalValue::String(big_extra.clone())));
    {
        let (ro, cur_schema) = engine.begin_read_pinned().unwrap();
        let dbs = engine.dbs();
        let k = elmdb_engine::lookup_ext_id_in_txn(&ro, &dbs, "arch-extra").unwrap().unwrap();
        let (dump_row, dump_extra) = elmdb_engine::read_raw_dump_row(&ro, &cur_schema, &dbs, k).unwrap().unwrap();
        assert_eq!(dump_row.fields.get("title"), Some(&CanonicalValue::String("extra".into())));
        assert!(!dump_row.fields.contains_key("big_extra"));
        assert_eq!(dump_extra.get("big_extra"), Some(&CanonicalValue::String(big_extra)));
    }
    // Overwriting spilled 6 KiB extra with a small 4-byte extra must delete the orphaned EXTRA_BLOB_FIELD_ID blob!
    let mut small_extra = BTreeMap::new();
    small_extra.insert("big_extra".into(), CanonicalValue::String("tiny".into()));
    engine.restore_loaded_extra_fields(&[("arch-extra".into(), small_extra)]).unwrap();
    assert_eq!(engine.get_field("arch-extra", "big_extra").unwrap(), Some(CanonicalValue::String("tiny".into())));
    {
        let (ro, _) = engine.begin_read_pinned().unwrap();
        let dbs = engine.dbs();
        let mut cur = ro.cursor(dbs.blobs).unwrap();
        assert!(cur.first().unwrap().is_none(), "shrinking spilled extra inline must delete orphaned blob");
    }
}

#[test]
fn phase0a_rust_hook_no_rerun_on_map_full_savepoint_panic_guard_and_archived_tiebreak() {
    use elmdb_engine::{BeforeCommitHook, CompiledSchema, FieldDef, FieldId, FieldKind, TypeDef, TypeId, TypeMask};
    use elmdb_storage::{DurabilityMode, KvStore, RecoveryStatus, Vfs};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // 1. `Engine::execute_batch` with a Rust `beforeCommit` hook hitting `MapFull` MUST NOT auto-rerun
    //    the hook: first call fails with StorageFull (runs == 1), map grows 2x, explicit retry succeeds (runs == 2)!
    let vfs = SimVfs::new(BootId([0x61; 16]));
    let dir = Path::new("/phase0a_hook_db");
    let store = SimKvStore::new(SimKvConfig { map_size: 256 * 1024, ..SimKvConfig::default() });
    let (engine, rep) =
        Engine::open(store.clone(), vfs.clone(), SimClock::new(1000), dir, DurabilityMode::Grouped).unwrap();
    assert_eq!(rep.status, RecoveryStatus::Fresh);
    assert!(vfs.exists(&dir.join("synced.v2")), "fresh Grouped Engine must seal synced.v2 at bootstrap");

    // Fresh Strict Engine must NOT call a redundant second `gate.sync()` at bootstrap (kills `&& -> ||`).
    let vfs_strict = SimVfs::new(BootId([0x69; 16]));
    let store_strict = SimKvStore::new(SimKvConfig::default());
    let (_strict_eng, strict_rep) = Engine::open(
        store_strict.clone(),
        vfs_strict,
        SimClock::new(1000),
        Path::new("/phase0a_strict_bootstrap"),
        DurabilityMode::Strict,
    )
    .unwrap();
    assert_eq!(strict_rep.status, RecoveryStatus::Fresh);
    assert_eq!(store_strict.sync_count(), 1, "Strict fresh bootstrap must sync once, not twice");

    let mut schema = RawSchema::default();
    let mut blob_fields = BTreeMap::new();
    blob_fields.insert("body".into(), RawField { kind: RawFieldKind::Bytes, optional: false, nullable: false });
    schema.types.insert("Blob".into(), RawType { fields: blob_fields, ..RawType::default() });
    engine.set_schema(&schema).unwrap();

    // Unsynced Grouped write + same-boot reopen -> `SameBootUnsyncedTail` (NOT `UnsyncedAfterReboot`).
    engine
        .execute_batch(&[WireOp::Create {
            type_name: "Blob".into(),
            external_id: Some("seed-1".into()),
            fields: vec![("body".into(), CanonicalValue::Bytes(vec![1, 2, 3]))],
        }])
        .unwrap();
    drop(engine);

    let (engine2, rep2) =
        Engine::open(store.clone(), vfs.clone(), SimClock::new(2000), dir, DurabilityMode::Grouped).unwrap();
    assert_eq!(rep2.status, RecoveryStatus::SameBootUnsyncedTail);

    // Real reboot with unsynced write still reports `UnsyncedAfterReboot`!
    engine2
        .execute_batch(&[WireOp::Create {
            type_name: "Blob".into(),
            external_id: Some("seed-2".into()),
            fields: vec![("body".into(), CanonicalValue::Bytes(vec![4, 5, 6]))],
        }])
        .unwrap();
    drop(engine2);
    vfs.crash_power_loss(&mut SimRng::seeded(7), true);
    let (engine3, rep3) =
        Engine::open(store.clone(), vfs.clone(), SimClock::new(3000), dir, DurabilityMode::Grouped).unwrap();
    assert_eq!(rep3.status, RecoveryStatus::UnsyncedAfterReboot);
    engine3.acknowledge_restore().unwrap();

    // Rust `beforeCommit` hook hitting real `MapFull` on `Engine::execute_batch`:
    let before_map = store.map_size();
    let mut big_bytes = Vec::with_capacity(before_map + 32 * 1024);
    let mut x = 0x1234_5678_9abc_def0_u64;
    for _ in 0..(before_map + 32 * 1024) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        big_bytes.push((x >> 24) as u8);
    }
    let hook_runs = Arc::new(AtomicUsize::new(0));
    let hook_runs_c = Arc::clone(&hook_runs);
    let big_for_hook = big_bytes.clone();
    let hid = engine3.register_before_commit(BeforeCommitHook {
        wants: TypeMask::all(),
        callback: Arc::new(move |ctx| {
            if ctx
                .entity_deltas()
                .iter()
                .any(|d| d.after.as_ref().is_some_and(|r| r.header.ext_id == "rust-hook-trigger"))
            {
                hook_runs_c.fetch_add(1, Ordering::SeqCst);
                ctx.exec_batch_and_flush(&[WireOp::Create {
                    type_name: "Blob".into(),
                    external_id: Some("rust-hook-big".into()),
                    fields: vec![("body".into(), CanonicalValue::Bytes(big_for_hook.clone()))],
                }])?;
            }
            Ok(())
        }),
    });
    let trigger = vec![WireOp::Create {
        type_name: "Blob".into(),
        external_id: Some("rust-hook-trigger".into()),
        fields: vec![("body".into(), CanonicalValue::Bytes(vec![9]))],
    }];
    let err = engine3.execute_batch(&trigger).unwrap_err();
    assert!(err.is_map_full(), "expected MapFull, got {err:?}");
    assert_eq!(hook_runs.load(Ordering::SeqCst), 1, "Rust beforeCommit hook must run once on MapFull");
    assert!(store.map_size() > before_map, "map must have grown before returning StorageFull");
    assert!(engine3.get_row("rust-hook-big").unwrap().is_none());

    // Explicit caller retry succeeds with hook_runs == 2 (or 2 + extra doublings).
    let mut retries = 0usize;
    loop {
        match engine3.execute_batch(&trigger) {
            Ok(_) => break,
            Err(e) if e.is_map_full() && retries < 4 => retries += 1,
            Err(e) => panic!("unexpected retry error: {e:?}"),
        }
    }
    assert_eq!(hook_runs.load(Ordering::SeqCst), 2 + retries);
    assert!(engine3.get_row("rust-hook-big").unwrap().is_some());
    assert!(engine3.unregister_before_commit(hid));

    // Non-MapFull error on `Engine::execute_batch` MUST NOT grow `map_size` (kills `err.is_map_full() -> true`).
    let map_before_non_full = store.map_size();
    let bad_err = engine3
        .execute_batch(&[WireOp::Create { type_name: "NoSuchType".into(), external_id: None, fields: vec![] }])
        .unwrap_err();
    assert_eq!(bad_err.code, ErrorCode::UnknownType);
    assert_eq!(store.map_size(), map_before_non_full, "non-MapFull error must not grow map");

    // Plain `Engine::execute_batch` (zero hooks invoked) auto-grows on `MapFull` and succeeds cleanly!
    let map_before_plain = store.map_size();
    let mut plain_big = Vec::with_capacity(map_before_plain + 32 * 1024);
    for _ in 0..(map_before_plain + 32 * 1024) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        plain_big.push((x >> 24) as u8);
    }
    engine3
        .execute_batch(&[WireOp::Create {
            type_name: "Blob".into(),
            external_id: Some("rust-plain-autogrow".into()),
            fields: vec![("body".into(), CanonicalValue::Bytes(plain_big))],
        }])
        .unwrap();
    assert!(store.map_size() > map_before_plain, "plain batch must auto-grow map on MapFull");
    assert!(engine3.get_row("rust-plain-autogrow").unwrap().is_some());

    // Direct `TxnCtx::hooks_invoked()` transitions from `false` -> `true` ONLY when a hook runs.
    let eng_arc = Arc::new(engine3);
    let h_flag =
        eng_arc.register_before_commit(BeforeCommitHook { wants: TypeMask::all(), callback: Arc::new(|_ctx| Ok(())) });
    let mut owned = eng_arc.begin_owned_txn().unwrap();
    assert!(!owned.hooks_invoked(), "fresh TxnCtx must report hooks_invoked == false");
    owned
        .exec_batch_and_flush(&[WireOp::Create {
            type_name: "Blob".into(),
            external_id: Some("outer-ok".into()),
            fields: vec![("body".into(), CanonicalValue::Bytes(vec![1]))],
        }])
        .unwrap();
    owned.run_rust_hooks_for_round(&TypeMask::all()).unwrap();
    assert!(owned.hooks_invoked(), "TxnCtx must report hooks_invoked == true after hook ran");
    assert!(eng_arc.unregister_before_commit(h_flag));

    // 2. `TxnCtx::savepoint` panic-safety RAII guard rolls back panicked savepoint & leaves outer txn intact!
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = owned.savepoint(|ctx| -> elmdb_wire::Result<()> {
            ctx.exec_batch_and_flush(&[WireOp::Create {
                type_name: "Blob".into(),
                external_id: Some("inner-panicked".into()),
                fields: vec![("body".into(), CanonicalValue::Bytes(vec![2]))],
            }])?;
            panic!("simulated panic inside savepoint");
        });
    }));
    assert!(caught.is_err());
    assert_eq!(owned.txn().savepoint_depth(), 0, "panicked savepoint must be aborted on unwind");
    assert!(owned.get_row("outer-ok").unwrap().is_some());
    assert!(owned.get_row("inner-panicked").unwrap().is_none());
    owned.finish_and_commit(false).unwrap();

    // 3. Duplicate archived field tie-break: `read_single_field` and `merge_row_fields` MUST agree
    //    (highest FieldId wins if two archived fields ever share a name in a CompiledSchema).
    let vfs2 = SimVfs::new(BootId([0x62; 16]));
    let dir2 = Path::new("/phase0a_arch_tie");
    let store2 = SimKvStore::new(SimKvConfig::default());
    let (eng_tie, _) = Engine::open(store2, vfs2, SimClock::new(1000), dir2, DurabilityMode::Strict).unwrap();
    let mut s_dup = RawSchema::default();
    let mut item_fields = BTreeMap::new();
    item_fields.insert("a".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: false });
    item_fields.insert("b".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: false });
    s_dup.types.insert("Item".into(), RawType { fields: item_fields, ..RawType::default() });
    eng_tie.set_schema(&s_dup).unwrap();
    eng_tie
        .execute_batch(&[WireOp::Create {
            type_name: "Item".into(),
            external_id: Some("dup-1".into()),
            fields: vec![
                ("a".into(), CanonicalValue::String("older-fid".into())),
                ("b".into(), CanonicalValue::String("newer-fid".into())),
            ],
        }])
        .unwrap();
    let (ro, base_sc) = eng_tie.begin_read_pinned().unwrap();
    let t0 = base_sc.type_by_name("Item").unwrap();
    let fa = t0.field_by_name("a").unwrap().id;
    let fb = t0.field_by_name("b").unwrap().id;
    assert!(fa.0 < fb.0);
    let synth = CompiledSchema {
        epoch: base_sc.epoch + 1,
        types: vec![TypeDef {
            id: TypeId(t0.id.0),
            name: t0.name.clone(),
            short_code: t0.short_code.clone(),
            additional_fields: false,
            change_order: false,
            declared_by: None,
            archived: false,
            fields: vec![
                FieldDef {
                    id: FieldId(fa.0),
                    name: "shared".into(),
                    kind: FieldKind::String,
                    optional: true,
                    nullable: false,
                    archived: true,
                },
                FieldDef {
                    id: FieldId(fb.0),
                    name: "shared".into(),
                    kind: FieldKind::String,
                    optional: true,
                    nullable: false,
                    archived: true,
                },
            ],
            next_field_id: t0.next_field_id,
        }],
        relations: vec![],
        indexes: vec![],
        next_type_id: base_sc.next_type_id,
        next_rel_kind_id: base_sc.next_rel_kind_id,
        next_index_id: base_sc.next_index_id,
    };
    let dbs = eng_tie.dbs();
    let ek = elmdb_engine::lookup_ext_id_in_txn(&ro, &dbs, "dup-1").unwrap().unwrap();
    let row = elmdb_engine::read_stored_row(&ro, &synth, &dbs, ek).unwrap().unwrap();
    let single = elmdb_engine::read_single_field(&ro, &synth, &dbs, "dup-1", "shared").unwrap();
    assert_eq!(row.fields.get("shared"), Some(&CanonicalValue::String("newer-fid".into())));
    assert_eq!(single, row.fields.get("shared").cloned());
}
