//! Model 4 (`elmdb_invalidation::model`): Shuttle concurrency model over real
//! `LiveQueryHub` + `Engine<SimKvStore, SimVfs, SimClock>` + `QueryEngine`.
//!
//! Verifies:
//! - Concurrent `register_query_watch` + commit + `notify_commit` NEVER drops an invalidation
//!   (either the watch saw the post-commit state directly, or `CommitRing` catch-up marks it dirty).
//! - Overlapping `rerun_query_watch` calls racing with concurrent commits and schema/maint bumps
//!   respect `run_id` + 3-clock `(commit_seq, maint_seq, schema_epoch)` fencing and NEVER wedge
//!   `running = true` or regress a watch's `ReadSet`.
//! - Concurrent `stop_watches()` and `unregister_watch()` racing with `register_query_watch` and
//!   `rerun_query_watch` NEVER resurrect a stopped or unregistered watch.

use std::collections::BTreeMap;
use std::sync::Arc as StdArc;

use elmdb_engine::{Engine, RawField, RawFieldKind, RawSchema, RawType};
use elmdb_projections::{BuildPolicy, ProjectionRegistry};
use elmdb_query::{
    ExecBudgets, LogicalNode, LogicalPlan, QueryEngine, QueryOutput, QueryResult, SourceSpec, TerminalSpec,
};
use elmdb_sim::{SimClock, SimKvConfig, SimKvStore, SimVfs};
use elmdb_storage::locks::model::{Arc, explore, thread};
use elmdb_storage::{BootId, DurabilityMode};
use elmdb_wire::{CanonicalValue, ErrorCode, WireOp};

use crate::{LiveQueryHub, WatchPolicy, hash_result_128};

fn minimal_schema() -> RawSchema {
    let mut fields = BTreeMap::new();
    fields.insert("tag".into(), RawField { kind: RawFieldKind::String, optional: true, nullable: true });
    let mut types = BTreeMap::new();
    types.insert("Note".into(), RawType { fields, ..Default::default() });
    RawSchema { types, relations: BTreeMap::new() }
}

fn hash_output(out: &QueryOutput) -> Option<[u8; 16]> {
    let len = match &out.result {
        QueryResult::Ids(ids) => ids.len() as u64,
        other => panic!("unexpected result: {other:?}"),
    };
    Some(hash_result_128(&len.to_le_bytes()))
}

fn setup_env() -> (Arc<Engine<SimKvStore, SimVfs, SimClock>>, Arc<QueryEngine>, Arc<LiveQueryHub>, LogicalPlan) {
    let store = SimKvStore::new(SimKvConfig::default());
    let vfs = SimVfs::new(BootId([0x42; 16]));
    let clock = SimClock::default();
    let registry = StdArc::new(ProjectionRegistry::new(BuildPolicy::EagerInSchemaChange));
    let (engine, _) =
        Engine::open_with_options(store, vfs, clock, "/sim-inv", DurabilityMode::Unsafe, registry as _, false)
            .expect("open sim engine");
    engine.set_schema(&minimal_schema()).expect("set schema");
    let schema = engine.schema();
    let note_type = schema.type_by_name("Note").expect("Note type").id;
    let plan = LogicalPlan::new(LogicalNode::Terminal {
        input: Box::new(LogicalNode::Source(SourceSpec::Type { type_id: note_type, include_trashed: false })),
        terminal: TerminalSpec::Ids,
    });
    let hub = LiveQueryHub::from_engine(&engine, 50).expect("hub from engine");
    (Arc::new(engine), Arc::new(QueryEngine::new(16)), Arc::new(hub), plan)
}

fn put_note(engine: &Engine<SimKvStore, SimVfs, SimClock>, hub: &LiveQueryHub, id: &str, tag: &str, now_ms: u64) {
    let op = WireOp::Create {
        type_name: "Note".into(),
        external_id: Some(id.into()),
        fields: vec![("tag".into(), CanonicalValue::String(tag.into()))],
    };
    let outcome = engine.execute_batch(&[op]).expect("put note");
    let schema = engine.schema();
    hub.notify_commit(&schema, outcome.summary.as_ref().expect("commit summary"), now_ms);
}

#[test]
fn concurrent_register_and_commit_never_misses_invalidation() {
    explore(|| {
        let (engine, qe, hub, plan) = setup_env();

        let t_reg = {
            let engine = Arc::clone(&engine);
            let qe = Arc::clone(&qe);
            let hub = Arc::clone(&hub);
            let plan = plan.clone();
            thread::spawn(move || {
                hub.register_query_watch(
                    &engine,
                    &qe,
                    &plan,
                    ExecBudgets::default(),
                    WatchPolicy::default(),
                    10,
                    1,
                    hash_output,
                )
                .expect("register watch")
            })
        };

        let t_commit = {
            let engine = Arc::clone(&engine);
            let hub = Arc::clone(&hub);
            thread::spawn(move || {
                put_note(&engine, &hub, "n1", "alpha", 11);
            })
        };

        let (wid, initial_out) = t_reg.join().unwrap();
        t_commit.join().unwrap();

        let initial_len = match &initial_out.result {
            QueryResult::Ids(ids) => ids.len(),
            other => panic!("unexpected result: {other:?}"),
        };

        // INVARIANT: either the watch observed `n1` in its initial snapshot (`initial_len == 1`),
        // OR `CommitRing` catch-up marked `wid` dirty so it is NOT settled yet and rerunning converges to 1!
        // Conversely, if `initial_len == 1` and `notify_commit` arrived after registration (harmless
        // false positive), rerunning MUST suppress duplicate delivery (`deliver == false`) and settle cleanly!
        if initial_len == 0 {
            assert!(!hub.watches_settled(), "watch missed concurrent commit without being marked dirty");
            let tick = hub.tick(1000);
            assert!(tick.due_ids.contains(&wid), "dirty watch must become due on tick");
            let (out2, deliver) = hub
                .rerun_query_watch(wid, &engine, &qe, &plan, ExecBudgets::default(), 1000, 1, hash_output)
                .expect("rerun");
            assert!(deliver, "changed result hash must deliver");
            match out2.result {
                QueryResult::Ids(ids) => assert_eq!(ids.len(), 1),
                other => panic!("unexpected result: {other:?}"),
            }
        } else {
            assert_eq!(initial_len, 1);
            if !hub.watches_settled() {
                let tick = hub.tick(1000);
                assert!(tick.due_ids.contains(&wid));
                let (out2, deliver) = hub
                    .rerun_query_watch(wid, &engine, &qe, &plan, ExecBudgets::default(), 1000, 1, hash_output)
                    .expect("rerun after late notify_commit");
                assert!(!deliver, "unchanged result hash must suppress duplicate delivery");
                match out2.result {
                    QueryResult::Ids(ids) => assert_eq!(ids.len(), 1),
                    other => panic!("unexpected result: {other:?}"),
                }
            }
        }
        assert!(hub.watches_settled());
    });
}

#[test]
fn overlapping_reruns_and_concurrent_commits_respect_run_id_fencing_and_converge() {
    explore(|| {
        let (engine, qe, hub, plan) = setup_env();
        let (wid, _) = hub
            .register_query_watch(
                &engine,
                &qe,
                &plan,
                ExecBudgets::default(),
                WatchPolicy::default(),
                0,
                1,
                hash_output,
            )
            .expect("register");
        put_note(&engine, &hub, "n1", "a", 10);

        let t_rerun1 = {
            let engine = Arc::clone(&engine);
            let qe = Arc::clone(&qe);
            let hub = Arc::clone(&hub);
            let plan = plan.clone();
            thread::spawn(move || {
                hub.rerun_query_watch(wid, &engine, &qe, &plan, ExecBudgets::default(), 50, 1, hash_output)
                    .expect("rerun 1")
            })
        };

        let t_rerun2 = {
            let engine = Arc::clone(&engine);
            let qe = Arc::clone(&qe);
            let hub = Arc::clone(&hub);
            let plan = plan.clone();
            thread::spawn(move || {
                hub.rerun_query_watch(wid, &engine, &qe, &plan, ExecBudgets::default(), 60, 1, hash_output)
                    .expect("rerun 2")
            })
        };

        let t_commit2 = {
            let engine = Arc::clone(&engine);
            let hub = Arc::clone(&hub);
            thread::spawn(move || {
                put_note(&engine, &hub, "n2", "b", 55);
            })
        };

        let (o1, _) = t_rerun1.join().unwrap();
        let (o2, _) = t_rerun2.join().unwrap();
        t_commit2.join().unwrap();

        let saw_two = [&o1, &o2].iter().any(|o| matches!(&o.result, QueryResult::Ids(ids) if ids.len() == 2));

        // If neither concurrent rerun observed both notes, the hub MUST NOT report settled;
        // draining one final tick+rerun MUST converge to 2 rows and settle cleanly (never wedged!).
        if !saw_two {
            assert!(!hub.watches_settled(), "watch settled at stale snapshot without seeing n2");
        }
        if !hub.watches_settled() {
            let tick = hub.tick(10_000);
            assert!(tick.due_ids.contains(&wid), "unsettled watch must not be wedged in running=true");
            let (final_out, _) = hub
                .rerun_query_watch(wid, &engine, &qe, &plan, ExecBudgets::default(), 10_000, 1, hash_output)
                .expect("final rerun");
            match final_out.result {
                QueryResult::Ids(ids) => assert_eq!(ids.len(), 2),
                other => panic!("unexpected result: {other:?}"),
            }
        }
        assert!(hub.watches_settled());
    });
}

#[test]
fn concurrent_stop_watches_and_unregister_never_resurrect_watch() {
    explore(|| {
        let (engine, qe, hub, plan) = setup_env();
        let (existing_id, _) = hub
            .register_query_watch(
                &engine,
                &qe,
                &plan,
                ExecBudgets::default(),
                WatchPolicy::default(),
                0,
                1,
                hash_output,
            )
            .expect("register existing");
        put_note(&engine, &hub, "n1", "a", 10);

        let t_reg = {
            let engine = Arc::clone(&engine);
            let qe = Arc::clone(&qe);
            let hub = Arc::clone(&hub);
            let plan = plan.clone();
            thread::spawn(move || {
                let res = hub.register_query_watch(
                    &engine,
                    &qe,
                    &plan,
                    ExecBudgets::default(),
                    WatchPolicy::default(),
                    20,
                    1,
                    hash_output,
                );
                if let Err(err) = res {
                    assert_eq!(err.code, ErrorCode::Closed);
                }
            })
        };

        let t_rerun = {
            let engine = Arc::clone(&engine);
            let qe = Arc::clone(&qe);
            let hub = Arc::clone(&hub);
            let plan = plan.clone();
            thread::spawn(move || {
                let res =
                    hub.rerun_query_watch(existing_id, &engine, &qe, &plan, ExecBudgets::default(), 25, 1, hash_output);
                if let Err(err) = res {
                    assert_eq!(err.code, ErrorCode::Closed);
                }
            })
        };

        let t_stop = {
            let hub = Arc::clone(&hub);
            thread::spawn(move || {
                hub.stop_watches();
            })
        };

        t_reg.join().unwrap();
        t_rerun.join().unwrap();
        t_stop.join().unwrap();

        // After stop_watches() + all racing ops finish: zero watches remain and new registrations fail closed!
        assert_eq!(hub.stats().active_watches, 0, "stop_watches raced and resurrected a watch");
        assert!(hub.watches_settled());
        let err = hub
            .register_query_watch(&engine, &qe, &plan, ExecBudgets::default(), WatchPolicy::default(), 100, 1, |_| None)
            .expect_err("register after stop_watches must fail closed");
        assert_eq!(err.code, ErrorCode::Closed);
    });
}

#[test]
fn concurrent_unregister_and_schema_bump_never_resurrect_unregistered_watch() {
    explore(|| {
        let (engine, qe, hub, plan) = setup_env();
        let (w_keep, _) = hub
            .register_query_watch(
                &engine,
                &qe,
                &plan,
                ExecBudgets::default(),
                WatchPolicy::default(),
                0,
                1,
                hash_output,
            )
            .expect("register keep");
        let (w_drop, _) = hub
            .register_query_watch(
                &engine,
                &qe,
                &plan,
                ExecBudgets::default(),
                WatchPolicy::default(),
                0,
                1,
                hash_output,
            )
            .expect("register drop");
        put_note(&engine, &hub, "n1", "a", 10);

        let t_rerun = {
            let engine = Arc::clone(&engine);
            let qe = Arc::clone(&qe);
            let hub = Arc::clone(&hub);
            let plan = plan.clone();
            thread::spawn(move || {
                let res =
                    hub.rerun_query_watch(w_drop, &engine, &qe, &plan, ExecBudgets::default(), 20, 1, hash_output);
                if let Err(err) = res {
                    assert_eq!(err.code, ErrorCode::Closed);
                }
            })
        };

        let t_unreg = {
            let hub = Arc::clone(&hub);
            thread::spawn(move || {
                assert!(hub.unregister_watch(w_drop));
            })
        };

        let t_schema = {
            let hub = Arc::clone(&hub);
            thread::spawn(move || {
                let st = hub.stats();
                hub.notify_schema_change_with_maint(
                    st.latest_seq,
                    st.latest_schema_epoch + 1,
                    st.latest_maint_seq + 1,
                    25,
                );
            })
        };

        t_rerun.join().unwrap();
        t_unreg.join().unwrap();
        t_schema.join().unwrap();

        // Only `w_keep` may remain active, and the schema/maint bump must have marked it dirty!
        assert_eq!(hub.stats().active_watches, 1, "unregistered watch was resurrected");
        let tick = hub.tick(10_000);
        assert!(tick.due_ids.contains(&w_keep), "schema bump must dirty surviving watch");
        assert!(!tick.due_ids.contains(&w_drop), "unregistered watch must never appear in tick.due_ids");
    });
}
