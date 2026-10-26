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
