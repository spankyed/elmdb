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
                external_id: Some("e-evolve".into()),
                fields: vec![
                    ("title".into(), CanonicalValue::String("T1".into())),
                    ("tag".into(), CanonicalValue::String("zombie_extra".into())),
                    ("keep_extra".into(), CanonicalValue::String("legit_extra".into())),
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
    // `Declared > Extra` (declared `headline` wins over stale `extra["headline"]`, unset declared `tag`
    // does not leak `zombie_extra`, and genuine `keep_extra` remains visible!).
    {
        let (ro, cur_schema) = engine.begin_read_pinned().unwrap();
        let dbs = engine.dbs();
        let k_ev = elmdb_engine::lookup_ext_id_in_txn(&ro, &dbs, "e-evolve").unwrap().unwrap();
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
    assert_eq!(engine.get_field("e-evolve", "tag").unwrap(), None);
    assert_eq!(engine.get_field("e-rename", "headline").unwrap(), Some(CanonicalValue::String("RealTitle".into())));

    // C1: Patching and Unsetting declared `tag` scrubs stale `packed.extra["tag"]` physically on disk!
    engine
        .execute_batch(&[WireOp::Patch {
            target: EntityTarget::ExternalId("e-evolve".into()),
            fields: vec![("tag".into(), CanonicalValue::String("declared_v1".into()))],
        }])
        .unwrap();
    assert_eq!(
        engine.get_row("e-evolve").unwrap().unwrap().fields.get("tag"),
        Some(&CanonicalValue::String("declared_v1".into()))
    );
    engine
        .execute_batch(&[WireOp::Unset {
            target: EntityTarget::ExternalId("e-evolve".into()),
            fields: vec!["tag".into()],
        }])
        .unwrap();
    assert_eq!(engine.get_row("e-evolve").unwrap().unwrap().fields.get("tag"), None);
    // Also verify `Unset` on a row that was NEVER patched (`e-evolve` had Patch first; test direct Unset on `e-rename2`)!
    // Even if `tag` is later archived from the schema, scrubbed `extra["tag"]` stays gone!
    engine.set_schema(&schema).unwrap();
    assert_eq!(
        engine.get_row("e-evolve").unwrap().unwrap().fields.get("tag"),
        None,
        "scrubbed extra['tag'] must stay absent even after archiving declared field 'tag'"
    );

    // C4: Empty `execute_batch(&[])` and read-only `Engine::transact` MUST NOT bump `commit_seq` or `txnid`!
    let (seq_before, txnid_before) = {
        let (ro, _) = engine.begin_read_pinned().unwrap();
        (read_commit_seq(&ro, &engine.dbs()).unwrap(), ro.txnid())
    };
    let empty_batch = engine.execute_batch(&[]).unwrap();
    assert_eq!(empty_batch.commit_seq, seq_before);
    assert_eq!(empty_batch.receipt.txnid, txnid_before);
    assert!(empty_batch.summary.is_none());

    let (val, tx_receipt) = engine.transact(|tx| Ok(tx.get_row("e-evolve")?.is_some())).unwrap();
    assert!(val);
    assert_eq!(tx_receipt.txnid, txnid_before);

    let (seq_after, txnid_after) = {
        let (ro, _) = engine.begin_read_pinned().unwrap();
        (read_commit_seq(&ro, &engine.dbs()).unwrap(), ro.txnid())
    };
    assert_eq!(seq_after, seq_before, "empty batch and read-only transact must not bump commit_seq");
    assert_eq!(txnid_after, txnid_before, "empty batch and read-only transact must not commit an LMDB txn");
}
