//! Tracked reads and watches (`docs/architecture/engine.md`, Change sets and watch): a commit names the tokens whose
//! reads it wrote into and no others, carries a sequence that only increases, and runs the database's watches again
//! when it writes into what they read, delivering only an answer that changed.

use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};

use super::{batch, open, write};
use crate::Database;
use crate::engine::plan::{ReadOp, Reads};

fn reads(ops: Vec<Json>, track: Option<u32>) -> Reads {
    Reads { reads: ops.into_iter().map(|op| serde_json::from_value::<ReadOp>(op).unwrap()).collect(), track }
}

/// The Tasks' ids, in creation order
fn tasks() -> Json {
    json!({ "read": "query", "plan": { "start": { "from": "types", "types": ["Task"] }, "terminal": { "answer": "ids" } } })
}

fn row(id: &str) -> Json {
    json!({ "read": "row", "id": id })
}

fn touched(db: &Database, ops: Vec<Json>) -> Vec<u32> {
    db.write(&batch(ops), false).unwrap().1.touched
}

#[test]
fn a_commit_names_the_tokens_whose_reads_it_wrote_into_and_no_others() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-1", "fields": { "a": 1 } })]).unwrap();
    db.read(&reads(vec![tasks()], Some(1))).unwrap();
    db.read(&reads(vec![row("Note-1")], Some(2))).unwrap();
    // Reads under one token add to what it holds
    db.read(&reads(vec![row("Task-9")], Some(2))).unwrap();

    assert_eq!(touched(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })]), [1]);
    assert_eq!(touched(&db, vec![json!({ "op": "set", "id": "Note-1", "fields": { "b": 2 } })]), [2]);
    // Another note, and a field of a task the first token's ids didn't read, are writes into neither
    assert!(touched(&db, vec![json!({ "op": "create", "type": "Note", "id": "Note-2" })]).is_empty());
    assert!(touched(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "a": 5 } })]).is_empty());
    // A key a read got and found absent is what it read too
    assert_eq!(touched(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-9" })]), [1, 2]);

    db.untrack(1);
    assert!(touched(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-2" })]).is_empty());
    assert_eq!(db.track_stats().0, 1);
}

#[test]
fn a_scan_a_read_stopped_is_tracked_as_far_as_it_went() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    for id in ["Task-1", "Task-2", "Task-3"] {
        write(&db, vec![json!({ "op": "create", "type": "Task", "id": id })]).unwrap();
    }
    let first = json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Task"] }, "terminal": { "answer": "first" } } });
    db.read(&reads(vec![first], Some(7))).unwrap();
    // The scan stopped at the first task: a task after it is no write into what it read, one before it is
    assert!(touched(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-4" })]).is_empty());
    assert!(touched(&db, vec![json!({ "op": "destroy", "id": "Task-2" })]).is_empty());
    assert_eq!(touched(&db, vec![json!({ "op": "destroy", "id": "Task-1" })]), [7]);
}

#[test]
fn a_unit_names_its_tokens_when_it_commits_and_a_refused_batch_commits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.read(&reads(vec![tasks()], Some(3))).unwrap();
    let before = db.sequence().unwrap();
    super::refused(
        &db,
        vec![json!({ "op": "create", "type": "Task", "id": "Task-1" }), json!({ "op": "create", "type": "Nope" })],
    );
    assert_eq!(db.sequence().unwrap(), before, "a refused batch commits nothing");

    let mut unit = db.begin().unwrap();
    unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })])).unwrap();
    let changes = unit.commit(false).unwrap();
    assert_eq!(changes.touched, [3]);
    assert_eq!(changes.sequence, before + 1);

    let mut unit = db.begin().unwrap();
    unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-2" })])).unwrap();
    drop(unit);
    assert_eq!(db.sequence().unwrap(), before + 1, "an aborted unit commits nothing");
}

#[test]
fn the_sequence_rises_by_one_a_commit_and_outlasts_a_reset_and_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let start = db.sequence().unwrap();
    let (_, changes) =
        db.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })]), false).unwrap();
    assert_eq!(changes.sequence, start + 1);
    let written = db.write_bytes(&rmp_serde::to_vec_named(&batch(vec![])).unwrap(), false).unwrap();
    let written: Json = serde_json::to_value(rmpv::decode::read_value(&mut &written[..]).unwrap()).unwrap();
    assert_eq!(written["sequence"], start + 2);
    assert_eq!(written["changes"]["sequence"], start + 2);
    db.reset().unwrap();
    let after_reset = db.sequence().unwrap();
    assert!(after_reset > start + 2);
    db.close().unwrap();
    let db = open(&dir);
    assert_eq!(db.sequence().unwrap(), after_reset);
    write(&db, vec![]).unwrap();
    assert_eq!(db.sequence().unwrap(), after_reset + 1);
}

#[test]
fn a_watch_delivers_each_answer_that_changed_after_a_commit_into_what_it_read() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": 1 } })]).unwrap();
    let heard: Arc<Mutex<Vec<Json>>> = Arc::default();
    let into = Arc::clone(&heard);
    let id = db
        .watch(
            reads(vec![row("Task-1")], None),
            Box::new(move |answer| {
                let bytes = answer.as_ref().unwrap();
                let answer: Json = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
                into.lock().unwrap().push(answer[0]["row"]["a"].clone());
            }),
        )
        .unwrap();
    let heard = move || std::mem::take(&mut *heard.lock().unwrap());
    assert_eq!(heard(), [json!(1)], "the first answer, as the watch begins");

    write(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "a": 2 } })]).unwrap();
    assert_eq!(heard(), [json!(2)]);
    // Written into, the same answer: not delivered
    write(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "a": 2 }, "touch": false })]).unwrap();
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-2" })]).unwrap();
    assert!(heard().is_empty());
    // A reset is heard as writing everything
    db.reset().unwrap();
    assert_eq!(heard(), [json!(null)]);
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": 3 } })]).unwrap();
    assert_eq!(heard(), [json!(3)]);

    db.unwatch(id);
    write(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "a": 4 } })]).unwrap();
    assert!(heard().is_empty());
    assert_eq!(db.track_stats(), (0, 0, 0));
}

#[test]
fn a_watch_whose_listener_writes_into_what_it_reads_hears_that_write() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(open(&dir));
    write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": 1 } })]).unwrap();
    let heard: Arc<Mutex<Vec<Json>>> = Arc::default();
    let into = Arc::clone(&heard);
    let writer = Arc::downgrade(&db);
    db.watch(
        reads(vec![row("Task-1")], None),
        Box::new(move |answer| {
            let bytes = answer.as_ref().unwrap();
            let answer: Json = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
            let a = answer[0]["row"]["a"].clone();
            into.lock().unwrap().push(a.clone());
            // On seeing 2, the listener writes 3 into the row it reads
            if a == json!(2)
                && let Some(db) = writer.upgrade()
            {
                write(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "a": 3 } })]).unwrap();
            }
        }),
    )
    .unwrap();
    write(&db, vec![json!({ "op": "set", "id": "Task-1", "fields": { "a": 2 } })]).unwrap();
    assert_eq!(*heard.lock().unwrap(), [json!(1), json!(2), json!(3)]);
}

#[test]
fn a_watch_that_failed_delivers_the_next_answer_even_one_equal_to_the_last_before_the_failure() {
    let with_task = |task: bool| {
        let mut types = json!({ "Note": { "fields": {} } });
        if task {
            types["Task"] = json!({ "fields": {} });
        }
        crate::schema::Schema::from_json(&json!({ "$schema": crate::schema::SCHEMA_LANGUAGE, "types": types })).unwrap()
    };
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path(),
        crate::OpenOptions { schema: Some(with_task(true)), syncs: false, ..Default::default() },
    )
    .unwrap();
    let heard: Arc<Mutex<Vec<Json>>> = Arc::default();
    let into = Arc::clone(&heard);
    db.watch(
        reads(vec![tasks()], None),
        Box::new(move |answer| {
            let heard = match answer {
                Ok(bytes) => {
                    let answer: Json =
                        serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
                    answer[0]["ids"].clone()
                }
                Err(e) => json!(e.code),
            };
            into.lock().unwrap().push(heard);
        }),
    )
    .unwrap();
    // Without the type the query fails; with it back, the answer is the one heard before the failure, and is heard
    db.set_schema(with_task(false)).unwrap();
    db.set_schema(with_task(true)).unwrap();
    let unknown = json!(crate::error::ErrorCode::UnknownType);
    assert_eq!(*heard.lock().unwrap(), [json!([]), unknown, json!([])]);
}

#[test]
fn a_panicking_listener_is_removed_without_changing_the_commit_outcome() {
    use crate::error::ErrorCode;
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let error = db.watch(reads(vec![tasks()], None), Box::new(|_| panic!("initial listener"))).unwrap_err();
    assert_eq!(error.code, ErrorCode::CallbackPanicked);
    assert_eq!(db.track_stats(), (0, 0, 0));
    let mut first = true;
    let bad = db
        .watch(
            reads(vec![tasks()], None),
            Box::new(move |_| {
                if first {
                    first = false;
                } else {
                    panic!("subsequent listener");
                }
            }),
        )
        .unwrap();
    let heard = Arc::new(Mutex::new(0));
    let listener_heard = heard.clone();
    let good = db
        .watch(
            reads(vec![tasks()], None),
            Box::new(move |_| {
                *listener_heard.lock().unwrap() += 1;
            }),
        )
        .unwrap();
    let before = db.sequence().unwrap();
    write(&db, vec![json!({"op":"create", "type":"Task", "id":"Task-panic"})]).unwrap();
    assert_eq!(db.sequence().unwrap(), before + 1);
    assert_eq!(*heard.lock().unwrap(), 2, "later listeners still run");
    assert_eq!(db.track_stats().0, 1);
    db.unwatch(bad);
    write(&db, vec![json!({"op":"create", "type":"Task", "id":"Task-after-panic"})]).unwrap();
    assert_eq!(*heard.lock().unwrap(), 3);
    db.unwatch(good);
    assert_eq!(db.track_stats(), (0, 0, 0));
    db.close().unwrap();
}

fn bounded_watch_child(test: &str) -> bool {
    // A regression must fail within a deadline rather than hang the entire test runner.
    const CHILD: &str = "ELMDB_TEST_BOUNDED_WATCH_CHILD";
    if std::env::var_os(CHILD).as_deref() != Some(std::ffi::OsStr::new(test)) {
        let marker = tempfile::tempdir().unwrap();
        let entered = marker.path().join("entered");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, test)
            .env("ELMDB_TEST_BOUNDED_WATCH_MARKER", &entered)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "callback close subprocess failed: {status}");
                assert_eq!(
                    std::fs::read_to_string(&entered).unwrap(),
                    "entered",
                    "the selected child test must actually run"
                );
                return true;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("callback close subprocess exceeded ten seconds");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    if let Some(marker) = std::env::var_os("ELMDB_TEST_BOUNDED_WATCH_MARKER") {
        std::fs::write(marker, "entered").unwrap();
    }
    false
}

#[test]
fn callback_close_refuses_before_taking_the_store() {
    if bounded_watch_child("engine::tests::tracked::callback_close_refuses_before_taking_the_store") {
        return;
    }
    use crate::error::ErrorCode;
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(open(&dir));
    let weak = Arc::downgrade(&db);
    let completed = Arc::new(Mutex::new(0));
    let heard = completed.clone();
    let id = db
        .watch(
            reads(vec![tasks()], None),
            Box::new(move |_| {
                let db = weak.upgrade().unwrap();
                assert_eq!(db.close().unwrap_err().code, ErrorCode::CallbackActive);
                assert_eq!(
                    db.close_within(Some(std::time::Duration::ZERO)).unwrap_err().code,
                    ErrorCode::CallbackActive
                );
                assert!(db.is_open());
                db.sequence().unwrap();
                *heard.lock().unwrap() += 1;
            }),
        )
        .unwrap();
    write(&db, vec![json!({"op":"create", "type":"Task", "id":"Task-close"})]).unwrap();
    assert_eq!(*completed.lock().unwrap(), 2, "both callbacks completed without panicking");
    db.unwatch(id);
    db.close().unwrap();
}

#[test]
fn callback_schema_unwatch_and_nested_delivery_terminate() {
    if bounded_watch_child("engine::tests::tracked::callback_schema_unwatch_and_nested_delivery_terminate") {
        return;
    }
    for action in ["schema", "unwatch", "nested"] {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(open(&dir));
        let weak = Arc::downgrade(&db);
        let registration = Arc::new(Mutex::new(None));
        let callback_registration = registration.clone();
        let completed = Arc::new(Mutex::new(0));
        let heard = completed.clone();
        let mut first = true;
        let id = db
            .watch(
                reads(vec![tasks()], None),
                Box::new(move |_| {
                    if first {
                        first = false;
                        return;
                    }
                    let db = weak.upgrade().unwrap();
                    match action {
                        "schema" => {
                            let schema = crate::schema::Schema::from_json(&json!({
                                "$schema": crate::schema::SCHEMA_LANGUAGE,
                                "types": {"Task": {"shortCode": {"prefix": "TSK"}, "fields": {}}, "Extra": {"fields": {}}}
                            }))
                            .unwrap();
                            db.set_schema(schema).unwrap();
                            assert!(db.schema().unwrap().types.contains_key("Extra"));
                        }
                        "unwatch" => db.unwatch(callback_registration.lock().unwrap().unwrap()),
                        "nested" => {
                            let other_dir = tempfile::tempdir().unwrap();
                            let other = Arc::new(open(&other_dir));
                            let outer = Arc::downgrade(&db);
                            other
                                .watch(
                                    reads(vec![tasks()], None),
                                    Box::new(move |_| {
                                        assert_eq!(
                                            outer.upgrade().unwrap().close().unwrap_err().code,
                                            crate::error::ErrorCode::CallbackActive
                                        );
                                    }),
                                )
                                .unwrap();
                            other.close().unwrap();
                            write(&db, vec![json!({"op":"create", "type":"Task", "id":"Task-nested"})]).unwrap();
                        }
                        _ => unreachable!(),
                    }
                    // Unregister here also bounds self-induced deliveries: a changed schema and nested write named us again.
                    db.unwatch(callback_registration.lock().unwrap().unwrap());
                    *heard.lock().unwrap() += 1;
                }),
            )
            .unwrap();
        *registration.lock().unwrap() = Some(id);
        write(&db, vec![json!({"op":"create", "type":"Task", "id":"Task-trigger"})]).unwrap();
        assert_eq!(*completed.lock().unwrap(), 1, "{action} callback completed without panic");
        assert_eq!(db.track_stats(), (0, 0, 0));
        db.close().unwrap();
    }
}

#[test]
fn database_cleanup_after_callback_tls_destruction_terminates() {
    if bounded_watch_child("engine::tests::tracked::database_cleanup_after_callback_tls_destruction_terminates") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    std::thread::spawn(move || {
        thread_local! {
            static EXIT_DATABASE: std::cell::RefCell<Option<Database>> = const { std::cell::RefCell::new(None) };
        }
        // TLS destructors run in reverse initialization order: the database drops after callback TLS is destroyed.
        EXIT_DATABASE.with_borrow(|slot| assert!(slot.is_none()));
        let db = Database::open(&path, crate::OpenOptions { syncs: false, ..Default::default() }).unwrap();
        db.watch(reads(Vec::new(), None), Box::new(|_| {})).unwrap();
        EXIT_DATABASE.with_borrow_mut(|slot| *slot = Some(db));
    })
    .join()
    .unwrap();
    Database::open(dir.path(), crate::OpenOptions { syncs: false, ..Default::default() }).unwrap().close().unwrap();
}

#[test]
fn watch_delivery_from_thread_exit_destructors_is_guarded() {
    if bounded_watch_child("engine::tests::tracked::watch_delivery_from_thread_exit_destructors_is_guarded") {
        return;
    }
    struct OnExit(Option<Arc<Database>>);
    impl Drop for OnExit {
        fn drop(&mut self) {
            if let Some(db) = self.0.take() {
                write(&db, vec![json!({"op":"create", "type":"Task", "id":"Task-exit"})]).unwrap();
                db.close().unwrap();
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let delivered = seen.clone();
    std::thread::spawn(move || {
        thread_local! { static ON_EXIT: std::cell::RefCell<OnExit> = const { std::cell::RefCell::new(OnExit(None)) }; }
        // Initialize this destructor before the callback state: the old callback Vec is destroyed first.
        ON_EXIT.with_borrow(|slot| assert!(slot.0.is_none()));
        let db = Arc::new(
            Database::open(
                &path,
                crate::OpenOptions { schema: Some(super::schema()), syncs: false, ..Default::default() },
            )
            .unwrap(),
        );
        let weak = Arc::downgrade(&db);
        db.watch(
            reads(vec![tasks()], None),
            Box::new(move |_| {
                assert_eq!(weak.upgrade().unwrap().close().unwrap_err().code, crate::ErrorCode::CallbackActive);
                delivered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        )
        .unwrap();
        ON_EXIT.with_borrow_mut(|slot| slot.0 = Some(db));
    })
    .join()
    .unwrap();
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 2);
    open(&dir).close().unwrap();
}

/// Run explicitly in release mode on an idle machine; reports times rather than enforcing a machine-specific limit.
#[test]
#[ignore = "tracking contention measurement"]
fn measure_tracking_contention() {
    use std::time::Instant;
    fn distribution(label: &str, mut times: Vec<u128>) {
        times.sort_unstable();
        println!(
            "{label}: samples={} p50_us={} p95_us={} max_us={}",
            times.len(),
            times[times.len() / 2],
            times[(times.len() - 1) * 95 / 100],
            times.last().unwrap()
        );
    }
    for (mode, writes) in [("locked", 16), ("token", 16), ("watch", 128), ("token", 128), ("token", 16), ("locked", 16)]
    {
        for size in [2_000, 20_000] {
            let dir = tempfile::tempdir().unwrap();
            let db = Arc::new(open(&dir));
            write(
                &db,
                (0..size)
                    .map(|i| {
                        json!({"op":"create", "type":"Task", "id":format!("Task-{i}"),
            "fields":{"title":format!("row {i}")}})
                    })
                    .collect(),
            )
            .unwrap();
            let scan = reads(
                vec![json!({"read":"query", "plan":{
            "start":{"from":"types", "types":["Task"]},
            "steps":[{"step":"where", "field":"title", "value":"absent"}],
            "terminal":{"answer":"count"}}})],
                None,
            );
            let mut baseline = Vec::new();
            for _ in 0..8 {
                let start = Instant::now();
                db.read(&scan).unwrap();
                baseline.push(start.elapsed().as_micros());
            }
            println!("mode={mode}; rows={size}; two readers x eight full scans; writes={writes}; syncs=false");
            distribution("untracked_read", baseline);
            let barrier = Arc::new(std::sync::Barrier::new(3));
            let readers: Vec<_> = (1..=2)
                .map(|token| {
                    let db = db.clone();
                    let barrier = barrier.clone();
                    let mut scan = scan.clone();
                    scan.track = Some(token);
                    std::thread::spawn(move || {
                        barrier.wait();
                        let mut samples = Vec::new();
                        for _ in 0..8 {
                            db.untrack(token);
                            let start = Instant::now();
                            match mode {
                                "locked" => {
                                    // The previous algorithm, kept only in the measurement for alternating comparison.
                                    let store = db.store().unwrap();
                                    let schema = db.engine_schema();
                                    let mut held = db.tracks.lock();
                                    let (answer, dependency) =
                                        crate::engine::watch::run_tracked(&store, &schema, &scan);
                                    answer.unwrap();
                                    held.file(token, dependency);
                                }
                                "watch" => {
                                    let id = db.watch(scan.clone(), Box::new(|_| {})).unwrap();
                                    db.unwatch(id);
                                }
                                _ => {
                                    db.read(&scan).unwrap();
                                }
                            }
                            samples.push(start.elapsed().as_micros());
                        }
                        samples
                    })
                })
                .collect();
            barrier.wait();
            let mut waits = Vec::new();
            let mut commits = Vec::new();
            for i in 0..writes {
                let start = Instant::now();
                drop(db.tracks.lock());
                waits.push(start.elapsed().as_micros());
                let start = Instant::now();
                write(&db, vec![json!({"op":"set", "id":"Task-0", "fields":{"a":i}})]).unwrap();
                commits.push(start.elapsed().as_micros());
                if writes > 16 {
                    std::thread::sleep(std::time::Duration::from_micros(250));
                }
            }
            distribution("tracking_lock_wait", waits);
            distribution("write_end_to_end", commits);
            distribution("tracked_read", readers.into_iter().flat_map(|r| r.join().unwrap()).collect());
        }
    }
}

#[test]
fn watch_delivery_refreshes_schema_after_callbacks_for_initial_batch_and_unit_delivery() {
    if bounded_watch_child(
        "engine::tests::tracked::watch_delivery_refreshes_schema_after_callbacks_for_initial_batch_and_unit_delivery",
    ) {
        return;
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    let schema = |task: bool| {
        crate::schema::Schema::from_json(&json!({
            "$schema":crate::schema::SCHEMA_LANGUAGE,
            "types":if task {json!({"Task":{"fields":{}}})} else {json!({})}
        }))
        .unwrap()
    };
    for trigger in ["initial", "batch", "unit"] {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            Database::open(
                dir.path(),
                crate::OpenOptions { schema: Some(schema(true)), syncs: false, ..Default::default() },
            )
            .unwrap(),
        );
        let weak = Arc::downgrade(&db);
        let armed = Arc::new(AtomicBool::new(trigger == "initial"));
        let callback_armed = armed.clone();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let into = heard.clone();
        let mut phase = 0;
        let id = db
            .watch(
                reads(vec![tasks()], None),
                Box::new(move |answer| {
                    into.lock().unwrap().push(answer.as_ref().err().map(|e| e.code));
                    if !callback_armed.load(Ordering::SeqCst) {
                        return;
                    }
                    let db = weak.upgrade().unwrap();
                    match phase {
                        0 => {
                            phase = 1;
                            db.set_schema(schema(false)).unwrap();
                        }
                        1 => {
                            phase = 2;
                            db.set_schema(schema(true)).unwrap();
                        }
                        _ => {}
                    }
                }),
            )
            .unwrap();
        armed.store(true, Ordering::SeqCst);
        if trigger == "batch" {
            write(&db, vec![json!({"op":"create", "type":"Task", "id":"Task-wake"})]).unwrap();
        } else if trigger == "unit" {
            let mut unit = db.begin().unwrap();
            unit.write(&batch(vec![json!({"op":"create", "type":"Task", "id":"Task-wake"})])).unwrap();
            unit.commit(false).unwrap();
        }
        let heard = heard.lock().unwrap();
        assert_eq!(
            &heard[heard.len().saturating_sub(3)..],
            &[None, Some(crate::error::ErrorCode::UnknownType), None],
            "{trigger}: {heard:?}"
        );
        db.unwatch(id);
        db.close().unwrap();
    }
}

#[test]
fn a_threaded_read_holds_its_schema_before_taking_the_snapshot() {
    if bounded_watch_child("engine::tests::tracked::a_threaded_read_holds_its_schema_before_taking_the_snapshot") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(open(&dir));
    let entered = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));
    let reader = db.clone();
    let paused = entered.clone();
    let resumed = resume.clone();
    let thread = std::thread::spawn(move || {
        crate::engine::BEFORE_READ_SNAPSHOT.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || {
                paused.wait();
                resumed.wait();
            }))
        });
        reader.read(&reads(vec![tasks()], None)).unwrap()
    });
    entered.wait();
    let schema_was_free = db.observing_schema().try_write().is_some();
    resume.wait();
    let bytes = thread.join().unwrap();
    assert!(!schema_was_free, "a schema change must not cross the schema/snapshot acquisition gap");
    let answer: Json = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    assert_eq!(answer[0]["ids"], json!([]));
}

#[test]
fn an_export_snapshot_keeps_its_schema_while_callbacks_can_change_the_live_schema() {
    if bounded_watch_child(
        "engine::tests::tracked::an_export_snapshot_keeps_its_schema_while_callbacks_can_change_the_live_schema",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.primary_snapshot(|declared, snapshot| {
        assert!(declared.unwrap().types.contains_key("Task"));
        db.set_schema(crate::schema::Schema::from_json(&json!({"$schema":crate::schema::SCHEMA_LANGUAGE})).unwrap())?;
        let bytes = crate::engine::reads::run(snapshot, &reads(vec![tasks()], None).reads)?;
        let answer: Json = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
        assert_eq!(answer[0]["ids"], json!([]));
        assert_eq!(db.read(&reads(vec![tasks()], None)).unwrap_err().code, crate::error::ErrorCode::UnknownType);
        Ok(())
    })
    .unwrap();
}

#[test]
fn dropping_a_listeners_last_database_reference_does_not_wait_on_its_unit() {
    if bounded_watch_child(
        "engine::tests::tracked::dropping_a_listeners_last_database_reference_does_not_wait_on_its_unit",
    ) {
        return;
    }
    for action in ["unwatch", "panic"] {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(open(&dir));
        let weak = Arc::downgrade(&db);
        let callback_db = db.clone();
        let identity = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let into = identity.clone();
        let mut initial = true;
        let id = db
            .watch(
                reads(vec![tasks()], None),
                Box::new(move |_| {
                    if initial {
                        initial = false;
                        return;
                    }
                    if action == "panic" {
                        panic!("listener panic with last database reference");
                    }
                    callback_db.unwatch(into.load(std::sync::atomic::Ordering::SeqCst));
                }),
            )
            .unwrap();
        identity.store(id, std::sync::atomic::Ordering::SeqCst);
        let mut unit = db.begin().unwrap();
        drop(db);
        unit.write(&serde_json::from_value(json!({"ops":[{"op":"create","type":"Task","id":"Task-last"}]})).unwrap())
            .unwrap();
        unit.commit(false).unwrap();
        assert!(weak.upgrade().is_none());
    }
}

#[test]
fn a_panicking_watch_read_is_removed_and_does_not_hide_failed_durability() {
    let dir = tempfile::tempdir().unwrap();
    let db =
        Database::open(dir.path(), crate::OpenOptions { schema: Some(super::schema()), ..Default::default() }).unwrap();
    crate::engine::watch::PANIC_NEXT_READ.set(true);
    assert_eq!(
        db.watch(reads(vec![tasks()], None), Box::new(|_| {})).unwrap_err().code,
        crate::ErrorCode::WatchPanicked
    );
    assert_eq!(db.track_stats().0, 0);
    db.watch(reads(vec![tasks()], None), Box::new(|_| {})).unwrap();
    let heard = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let into = heard.clone();
    db.watch(
        reads(vec![tasks()], None),
        Box::new(move |_| {
            into.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }),
    )
    .unwrap();
    let before = db.sequence().unwrap();
    let _failure = super::commits::FailedSync::arm(&db);
    crate::engine::watch::PANIC_NEXT_READ.set(true);
    let result = db.write(&batch(vec![json!({"op":"create","type":"Task","id":"Task-read-panic"})]), true);
    assert_eq!(result.unwrap_err().code, crate::ErrorCode::NotDurable);
    assert_eq!(db.sequence().unwrap(), before + 1);
    assert_eq!(db.track_stats().0, 1);
    assert_eq!(heard.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(db.close().is_err());
}

#[test]
fn a_unit_opened_between_writer_checks_is_refused_without_holding_schema() {
    if bounded_watch_child(
        "engine::tests::tracked::a_unit_opened_between_writer_checks_is_refused_without_holding_schema",
    ) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(open(&dir));
    let (ready, queued) = std::sync::mpsc::channel();
    let (proceed, start) = std::sync::mpsc::channel();
    let (finished, result) = std::sync::mpsc::channel();
    let writer = db.clone();
    let handle = std::thread::spawn(move || {
        crate::store::BEFORE_WRITE_RESERVATION.with_borrow_mut(|hook| {
            *hook = Some(Box::new(move || {
                ready.send(()).unwrap();
                start.recv().unwrap();
            }))
        });
        finished.send(writer.write(&batch(Vec::new()), false).map(|_| ())).unwrap();
    });
    queued.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
    let unit = db.begin().unwrap();
    proceed.send(()).unwrap();
    assert_eq!(db.set_schema(super::schema()).unwrap_err().code, crate::ErrorCode::UnitOpen);
    assert_eq!(
        result
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("a writer must refuse an open unit rather than wait on it")
            .unwrap_err()
            .code,
        crate::ErrorCode::UnitOpen
    );
    drop(unit);
    handle.join().unwrap();
}
