//! Tracked reads and watches (`docs/architecture/engine.md`, Change sets and watch): a commit names the tokens whose
//! reads it wrote into and no others, carries a sequence that only increases, and runs the database's watches again
//! when it writes into what they read, delivering only an answer that changed.

use crate::engine::watch::Unwatched;
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
fn a_walk_between_two_keys_reads_and_records_nothing_outside_them() {
    use crate::engine::keys;
    use crate::engine::tracking::Recording;
    use crate::engine::unit::Read;
    use crate::format::key::Part;

    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let tasks =
        (1..=5).map(|n| json!({ "op": "create", "type": "Task", "id": format!("Task-{n}"), "fields": { "a": n } }));
    write(&db, tasks.collect()).unwrap();
    let at = |n: f64| keys::index_range("Task", "a", &[Part::Number(n)]);
    // The numbers of the entries a walk hands over, and the ranges it is recorded as having read
    let walked = |back: bool, from: f64, to: f64, stop_at: Option<f64>| {
        db.snapshot(|s| {
            let recording = Recording::new(s);
            let mut seen = Vec::new();
            let each = |key: &[u8], _: &[u8]| {
                let Some(Part::Number(n)) = crate::format::key::decode(key).unwrap().into_iter().nth(3) else {
                    panic!("an entry of a number")
                };
                seen.push(n);
                stop_at != Some(n)
            };
            if back {
                recording.visit_back_within(&at(from), &at(to), each)?;
            } else {
                recording.visit_within(&at(from), &at(to), each)?;
            }
            Ok((seen, recording.into_reads().ranges))
        })
        .unwrap()
    };
    let whole = |from: f64, to: f64| vec![(at(from), Some(at(to)))];
    assert_eq!(walked(false, 2.0, 4.0, None), (vec![2.0, 3.0], whole(2.0, 4.0)));
    assert_eq!(walked(true, 2.0, 4.0, None), (vec![3.0, 2.0], whole(2.0, 4.0)));
    assert_eq!(walked(false, 0.0, 9.0, None).0, [1.0, 2.0, 3.0, 4.0, 5.0]);
    assert_eq!(walked(true, 0.0, 9.0, None).0, [5.0, 4.0, 3.0, 2.0, 1.0]);
    // A walk stopped is recorded as far as it went: up to just past its last entry, or down to it
    let (seen, ranges) = walked(false, 1.0, 5.0, Some(2.0));
    assert_eq!(seen, [1.0, 2.0]);
    assert_eq!(ranges.len(), 1);
    let (low, high) = (&ranges[0].0, ranges[0].1.as_ref().unwrap());
    assert_eq!(low, &at(1.0));
    assert!(high > &at(2.0) && high < &at(2.5), "it ends past the entry of 2 and before any of a greater number");
    let (seen, ranges) = walked(true, 1.0, 5.0, Some(3.0));
    assert_eq!(seen, [4.0, 3.0]);
    assert_eq!(ranges.len(), 1);
    let (low, high) = (&ranges[0].0, ranges[0].1.as_ref().unwrap());
    assert!(low > &at(3.0) && low < &at(3.5), "it starts at the entry of 3");
    assert_eq!(high, &at(5.0));
    // Nothing lies from a key up to itself, or up to one before it
    for back in [false, true] {
        assert!(walked(back, 3.0, 3.0, None).0.is_empty(), "back {back}");
        assert!(walked(back, 4.0, 2.0, None).0.is_empty(), "back {back}");
    }
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

    assert_eq!(db.unwatch(id), Unwatched::Ended);
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
    assert_eq!(db.unwatch(bad), Unwatched::Ended);
    write(&db, vec![json!({"op":"create", "type":"Task", "id":"Task-after-panic"})]).unwrap();
    assert_eq!(*heard.lock().unwrap(), 3);
    assert_eq!(db.unwatch(good), Unwatched::Ended);
    assert_eq!(db.track_stats(), (0, 0, 0));
    db.close().unwrap();
}

#[test]
fn callback_close_refuses_before_taking_the_store() {
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
    assert_eq!(db.unwatch(id), Unwatched::Ended);
    db.close().unwrap();
}

#[test]
fn callback_schema_unwatch_and_nested_delivery_terminate() {
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
                        "unwatch" => {
                            let _ = db.unwatch(callback_registration.lock().unwrap().unwrap());
                        }
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
                    let _ = db.unwatch(callback_registration.lock().unwrap().unwrap());
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
                                    assert_eq!(db.unwatch(id), Unwatched::Ended);
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
        assert_eq!(db.unwatch(id), Unwatched::Ended);
        db.close().unwrap();
    }
}

#[test]
fn a_threaded_read_holds_its_schema_before_taking_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(open(&dir));
    let entered = Arc::new(std::sync::Barrier::new(2));
    let resume = Arc::new(std::sync::Barrier::new(2));
    let reader = db.clone();
    let paused = entered.clone();
    let resumed = resume.clone();
    let thread = std::thread::spawn(move || {
        crate::failpoints::run_once_here(crate::failpoints::points::READ_SNAPSHOT, move || {
            paused.wait();
            resumed.wait();
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
                    let _ = callback_db.unwatch(into.load(std::sync::atomic::Ordering::SeqCst));
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
    crate::failpoints::run_once_here(crate::failpoints::points::WATCH_READ, || panic!("injected watch read panic"));
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
    crate::failpoints::run_once_here(crate::failpoints::points::WATCH_READ, || panic!("injected watch read panic"));
    let result = db.write(&batch(vec![json!({"op":"create","type":"Task","id":"Task-read-panic"})]), true);
    assert_eq!(result.unwrap_err().code, crate::ErrorCode::NotDurable);
    assert_eq!(db.sequence().unwrap(), before + 1);
    assert_eq!(db.track_stats().0, 1);
    assert_eq!(heard.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(db.close().is_err());
}

#[test]
fn a_unit_opened_between_writer_checks_is_refused_without_holding_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(open(&dir));
    let (ready, queued) = std::sync::mpsc::channel();
    let (proceed, start) = std::sync::mpsc::channel();
    let (finished, result) = std::sync::mpsc::channel();
    let writer = db.clone();
    let handle = std::thread::spawn(move || {
        crate::failpoints::run_once_here(crate::failpoints::points::WRITE_RESERVATION, move || {
            ready.send(()).unwrap();
            start.recv().unwrap();
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

/// What a watch's listener does on the builder's thread that needs the schema's lock while another thread changes the
/// schema or closes
#[derive(Clone, Copy)]
enum Needs {
    /// Writes into what its watch reads before the other thread starts, so the delivery reads again once it returns
    AnotherRead,
    /// Calls the database once the other thread has started
    TheDatabase,
}

/// A schema change or a close made while the builder's thread delivers a commit of its own to a watch: both wait for
/// that thread to end, and must not hold what its delivery, or its listener, waits for
fn ends_beside_a_builder_s_delivery(action: &'static str, needs: Needs) {
    use std::time::Duration;
    let schema = |tolerance: f64| {
        let mut json = super::schema_json();
        json["ranks"] = json!({ "taskRank": {
            "types": ["Task"], "relations": ["refs"], "direction": "out", "weights": false, "tolerance": tolerance
        } });
        crate::schema::Schema::from_json(&json).unwrap()
    };
    let dir = tempfile::tempdir().unwrap();
    let options = crate::OpenOptions { schema: Some(schema(1e-9)), syncs: false, ..Default::default() };
    let db = Arc::new(Database::open(dir.path(), options).unwrap());
    write(&db, (0..60).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect())
        .unwrap();
    db.wait_for_index_builds().unwrap();

    // The scores, which the builder's drain moves, so its commit's answer differs and the listener is called on the
    // builder's thread; and a row, which the listener's own write changes
    let top = json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Task"] },
        "steps": [{ "step": "ranked", "index": "taskRank" }, { "step": "limit", "n": 5 }],
        "terminal": { "answer": "hits" } } });
    let (go, told) = std::sync::mpsc::channel();
    let mut go = Some(go);
    let weak = Arc::downgrade(&db);
    db.watch(
        reads(vec![top, row("Task-1")], None),
        Box::new(move |_| {
            if std::thread::current().name() != Some("elmdb-index") {
                return;
            }
            let Some(go) = go.take() else { return };
            let db = weak.upgrade().unwrap();
            if let Needs::AnotherRead = needs {
                let named = json!({ "op": "set", "id": "Task-1", "fields": { "title": "the listener's" } });
                write(&db, vec![named]).unwrap();
            }
            go.send(()).unwrap();
            // Until the other thread has taken what it takes and waits for this one to end
            std::thread::sleep(Duration::from_millis(300));
            if let Needs::TheDatabase = needs {
                // Answered or refused as closed, either way without waiting on the thread that waits for this one
                let _ = db.read(&reads(vec![row("Task-2")], None));
            }
        }),
    )
    .unwrap();
    let links =
        (1..60).map(|i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": "Task-0" }));
    write(&db, links.collect()).unwrap();
    told.recv_timeout(Duration::from_secs(4)).expect("the builder's drain delivered to the watch");

    let (done, returned) = std::sync::mpsc::channel();
    let called = Arc::clone(&db);
    std::thread::spawn(move || {
        let outcome = match action {
            "schema" => called.set_schema(schema(1e-8)).map(|_| ()),
            _ => called.close(),
        };
        let _ = done.send(outcome);
    });
    returned
        .recv_timeout(Duration::from_secs(4))
        .unwrap_or_else(|_| panic!("{action}: it waits for the builder's thread, which waits for what it holds"))
        .unwrap();
}

#[test]
fn a_schema_change_ends_while_the_builder_s_delivery_owes_its_watch_another_read() {
    ends_beside_a_builder_s_delivery("schema", Needs::AnotherRead);
}

#[test]
fn a_schema_change_ends_while_a_listener_on_the_builder_s_thread_calls_the_database() {
    ends_beside_a_builder_s_delivery("schema", Needs::TheDatabase);
}

#[test]
fn a_close_ends_while_the_builder_s_delivery_owes_its_watch_another_read() {
    ends_beside_a_builder_s_delivery("close", Needs::AnotherRead);
}

#[test]
fn a_close_ends_while_a_listener_on_the_builder_s_thread_calls_the_database() {
    ends_beside_a_builder_s_delivery("close", Needs::TheDatabase);
}

/// A database with a rank index over sixty tasks, built; `schema(tolerance)` is its schema with that tolerance, and
/// `top` a read of the index's best, which a drain's commit writes into
/// The tests' schema with a rank index over its tasks, at a tolerance
fn ranked_schema(tolerance: f64) -> crate::schema::Schema {
    let mut json = super::schema_json();
    json["ranks"] = json!({ "taskRank": {
        "types": ["Task"], "relations": ["refs"], "direction": "out", "weights": false, "tolerance": tolerance
    } });
    crate::schema::Schema::from_json(&json).unwrap()
}

fn ranked_tasks() -> (tempfile::TempDir, Arc<Database>, impl Fn(f64) -> crate::schema::Schema, Json) {
    let schema = ranked_schema;
    let dir = tempfile::tempdir().unwrap();
    let options = crate::OpenOptions { schema: Some(schema(1e-9)), syncs: false, ..Default::default() };
    let db = Arc::new(Database::open(dir.path(), options).unwrap());
    write(&db, (0..60).map(|i| json!({ "op": "create", "type": "Task", "id": format!("Task-{i}") })).collect())
        .unwrap();
    db.wait_for_index_builds().unwrap();
    let top = json!({ "read": "query", "plan": {
        "start": { "from": "types", "types": ["Task"] },
        "steps": [{ "step": "ranked", "index": "taskRank" }, { "step": "limit", "n": 5 }],
        "terminal": { "answer": "hits" } } });
    (dir, db, schema, top)
}

/// Links every task to the first, which leaves the rank index a drain to commit
fn link_all(db: &Database) {
    let links =
        (1..60).map(|i| json!({ "op": "link", "source": format!("Task-{i}"), "kind": "refs", "target": "Task-0" }));
    write(db, links.collect()).unwrap();
}

/// A schema change stops the builder and waits for its thread to end. Made by a watch's listener on that thread,
/// which a drain's commit delivered to, it has no one to wait for: the thread ends once the listener returns
#[test]
fn a_listener_on_the_builder_s_thread_changes_the_schema() {
    use std::time::Duration;
    let (_dir, db, schema, top) = ranked_tasks();
    let (tell, told) = std::sync::mpsc::channel();
    let mut tell = Some(tell);
    let weak = Arc::downgrade(&db);
    let changed = schema(1e-8);
    let again = top.clone();
    db.watch(
        reads(vec![top], None),
        Box::new(move |_| {
            if std::thread::current().name() != Some("elmdb-index") {
                return;
            }
            let Some(tell) = tell.take() else { return };
            let db = weak.upgrade().unwrap();
            tell.send(db.set_schema(changed.clone()).map(|changes| changes.len())).unwrap();
        }),
    )
    .unwrap();
    link_all(&db);
    let outcome = told
        .recv_timeout(Duration::from_secs(4))
        .expect("the change waits for the thread it is made on, or that thread ended without making it");
    assert_eq!(outcome.unwrap(), 1, "the tolerance changed");
    // The schema is the one given, and the builder started for it builds what the stopped one left
    assert_eq!(db.schema().unwrap().ranks["taskRank"].tolerance, 1e-8);
    db.wait_for_index_builds().unwrap();
    let report = db.check().unwrap();
    assert!(report.is_clean(), "{report:?}");
    // Built under the new schema, the index is read: the task every other links to is its first
    let ranked = db.read(&reads(vec![again], None)).unwrap();
    let ranked = serde_json::to_value(rmpv::decode::read_value(&mut &ranked[..]).unwrap()).unwrap();
    assert_eq!(ranked[0]["hits"][0]["id"], "Task-0", "{ranked}");
    db.close().unwrap();
}

/// What a listener of the builder's commits asks for on the builder's thread, where it runs, and what it is told:
/// `asks` is handed the database, and its answer comes back
fn asked_on_the_builder_s_thread<T: Send + 'static>(
    asks: impl Fn(&Database) -> T + Send + Sync + 'static,
) -> (tempfile::TempDir, Arc<Database>, T) {
    use std::time::Duration;
    let (dir, db, _, top) = ranked_tasks();
    db.read(&reads(vec![top], Some(7))).unwrap();
    let (tell, told) = std::sync::mpsc::channel();
    let tell = Mutex::new(Some(tell));
    let weak = Arc::downgrade(&db);
    db.hear_builder_commits(Some(Arc::new(move |_, _| {
        let Some(tell) = tell.lock().unwrap().take() else { return };
        assert_eq!(std::thread::current().name(), Some("elmdb-index"));
        tell.send(asks(&weak.upgrade().unwrap())).unwrap();
    })))
    .unwrap();
    link_all(&db);
    let answer = told
        .recv_timeout(Duration::from_secs(4))
        .expect("the builder's commit told its listener, which was answered and not left waiting for its own thread");
    (dir, db, answer)
}

/// A close waits for the store, which the builder's thread holds until it ends: asked for on that thread, by the
/// listener of the builder's commits, it is refused as one made by a watch's listener is, with or without a time
/// to wait
#[test]
fn a_close_on_the_builder_s_thread_is_refused() {
    use std::time::Duration;
    let (_dir, db, closes) = asked_on_the_builder_s_thread(|db| {
        let code = |refused: crate::Error| refused.code;
        (db.close_within(Some(Duration::from_millis(50))).map_err(code), db.close().map_err(code))
    });
    assert_eq!(closes, (Err(crate::ErrorCode::CallbackActive), Err(crate::ErrorCode::CallbackActive)));
    // And on any other thread it closes
    db.wait_for_index_builds().unwrap();
    db.close().unwrap();
}

/// A schema change by the listener of the builder's commits leaves another builder the database's, or none. A
/// close the same listener then asks for still waits for the store its own thread holds, and is refused as it is
/// before the change
#[test]
fn a_close_after_a_schema_change_on_the_builder_s_thread_is_refused() {
    use std::time::Duration;
    let changed = ranked_schema(1e-8);
    let (_dir, db, (schema, close)) = asked_on_the_builder_s_thread(move |db| {
        let schema = db.set_schema(changed.clone()).map(|changes| changes.len());
        (schema, db.close_within(Some(Duration::from_millis(50))).map_err(|refused| refused.code))
    });
    assert_eq!(schema.unwrap(), 1, "the tolerance changed");
    assert_eq!(close, Err(crate::ErrorCode::CallbackActive));
    db.wait_for_index_builds().unwrap();
    db.close().unwrap();
}

/// The builds go on once the listener of the builder's commits returns, so a wait for them that it asks for would
/// wait for itself: it is refused, and on any other thread it waits
#[test]
fn a_wait_for_the_builds_on_the_builder_s_thread_is_refused() {
    let (_dir, db, waited) =
        asked_on_the_builder_s_thread(|db| db.wait_for_index_builds().map_err(|refused| refused.code));
    assert_eq!(waited, Err(crate::ErrorCode::CallbackActive));
    db.wait_for_index_builds().unwrap();
    db.close().unwrap();
}

/// The listener of the builder's commits is let go of on the builder's thread, when it was taken away as it ran.
/// What it held may be the last of the database, whose close, there, waits for the store that thread holds: it is
/// refused as any close the listener asks for is, and the environment is let go of as the thread ends
#[test]
fn the_last_of_a_database_let_go_of_with_the_builder_s_listener_waits_for_nothing() {
    use std::time::{Duration, Instant};
    let (dir, db, schema, top) = ranked_tasks();
    db.read(&reads(vec![top], Some(7))).unwrap();
    let (started, starts) = std::sync::mpsc::channel();
    let started = Mutex::new(Some(started));
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = Mutex::new(released);
    let held = Arc::clone(&db);
    db.hear_builder_commits(Some(Arc::new(move |_, _| {
        let Some(started) = started.lock().unwrap().take() else { return };
        started.send(()).unwrap();
        released.lock().unwrap().recv().unwrap();
        // Taken away as it runs: this call's hold on it is then the last, and its own on the database with it
        held.hear_builder_commits(None).unwrap();
    })))
    .unwrap();
    link_all(&db);
    starts.recv_timeout(Duration::from_secs(4)).expect("the builder's commit told its listener");
    drop(db);
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(4);
    let again = loop {
        let options = crate::OpenOptions { schema: Some(schema(1e-9)), syncs: false, ..Default::default() };
        match Database::open(dir.path(), options) {
            Ok(db) => break db,
            Err(held) if held.code == crate::ErrorCode::AlreadyOpen && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(refused) => panic!("the close is waiting for its own thread, which holds the environment: {refused}"),
        }
    };
    again.close().unwrap();
}

/// A listener of the builder's commits that panics is heard no more, as a watch whose listener panics is ended. The
/// builder goes on to finish its builds, where the panic would have ended its thread with builds nothing waits
/// out, and it is stopped and waited for as ever
#[test]
fn a_listener_of_the_builder_s_commits_that_panics_is_heard_no_more() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    let (_dir, db, schema, top) = ranked_tasks();
    db.read(&reads(vec![top], Some(7))).unwrap();
    let heard = Arc::new(AtomicUsize::new(0));
    let hears = Arc::clone(&heard);
    db.hear_builder_commits(Some(Arc::new(move |_, _| {
        hears.fetch_add(1, Ordering::SeqCst);
        panic!("the listener's own");
    })))
    .unwrap();
    link_all(&db);
    let (tell, told) = std::sync::mpsc::channel();
    let waiting = Arc::clone(&db);
    std::thread::spawn(move || tell.send(waiting.wait_for_index_builds()).unwrap());
    told.recv_timeout(Duration::from_secs(4)).expect("the builder went on past its listener's panic").unwrap();
    assert_eq!(heard.load(Ordering::SeqCst), 1);
    // A schema change waits for the builder's thread, which ended without the panic, and the next builder's commits
    // tell no one
    assert_eq!(db.set_schema(schema(1e-8)).unwrap().len(), 1, "the tolerance changed");
    db.wait_for_index_builds().unwrap();
    assert_eq!(heard.load(Ordering::SeqCst), 1);
    // Taken away, so another may be given, which a database with one refuses
    db.hear_builder_commits(Some(Arc::new(|_, _| {}))).unwrap();
    db.close().unwrap();
}
