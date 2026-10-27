//! A commit's visibility, in-memory declarations and watch delivery are independent of whether its sync succeeded.
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::{batch, open, schema, write};
use crate::engine::plan::{ReadOp, Reads};
use crate::{Database, ErrorCode, OpenOptions};

pub(super) struct FailedSync(std::path::PathBuf);
impl FailedSync {
    pub(super) fn arm(db: &Database) -> Self {
        let path = db.path().to_owned();
        crate::store::FAIL_SYNC_AT.lock().unwrap().push(path.clone());
        Self(path)
    }
}
impl Drop for FailedSync {
    fn drop(&mut self) {
        crate::store::FAIL_SYNC_AT.lock().unwrap().retain(|path| path != &self.0);
    }
}

#[test]
fn committed_operations_notify_before_reporting_a_failed_sync() {
    for operation in ["batch", "unit", "reset", "replace", "load", "rename", "rebuild", "schema"] {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let source = open(&source_dir);
        write(&source, vec![json!({"op":"create","type":"Task","id":"Task-1","fields":{"a":9}})]).unwrap();
        source.close().unwrap();
        let db = open(&dir);
        write(&db, vec![json!({"op":"create","type":"Task","id":"Task-1","fields":{"a":1}})]).unwrap();
        if operation == "rebuild" {
            let seq = db.snapshot(|s| super::entities::record(s, "Task-1").map(|r| r.unwrap().seq)).unwrap();
            let key = crate::engine::keys::index_entry("Task", "a", &[crate::format::key::Part::Number(1.0)], seq);
            db.store()
                .unwrap()
                .write(
                    |txn, db| {
                        db.delete(txn, &key)?;
                        Ok(())
                    },
                    false,
                )
                .unwrap();
        }
        db.close().unwrap();
        let db = Database::open(dir.path(), OpenOptions::default()).unwrap();
        let read = if operation == "schema" {
            json!({"read":"query","plan":{"start":{"from":"types","types":["Task"]},"terminal":{"answer":"ids"}}})
        } else if operation == "rebuild" {
            json!({"read":"query","plan":{"start":{"from":"types","types":["Task"]},"steps":[{"step":"where","field":"a","value":1}],"terminal":{"answer":"ids"}}})
        } else {
            json!({"read":"row","id":"Task-1"})
        };
        let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(read).unwrap()], track: None };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let into = seen.clone();
        db.watch(reads.clone(), Box::new(move |answer| into.lock().unwrap().push(answer.clone()))).unwrap();
        let before = db.sequence().unwrap();
        let _failure = FailedSync::arm(&db);
        let mut changed_schema = schema();
        changed_schema.types.remove("Task");
        // The direct Rust schema must still satisfy the declarations retained in this example.
        changed_schema.relations.remove("pins");
        let result = match operation {
            "batch" => db.write(&batch(vec![json!({"op":"set","id":"Task-1","fields":{"a":2}})]), true).map(|_| ()),
            "unit" => {
                let mut unit = db.begin().unwrap();
                unit.write(&batch(vec![json!({"op":"set","id":"Task-1","fields":{"a":2}})])).unwrap();
                unit.commit(true).map(|_| ())
            }
            "reset" => db.reset(),
            "replace" => db.replace_with(source_dir.path()),
            "load" => db
                .bulk_load(&[serde_json::from_value(json!({"load":"entity","id":"Task-1","fields":{"a":3}})).unwrap()]),
            "rename" => db.rename_field("Task", "a", "b"),
            "rebuild" => db.rebuild(Some(("Task", "a"))),
            "schema" => db.set_schema(changed_schema.clone()).map(|_| ()),
            _ => unreachable!(),
        };
        assert_eq!(result.unwrap_err().code, ErrorCode::NotDurable, "{operation}");
        assert!(db.sequence().unwrap() > before, "{operation}");
        let answers = seen.lock().unwrap();
        assert_eq!(answers.len(), 2, "{operation} notified before returning its error");
        assert_eq!(answers[1], db.read(&reads), "{operation} delivered committed state");
        drop(answers);
        if operation == "schema" {
            assert_eq!(db.schema(), Some(changed_schema));
        }
        assert!(db.check().unwrap().is_clean(), "{operation} left consistent indexes");
        let committed_schema = db.schema();
        assert!(db.close().is_err(), "the failed sync stays failed until reopen");
        let reopened = Database::open(dir.path(), OpenOptions { syncs: false, ..Default::default() }).unwrap();
        assert_eq!(reopened.schema(), committed_schema, "{operation} schema agrees after reopen");
    }
}

#[test]
fn a_failed_commit_keeps_schema_and_data_and_notifies_nobody() {
    for operation in ["batch", "unit", "reset", "schema", "replace", "load", "rename", "rebuild"] {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let source = open(&source_dir);
        write(&source, vec![json!({"op":"create","type":"Task","id":"Task-1","fields":{"a":9}})]).unwrap();
        source.close().unwrap();
        let db = open(&dir);
        write(&db, vec![json!({"op":"create","type":"Task","id":"Task-1","fields":{"a":1}})]).unwrap();
        let reads = Reads { reads: vec![ReadOp::Row { id: "Task-1".into() }], track: None };
        let seen = Arc::new(Mutex::new(0));
        let into = seen.clone();
        db.watch(reads.clone(), Box::new(move |_| *into.lock().unwrap() += 1)).unwrap();
        let before = db.read(&reads).unwrap();
        let sequence = db.sequence().unwrap();
        let held_schema = db.schema();
        crate::store::FAIL_NEXT_COMMIT.with(|flag| flag.set(true));
        let result = match operation {
            "batch" => db.write(&batch(vec![json!({"op":"set","id":"Task-1","fields":{"a":2}})]), true).map(|_| ()),
            "unit" => {
                let mut unit = db.begin().unwrap();
                unit.write(&batch(vec![json!({"op":"set","id":"Task-1","fields":{"a":2}})])).unwrap();
                unit.commit(true).map(|_| ())
            }
            "reset" => db.reset(),
            "replace" => db.replace_with(source_dir.path()),
            "load" => db
                .bulk_load(&[serde_json::from_value(json!({"load":"entity","id":"Task-1","fields":{"a":3}})).unwrap()]),
            "rename" => db.rename_field("Task", "a", "b"),
            "rebuild" => db.rebuild(Some(("Task", "a"))),
            "schema" => {
                let mut changed = schema();
                changed.types.get_mut("Task").unwrap().fields.remove("a");
                db.set_schema(changed).map(|_| ())
            }
            _ => unreachable!(),
        };
        assert_eq!(result.unwrap_err().code, ErrorCode::Io, "{operation}");
        assert_eq!(db.sequence().unwrap(), sequence, "{operation}");
        assert_eq!(db.read(&reads).unwrap(), before, "{operation}");
        assert_eq!(db.schema(), held_schema, "{operation}");
        assert_eq!(*seen.lock().unwrap(), 1, "{operation}");
    }
}

#[test]
fn reset_checkpoint_preserves_one_logical_commit_and_falls_back_on_failure() {
    for fail_checkpoint in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path(), OpenOptions { schema: Some(schema()), ..Default::default() }).unwrap();
        write(&db, vec![json!({"op":"create","type":"Task","id":"Task-1","fields":{"a":1}})]).unwrap();
        let seen = Arc::new(Mutex::new(0));
        let into = seen.clone();
        db.watch(
            Reads { reads: vec![ReadOp::Row { id: "Task-1".into() }], track: None },
            Box::new(move |_| *into.lock().unwrap() += 1),
        )
        .unwrap();
        let before = db.sequence().unwrap();
        crate::store::FAIL_RESET_CHECKPOINT.with(|flag| flag.set(fail_checkpoint));
        db.reset().unwrap();
        assert!(!crate::store::FAIL_RESET_CHECKPOINT.with(|flag| flag.get()));
        assert_eq!(db.sequence().unwrap(), before + 1);
        assert_eq!(*seen.lock().unwrap(), 2);
        assert!(db.check().unwrap().is_clean());
        db.close().unwrap();
        let reopened = Database::open(dir.path(), OpenOptions::default()).unwrap();
        assert_eq!(reopened.sequence().unwrap(), before + 1);
        assert_eq!(reopened.stats().unwrap().types.get("Task").map(|s| s.live).unwrap_or(0), 0);
    }
}

#[test]
fn repeated_reset_and_load_reuse_the_freed_database_pages() {
    let dir = tempfile::tempdir().unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { syncs: false, schema: Some(schema()), ..Default::default() }).unwrap();
    let loads: Vec<crate::engine::maintenance::Load> = (0..2000)
        .map(|i| {
            serde_json::from_value(
                json!({"load":"entity","id":format!("Task-{i}"),"fields":{"a":i,"title":"x".repeat(1024)}}),
            )
            .unwrap()
        })
        .collect();
    db.bulk_load(&loads).unwrap();
    let first = db.store().unwrap().file_size();
    for _ in 0..4 {
        db.reset().unwrap();
        db.bulk_load(&loads).unwrap();
    }
    let after = db.store().unwrap().file_size();
    assert!(after < first * 3 / 2 + 1024 * 1024, "reset/load grew from {first} to {after} bytes");
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn reset_checkpoint_respects_an_active_read_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![json!({"op":"create","type":"Task","id":"Task-1","fields":{"a":1}})]).unwrap();
    db.snapshot(|snapshot| {
        let held = super::entities::record(snapshot, "Task-1")?;
        db.reset()?;
        assert_eq!(super::entities::record(snapshot, "Task-1")?, held);
        Ok(())
    })
    .unwrap();
    assert!(db.snapshot(|s| super::entities::record(s, "Task-1")).unwrap().is_none());
}

#[test]
fn unchanged_schema_marker_commits_finalize_before_failed_sync() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), OpenOptions { schema: Some(schema()), ..Default::default() }).unwrap();
    db.store()
        .unwrap()
        .write(
            |txn, main| {
                main.delete(txn, &crate::engine::keys::index_built("Task", "a"))?;
                Ok(())
            },
            false,
        )
        .unwrap();
    let reads: Reads = serde_json::from_value(json!({"reads":[{"read":"query","plan":{"start":{"from":"types","types":["Task"]},"terminal":{"answer":"ids"}}}]})).unwrap();
    db.watch(reads, Box::new(|_| {})).unwrap();
    let before = db.sequence().unwrap();
    let _failure = FailedSync::arm(&db);
    crate::engine::watch::PANIC_NEXT_READ.set(true);
    assert_eq!(db.set_schema(schema()).unwrap_err().code, ErrorCode::NotDurable);
    assert_eq!(db.sequence().unwrap(), before + 1);
    assert_eq!(
        db.track_stats().0,
        0,
        "committed marker repair delivered the watch, whose injected read panic removes it"
    );
    assert!(!crate::engine::watch::PANIC_NEXT_READ.replace(false));
    assert!(db.close().is_err());
}
