//! Reads asked at a call and answered later, on any thread (`Database::read_at_call`; `docs/architecture/engine.md`,
//! Reads): each answers what `read` would have answered at its call, whatever commits and growths of the map come
//! before it runs; a burst holds a few of LMDB's reader slots; a close refuses what still waits; and a tracked one is
//! told of a commit that wrote into what it read before it was filed.

use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};

use super::open;
use crate::engine::CallRead;
use crate::engine::plan::{ReadOp, Reads};
use crate::engine::watch::MOST_NOTED;
use crate::store::call_reads::{HELD, PER_COMMIT};
use crate::{Database, ErrorCode};

/// The Tasks counted
fn count(track: Option<u32>) -> Reads {
    let op = json!({ "read": "query", "plan": { "start": { "from": "types", "types": ["Task"] }, "terminal": { "answer": "count" } } });
    Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track }
}

fn counted(db: &Database, read: CallRead) -> u64 {
    let bytes = db.answer_call_read(read).answer.unwrap();
    let answer: Json = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    answer[0]["n"].as_u64().unwrap()
}

/// Creates `id`, answering the commit's sequence and the tokens its result named
fn create(db: &Database, entity_type: &str, id: &str) -> (u64, Vec<u32>) {
    let (_, changes) =
        db.write(&super::batch(vec![json!({ "op": "create", "type": entity_type, "id": id })]), false).unwrap();
    (changes.sequence, changes.touched)
}

#[test]
fn a_read_answers_what_read_would_have_at_its_call_though_commits_landed_before_it_ran() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    create(&db, "Task", "Task-1");
    let first = db.read_at_call(count(None));
    create(&db, "Task", "Task-2");
    let second = db.read_at_call(count(None));
    create(&db, "Task", "Task-3");
    // Answered on another thread, the later call first
    std::thread::scope(|scope| {
        assert_eq!(scope.spawn(|| counted(&db, second)).join().unwrap(), 2);
        assert_eq!(scope.spawn(|| counted(&db, first)).join().unwrap(), 1);
    });
    assert_eq!(counted(&db, db.read_at_call(count(None))), 3);
}

#[test]
fn a_burst_of_reads_at_one_commit_shares_a_few_snapshots_and_reads_across_commits_hold_no_more_than_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    create(&db, "Task", "Task-0");
    let store = db.store().unwrap();
    let burst: Vec<_> = (0..200).map(|_| db.read_at_call(count(None))).collect();
    assert_eq!(store.held_for_calls(), (PER_COMMIT, PER_COMMIT));
    for read in burst {
        assert_eq!(counted(&db, read), 1);
    }
    assert_eq!(store.held_for_calls(), (0, 0));

    // A read after each commit: one snapshot each, until as many are held as may be, and then a read runs at its call
    let mut asked = Vec::new();
    for n in 2..=HELD as u64 + 8 {
        create(&db, "Task", &format!("Task-{n}"));
        asked.push((n, db.read_at_call(count(None))));
        assert!(store.held_for_calls().0 <= HELD, "{:?} held after {n} commits", store.held_for_calls());
    }
    assert_eq!(store.held_for_calls(), (HELD, 1));
    for (n, read) in asked {
        assert_eq!(counted(&db, read), n, "the read asked after the Task-{n} commit");
    }
    assert_eq!(store.held_for_calls(), (0, 0));
}

#[test]
fn a_growth_of_the_map_runs_the_reads_waiting_on_their_own_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    create(&db, "Task", "Task-1");
    let asked = db.read_at_call(count(None));
    create(&db, "Task", "Task-2");
    let store = db.store().unwrap();
    store.grow().unwrap();
    assert_eq!(store.held_for_calls(), (0, 0), "a transaction outlived the growth");
    create(&db, "Task", "Task-3");
    assert_eq!(counted(&db, asked), 1);
}

#[test]
fn a_close_refuses_the_reads_still_waiting_and_closes_though_their_snapshots_were_held() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(open(&dir));
    create(&db, "Task", "Task-1");
    let asked = db.read_at_call(count(None));
    let (closed, done) = std::sync::mpsc::channel();
    let closing = Arc::clone(&db);
    std::thread::spawn(move || closed.send(closing.close()).unwrap());
    done.recv_timeout(std::time::Duration::from_secs(10)).expect("the close returned").unwrap();
    assert_eq!(db.answer_call_read(asked).answer.unwrap_err().code, ErrorCode::Closed);
}

#[test]
fn a_tracked_read_is_told_of_the_last_commit_that_wrote_into_it_before_it_was_filed_and_of_none_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    create(&db, "Task", "Task-1");
    let asked = db.read_at_call(count(Some(7)));
    // Not filed yet: no commit's result names it
    assert_eq!(create(&db, "Note", "Note-1").1, [0u32; 0]);
    assert_eq!(create(&db, "Task", "Task-2").1, [0u32; 0]);
    let (into, named) = create(&db, "Task", "Task-3");
    assert_eq!(named, [0u32; 0]);
    create(&db, "Note", "Note-2");
    let answered = db.answer_call_read(asked);
    assert_eq!(answered.touched, Some((into, 7)));
    assert_eq!(answered.answer.map(|bytes| bytes.is_empty()), Ok(false));
    // Filed now: the next commit into it names it in its result
    assert_eq!(create(&db, "Task", "Task-4").1, [7]);

    let quiet = db.read_at_call(count(Some(8)));
    create(&db, "Note", "Note-3");
    assert_eq!(db.answer_call_read(quiet).touched, None);
}

#[test]
fn a_read_untracked_while_it_waited_is_neither_filed_nor_told() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let asked = db.read_at_call(count(Some(7)));
    create(&db, "Task", "Task-1");
    db.untrack(7);
    assert_eq!(db.answer_call_read(asked).touched, None);
    assert_eq!(db.track_stats(), (0, 0, 0));
    assert_eq!(create(&db, "Task", "Task-2").1, [0u32; 0]);
}

#[test]
fn what_a_tracked_read_is_told_reaches_the_listener_of_the_builder_s_commits() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    db.tell_touched(3, 7);
    let heard = Arc::new(Mutex::new(Vec::new()));
    let hearing = Arc::clone(&heard);
    db.hear_builder_commits(Some(Arc::new(move |sequence, touched| hearing.lock().unwrap().push((sequence, touched)))))
        .unwrap();
    db.tell_touched(5, 7);
    assert_eq!(*heard.lock().unwrap(), [(5, vec![7])]);
}

#[test]
fn a_tracked_read_waiting_through_more_commits_than_are_noted_is_told_of_the_last_though_none_wrote_into_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let fewer = db.read_at_call(count(Some(7)));
    for n in 0..MOST_NOTED {
        create(&db, "Note", &format!("Note-{n}"));
    }
    assert_eq!(db.answer_call_read(fewer).touched, None, "as many commits as are noted, none into it");
    let more = db.read_at_call(count(Some(8)));
    let mut last = 0;
    for n in 0..=MOST_NOTED {
        last = create(&db, "Note", &format!("Note-more-{n}")).0;
    }
    // Past the bound the commits stand for one that wrote everything: a notice too many, never one missed
    assert_eq!(db.answer_call_read(more).touched, Some((last, 8)));
}
