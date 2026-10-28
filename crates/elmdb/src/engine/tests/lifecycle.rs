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
