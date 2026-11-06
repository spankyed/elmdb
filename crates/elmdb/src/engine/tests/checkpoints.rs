//! A reader's checkpoints: written in a unit, kept with the rows they describe, and gone with them

use serde_json::json;

use super::{batch, write};
use crate::error::ErrorCode;
use crate::{Database, OpenOptions};

fn open(path: &std::path::Path) -> Database {
    Database::open(path, OpenOptions { schema: Some(super::schema()), syncs: false, ..Default::default() }).unwrap()
}

fn task(id: &str) -> serde_json::Value {
    json!({ "op": "create", "type": "Task", "id": id, "fields": {} })
}

/// A unit that writes a row and the checkpoint saying it was read
fn record(db: &Database, id: &str, place: &[u8]) {
    let mut unit = db.begin().unwrap();
    unit.write(&batch(vec![task(id)])).unwrap();
    unit.set_checkpoint("reader", place).unwrap();
    unit.commit(false).unwrap();
}

#[test]
fn a_checkpoint_commits_with_its_unit_and_reads_back_in_it_and_after() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    assert_eq!(db.checkpoint("reader").unwrap(), None);
    let mut unit = db.begin().unwrap();
    unit.set_checkpoint("reader", b"one").unwrap();
    assert_eq!(unit.checkpoint("reader").unwrap().as_deref(), Some(&b"one"[..]), "in the unit");
    assert_eq!(db.checkpoint("reader").unwrap(), None, "not before it commits");
    unit.commit(false).unwrap();
    assert_eq!(db.checkpoint("reader").unwrap().as_deref(), Some(&b"one"[..]));
    assert_eq!(db.checkpoint("another").unwrap(), None, "each reader's own");
    assert_eq!(db.begin().unwrap().set_checkpoint("", b"x").unwrap_err().code, ErrorCode::BatchInvalid);
}

#[test]
fn a_checkpoint_goes_with_an_aborted_unit_or_a_savepoint_rolled_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let mut unit = db.begin().unwrap();
    unit.set_checkpoint("reader", b"aborted").unwrap();
    unit.abort();
    assert_eq!(db.checkpoint("reader").unwrap(), None);

    let mut unit = db.begin().unwrap();
    unit.set_checkpoint("reader", b"kept").unwrap();
    let depth = unit.savepoint().unwrap();
    unit.set_checkpoint("reader", b"undone").unwrap();
    unit.rollback_to(depth).unwrap();
    unit.commit(false).unwrap();
    assert_eq!(db.checkpoint("reader").unwrap().as_deref(), Some(&b"kept"[..]));
}

#[test]
fn a_checkpoint_survives_a_reopen_and_a_backup_and_is_no_row_a_dump_holds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = open(&path);
    record(&db, "Task-1", b"there");
    db.close().unwrap();
    let db = open(&path);
    assert_eq!(db.checkpoint("reader").unwrap().as_deref(), Some(&b"there"[..]), "reopened");

    let copy = dir.path().join("backup");
    db.backup(&copy).unwrap();
    assert_eq!(
        open(&copy).checkpoint("reader").unwrap().as_deref(),
        Some(&b"there"[..]),
        "the backup's rows and it agree"
    );

    let dumped = |db: &Database| {
        let mut loads = 0;
        db.primary_loads(|_| {
            loads += 1;
            Ok(())
        })
        .unwrap();
        loads
    };
    let before = dumped(&db);
    let mut unit = db.begin().unwrap();
    unit.set_checkpoint("another", b"moved").unwrap();
    unit.commit(false).unwrap();
    assert_eq!(dumped(&db), before, "a dump holds the rows, and no checkpoint");
}

#[test]
fn a_reset_or_a_replacement_drops_the_checkpoints_with_the_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir.path().join("db"));
    record(&db, "Task-1", b"there");
    db.reset().unwrap();
    assert_eq!(db.checkpoint("reader").unwrap(), None, "reset");

    let other = open(&dir.path().join("other"));
    record(&other, "Task-2", b"the other's");
    other.close().unwrap();
    record(&db, "Task-3", b"here");
    db.replace_with(dir.path().join("other")).unwrap();
    assert_eq!(db.checkpoint("reader").unwrap(), None, "a replacement brings rows and no reader's place in them");
    write(&db, vec![task("Task-4")]).unwrap();
}
