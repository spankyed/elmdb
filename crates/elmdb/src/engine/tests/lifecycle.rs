//! A database's lifecycle around an open unit and a growing map: a close refuses while a unit holds the write
//! transaction, a unit that filled the map says so and goes on refusing, an aborted unit lets the map grow, and the
//! commit sequence follows what the database holds.

use std::sync::mpsc;
use std::time::Duration;

use serde_json::json;

use super::{batch, open, schema, write};
use crate::error::ErrorCode;
use crate::{Database, OpenOptions};

/// A database whose map is 1 MiB, so a few MiB of writes fill it
fn small(dir: &tempfile::TempDir) -> Database {
    Database::open(
        dir.path(),
        OpenOptions { schema: Some(schema()), syncs: false, map_size_bytes: Some(1 << 20), ..Default::default() },
    )
    .unwrap()
}

/// Runs `f` on a thread of its own, failing the test rather than hanging when it doesn't finish within ten seconds
fn within_ten_seconds<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(f());
    });
    finished.recv_timeout(Duration::from_secs(10)).expect("finished within ten seconds")
}

/// A 4 MiB row in a field no index reads, so the write costs its bytes and nothing else
/// A task holding 4 MiB of text that doesn't compress, so it outgrows a 1 MiB map however its value is stored
fn large_task(id: &str) -> serde_json::Value {
    let mut state = 0x9e37_79b9_u32;
    let text: String = (0..4 << 20)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from(b'!' + (state >> 24) as u8 % 90)
        })
        .collect();
    json!({ "op": "create", "type": "Task", "id": id, "fields": { "c": text } })
}

#[test]
fn a_close_refuses_while_a_unit_is_open_and_closes_once_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open(&dir));
    let mut unit = db.begin().unwrap();
    unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })])).unwrap();
    let closing = std::sync::Arc::clone(&db);
    let refused = within_ten_seconds(move || closing.close());
    assert_eq!(refused.unwrap_err().code, ErrorCode::UnitOpen);
    unit.abort();
    drop(unit);
    db.close().unwrap();
    assert!(!db.is_open());
}

#[test]
fn a_unit_that_fills_the_map_says_so_and_refuses_what_follows() {
    let dir = tempfile::tempdir().unwrap();
    let db = small(&dir);
    let mut unit = db.begin().unwrap();
    let full = unit.write(&batch(vec![large_task("Task-9")])).unwrap_err();
    assert_eq!(full.code, ErrorCode::MapFull, "{}", full.message);
    let after = unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })])).unwrap_err();
    assert_eq!(after.code, ErrorCode::MapFull, "{}", after.message);
    assert!(after.message.contains("must be aborted"), "{}", after.message);
}

#[test]
fn an_aborted_unit_lets_the_map_grow_while_it_is_still_held() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(small(&dir));
    let mut unit = db.begin().unwrap();
    unit.abort();
    let writing = std::sync::Arc::clone(&db);
    // The unit is aborted and not yet dropped: a write that has to grow the map must not wait for the drop
    within_ten_seconds(move || write(&writing, vec![large_task("Task-9")])).unwrap();
    drop(unit);
}

#[test]
fn a_commit_says_when_it_found_another_process_had_committed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let (_, own) = db.write(&batch(vec![]), false).unwrap();
    assert_eq!(own.elsewhere_through, None, "a process's own commits are its own");
    let (_, own) = db.write(&batch(vec![]), false).unwrap();
    assert_eq!(own.elsewhere_through, None);

    // What another process's commits leave: a stored sequence this handle never counted
    let store = db.store().unwrap();
    store
        .write(
            |txn, db| crate::store::put_meta(txn, db.main, crate::format::meta::SEQUENCE, &40u64.to_be_bytes()),
            false,
        )
        .unwrap();
    let (_, found) = db.write(&batch(vec![]), false).unwrap();
    assert_eq!(found.elsewhere_through, Some(40));
    let (_, after) = db.write(&batch(vec![]), false).unwrap();
    assert_eq!(after.elsewhere_through, None, "said once");

    store
        .write(
            |txn, db| crate::store::put_meta(txn, db.main, crate::format::meta::SEQUENCE, &90u64.to_be_bytes()),
            false,
        )
        .unwrap();
    let unit = db.begin().unwrap();
    assert_eq!(unit.commit(false).unwrap().elsewhere_through, Some(90), "a unit's commit says it too");
}

#[test]
fn the_sequence_follows_what_the_database_holds() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let store = db.store().unwrap();
    // What another process's commits leave: a stored sequence this handle never counted
    store
        .write(
            |txn, db| crate::store::put_meta(txn, db.main, crate::format::meta::SEQUENCE, &40u64.to_be_bytes()),
            false,
        )
        .unwrap();
    assert_eq!(db.sequence().unwrap(), 41, "the write that stored 40 committed as 41");
    let (_, changes) = db.write(&batch(vec![]), false).unwrap();
    assert_eq!(changes.sequence, 42);
}

/// What another process leaves: one commit, naming a task, and it has gone
#[test]
#[ignore = "run by a_reader_learns_what_another_process_committed, as a process of its own"]
fn child_commits_a_task() {
    let Ok(path) = std::env::var("ELMDB_LOGGING_DB") else { return };
    let db = Database::open(std::path::Path::new(&path), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    write(&db, vec![large_task("Task-elsewhere")]).unwrap();
    db.close().unwrap();
}

#[test]
fn a_reader_learns_what_another_process_committed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![large_task("Task-mine")]).unwrap();
    let cursor = db.sequence().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "engine::tests::lifecycle::child_commits_a_task",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ELMDB_LOGGING_DB", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the other process failed");

    let changes = db.changes_since(cursor, 100).unwrap();
    assert!(!changes.truncated);
    assert_eq!(changes.through, db.sequence().unwrap());
    let [theirs] = changes.commits.as_slice() else { panic!("one commit, not {:?}", changes.commits) };
    assert_ne!(theirs.origin, db.origin().unwrap(), "named as another's");
    assert!(!theirs.everything);
    assert!(theirs.types.contains("Task"));
    assert_eq!(theirs.entities.as_deref(), Some(&["Task-elsewhere".to_owned()][..]));
    assert_eq!(db.changes_since(changes.through, 100).unwrap().commits, vec![], "nothing since");
}

#[test]
fn the_log_says_what_each_kind_of_commit_changed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let before = db.sequence().unwrap();
    write(&db, vec![large_task("Task-1")]).unwrap();
    let unit = db.begin().unwrap();
    unit.commit(false).unwrap();
    db.store().unwrap().write(|_, _| Ok(()), false).unwrap();
    let changes = db.changes_since(before, 100).unwrap();
    let origin = db.origin().unwrap();
    assert!(changes.commits.iter().all(|commit| commit.origin == origin), "this handle's");
    let [row, empty_unit, unknown] = changes.commits.as_slice() else { panic!("{:?}", changes.commits) };
    assert!(row.types.contains("Task") && !row.everything);
    assert!(empty_unit.types.is_empty() && !empty_unit.everything, "a unit that wrote nothing changed nothing");
    assert!(unknown.everything, "a write that didn't say what it changed");
}

#[test]
fn a_reader_past_the_log_is_told_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let start = db.sequence().unwrap();
    let store = db.store().unwrap();
    // Past the log's reach by more than it trims at a time
    for _ in 0..crate::engine::changelog::LOG_COMMITS + 128 {
        store.write(|_, _| Ok(()), false).unwrap();
    }
    let behind = db.changes_since(start, 10).unwrap();
    assert!(behind.truncated, "the commit after the cursor has gone");
    assert_eq!(behind.through, db.sequence().unwrap(), "a truncated reader starts again from now");

    let kept = db.sequence().unwrap() - crate::engine::changelog::LOG_COMMITS + 1;
    let at_the_edge = db.changes_since(kept - 1, 10).unwrap();
    assert!(!at_the_edge.truncated, "the log holds exactly as many as it says");
    assert_eq!(at_the_edge.commits[0].sequence, kept);
    assert_eq!(at_the_edge.commits.len(), 10);
    assert_eq!(at_the_edge.through, kept + 9, "the limit says how far this answer goes");
    assert!(db.changes_since(db.sequence().unwrap() + 5, 10).unwrap().truncated, "a cursor from the future");
}

#[test]
fn a_unit_sees_its_own_changes_before_they_are_logged() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let cursor = db.sequence().unwrap();
    let mut unit = db.begin().unwrap();
    unit.write(&batch(vec![large_task("Task-pending")])).unwrap();
    let changes = unit.changes_since(cursor, 10).unwrap();
    assert_eq!(changes.commits, vec![], "not until it commits");
    assert!(changes.pending.unwrap().entities.contains_key("Task-pending"));
    unit.commit(false).unwrap();
    assert!(db.changes_since(cursor, 10).unwrap().commits[0].types.contains("Task"));
}

#[test]
fn the_log_stays_within_its_bound() {
    use crate::engine::changelog::{LOG_COMMITS, LOG_TRIM_EVERY};
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let store = db.store().unwrap();
    for _ in 0..LOG_COMMITS + 3 * LOG_TRIM_EVERY {
        store.write(|_, _| Ok(()), false).unwrap();
    }
    let held = db
        .snapshot(|s| {
            let mut held = 0u64;
            crate::engine::unit::Read::visit(s, &crate::engine::keys::change_log_prefix(), |_, _| {
                held += 1;
                true
            })?;
            Ok(held)
        })
        .unwrap();
    assert!(held >= LOG_COMMITS, "it holds what it promises: {held}");
    assert!(held <= LOG_COMMITS + LOG_TRIM_EVERY, "and no more than a trim's worth over: {held}");
}
