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
