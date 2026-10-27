//! Cross-process declarations cannot be paired with an earlier handle's cached schema.

use super::{batch, write};
use crate::schema::Schema;
use crate::{Database, ErrorCode, OpenOptions};
use serde_json::json;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn schema(field: &str) -> Schema {
    Schema::from_json(&json!({"$schema":crate::schema::SCHEMA_LANGUAGE, "types":{"Task":{
        "fields":{"a":{"type":"string"},"b":{"type":"string"}},
        "indexes":{"lookup":{"kind":"value","fields":[field]}}
    }}}))
    .unwrap()
}

fn query() -> crate::engine::plan::Reads {
    serde_json::from_value(json!({"reads":[{"read":"query", "plan":{
        "start":{"from":"types","types":["Task"]},
        "steps":[{"step":"where","field":"a","value":"match"}],"terminal":{"answer":"ids"}
    }}]}))
    .unwrap()
}

fn change_in_child(path: &Path) {
    run_child(path, "schema");
}

fn test_command() -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "engine::tests::schema_versions::cross_process_schema_changes_refuse_stale_reads_and_mutations",
            "--nocapture",
        ])
        .stdin(Stdio::null());
    command
}

fn run_child(path: &Path, mode: &str) {
    let mut command = test_command();
    command.env("ELMDB_SCHEMA_CHILD", path).env("ELMDB_SCHEMA_CHILD_MODE", mode);
    run_process(command, mode, Duration::from_secs(15));
}

fn run_process(mut command: Command, description: &str, timeout: Duration) {
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{description} exited {status}");
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("{description} timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn cross_process_schema_changes_refuse_stale_reads_and_mutations() {
    if let Some(path) = std::env::var_os("ELMDB_SCHEMA_CHILD") {
        let db = Database::open(path, OpenOptions { syncs: false, ..Default::default() }).unwrap();
        if std::env::var("ELMDB_SCHEMA_CHILD_MODE").unwrap_or_default() == "insert" {
            write(&db, vec![json!({"op":"create","type":"Task","id":"Task-0","fields":{"a":"match","b":"other"}})])
                .unwrap();
        } else {
            db.set_schema(schema("b")).unwrap();
            db.wait_for_index_builds().unwrap();
        }
        db.close().unwrap();
        return;
    }
    if let Ok(mode) = std::env::var("ELMDB_SCHEMA_PROBE") {
        stale_handle_probe(mode == "read-only");
    } else {
        for mode in ["writable", "read-only"] {
            let mut command = test_command();
            command.env("ELMDB_SCHEMA_PROBE", mode);
            run_process(command, mode, Duration::from_secs(30));
        }
    }
}

fn stale_handle_probe(read_only: bool) {
    let dir = tempfile::tempdir().unwrap();
    let mut db =
        Database::open(dir.path(), OpenOptions { schema: Some(schema("a")), syncs: false, ..Default::default() })
            .unwrap();
    write(
        &db,
        vec![
            json!({"op":"create","type":"Task","id":"Task-0","fields":{"a":"match","b":"other"}}),
            json!({"op":"create","type":"Task","id":"Task-1","fields":{"a":"other","b":"match"}}),
        ],
    )
    .unwrap();
    if read_only {
        db.close().unwrap();
        db = Database::open(dir.path(), OpenOptions { read_only: true, ..Default::default() }).unwrap();
    }
    change_in_child(dir.path());
    assert_eq!(db.read(&query()).unwrap_err().code, ErrorCode::SchemaChanged);
    let mut tracked = query();
    tracked.track = Some(1);
    assert_eq!(db.read(&tracked).unwrap_err().code, ErrorCode::SchemaChanged);
    db.untrack(1);
    assert_eq!(db.check().unwrap_err().code, ErrorCode::SchemaChanged);
    assert_eq!(db.stats().unwrap_err().code, ErrorCode::SchemaChanged);
    assert_eq!(db.index_status().unwrap_err().code, ErrorCode::SchemaChanged);
    assert_eq!(db.primary_loads(|_| panic!("stale export must not run")).unwrap_err().code, ErrorCode::SchemaChanged);
    let dump = dir.path().join("stale.dump");
    assert_eq!(crate::dump::dump(&db, &dump).unwrap_err().code, ErrorCode::SchemaChanged);
    assert!(!dump.exists());
    let heard = std::sync::Arc::new(std::sync::Mutex::new(None));
    let into = heard.clone();
    let watch = db
        .watch(query(), Box::new(move |answer| *into.lock().unwrap() = answer.as_ref().err().map(|e| e.code)))
        .unwrap();
    assert_eq!(*heard.lock().unwrap(), Some(ErrorCode::SchemaChanged));
    db.unwatch(watch);
    if !read_only {
        assert_eq!(db.write(&batch(vec![]), false).unwrap_err().code, ErrorCode::SchemaChanged);
        assert_eq!(db.bulk_load(&[]).unwrap_err().code, ErrorCode::SchemaChanged);
        assert_eq!(db.reset().unwrap_err().code, ErrorCode::SchemaChanged);
        assert_eq!(db.rebuild(None).unwrap_err().code, ErrorCode::SchemaChanged);
        assert_eq!(db.begin().err().unwrap().code, ErrorCode::SchemaChanged);
        assert_eq!(db.begin().err().unwrap().code, ErrorCode::SchemaChanged, "failed begin releases unit state");
    }
    db.close().unwrap();
    let db = Database::open(dir.path(), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    let answer: serde_json::Value = rmp_serde::from_slice(&db.read(&query()).unwrap()).unwrap();
    assert_eq!(answer[0]["ids"], json!(["Task-0"]));
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_schema_change_refuses_if_another_process_changes_its_input_before_the_write() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), OpenOptions { schema: Some(schema("a")), syncs: false, ..Default::default() })
        .unwrap();
    write(&db, vec![json!({"op":"create","type":"Task","id":"Task-0","fields":{"a":"match","b":"other"}})]).unwrap();
    let mut proposed = serde_json::to_value(schema("a")).unwrap();
    proposed["types"]["Task"]["fields"]["newField"] = json!({"type":"any", "optional":true});
    let proposed = Schema::from_json(&proposed).unwrap();
    let path = dir.path().to_owned();
    crate::database::BEFORE_SCHEMA_WRITE.with_borrow_mut(|hook| *hook = Some(Box::new(move || change_in_child(&path))));
    assert_eq!(db.set_schema(proposed).unwrap_err().code, ErrorCode::SchemaChanged);
    db.close().unwrap();
    let db = Database::open(dir.path(), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    assert_eq!(db.schema(), Some(schema("b")), "the other process's declaration survived the stale migration");
    assert!(db.check().unwrap().is_clean());
}

#[test]
fn a_schema_change_rechecks_primary_inventory_in_its_write_transaction() {
    let with_code = |prefix: &str| {
        let mut value = serde_json::to_value(schema("a")).unwrap();
        value["types"]["Task"]["shortCode"] = json!({"prefix":prefix});
        Schema::from_json(&value).unwrap()
    };
    let dir = tempfile::tempdir().unwrap();
    let db =
        Database::open(dir.path(), OpenOptions { schema: Some(with_code("OLD")), syncs: false, ..Default::default() })
            .unwrap();
    let path = dir.path().to_owned();
    crate::database::BEFORE_SCHEMA_WRITE
        .with_borrow_mut(|hook| *hook = Some(Box::new(move || run_child(&path, "insert"))));
    let mut proposed = serde_json::to_value(with_code("OLD")).unwrap();
    let task = proposed["types"].as_object_mut().unwrap().remove("Task").unwrap();
    proposed["types"]["Note"] = task;
    let proposed = Schema::from_json(&proposed).unwrap();
    assert_eq!(db.set_schema(proposed).unwrap_err().code, ErrorCode::SchemaChangeRefused);
    assert_eq!(db.schema(), Some(with_code("OLD")));
    assert!(db.check().unwrap().is_clean());
    assert!(super::row(&db, "Task-0").is_some());
}

#[test]
fn a_builder_with_an_earlier_declaration_refuses_before_writing_index_entries() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), OpenOptions { schema: Some(schema("a")), syncs: false, ..Default::default() })
        .unwrap();
    write(
        &db,
        vec![
            json!({"op":"create","type":"Task","id":"Task-0","fields":{"a":"match","b":"other"}}),
            json!({"op":"create","type":"Task","id":"Task-1","fields":{"a":"other","b":"match"}}),
        ],
    )
    .unwrap();
    change_in_child(dir.path());
    let mut builder = crate::engine::Builder::start(db.store().unwrap(), db.engine_schema());
    builder.queue([("Task".into(), "lookup".into())]);
    builder.waiter().wait();
    let failures = builder.failures();
    builder.stop();
    drop(builder);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].message.contains("schema") && failures[0].message.contains("reopen"));
    db.close().unwrap();
    let db = Database::open(dir.path(), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    assert!(
        db.check().unwrap().is_clean(),
        "the earlier builder did not mix old index entries into the new declaration"
    );
}

#[test]
fn a_stale_handle_cannot_overwrite_another_process_schema() {
    stale_setter_refuses("a");
}

#[test]
fn a_stale_handle_cannot_adopt_another_process_schema() {
    stale_setter_refuses("b");
}

fn stale_setter_refuses(field: &str) {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), OpenOptions { schema: Some(schema("a")), syncs: false, ..Default::default() })
        .unwrap();
    write(&db, vec![json!({"op":"create","type":"Task","id":"Task-0","fields":{"a":"match","b":"other"}})]).unwrap();
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let into = seen.clone();
    db.watch(
        query(),
        Box::new(move |_| {
            into.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }),
    )
    .unwrap();
    change_in_child(dir.path());
    let sequence = db.sequence().unwrap();
    assert_eq!(db.set_schema(schema(field)).unwrap_err().code, ErrorCode::SchemaChanged, "requested {field}");
    assert_eq!(db.schema(), Some(schema("a")));
    assert_eq!(db.sequence().unwrap(), sequence);
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), 1);
    db.close().unwrap();
    let reopened = Database::open(dir.path(), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    assert_eq!(reopened.schema(), Some(schema("b")));
    assert!(reopened.check().unwrap().is_clean());
}

#[test]
fn a_failed_schema_commit_keeps_its_error_when_recovery_sees_a_new_declaration() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), OpenOptions { schema: Some(schema("a")), syncs: false, ..Default::default() })
        .unwrap();
    write(&db, vec![json!({"op":"create","type":"Task","id":"Task-0","fields":{"a":"match","b":"other"}})]).unwrap();
    let path = dir.path().to_owned();
    crate::database::BEFORE_SCHEMA_RECOVERY
        .with_borrow_mut(|hook| *hook = Some(Box::new(move || change_in_child(&path))));
    crate::store::FAIL_NEXT_COMMIT.set(true);
    let error = db.set_schema(schema("b")).unwrap_err();
    assert_eq!(error.code, ErrorCode::Io);
    assert!(error.message.contains("refreshing index state also failed"));
    db.close().unwrap();
    let reopened = Database::open(dir.path(), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    assert_eq!(reopened.schema(), Some(schema("b")));
    assert!(reopened.check().unwrap().is_clean());
}

#[test]
fn interrupted_index_work_resumes_when_schema_change_and_refresh_both_fail() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), OpenOptions { schema: Some(schema("a")), syncs: false, ..Default::default() })
        .unwrap();
    write(&db, vec![json!({"op":"create","type":"Task","id":"Task-0","fields":{"a":"match","b":"other"}})]).unwrap();
    db.wait_for_index_builds().unwrap();
    let store = db.store().unwrap();
    store
        .write(
            |txn, raw| {
                raw.delete(txn, &crate::engine::keys::index_built("Task", "lookup"))?;
                Ok(())
            },
            false,
        )
        .unwrap();
    // Deterministically represent a stopped worker with pending work, avoiding a scheduler-dependent race with a
    // fast single-row build. set_schema must retain this queue even when its recovery inventory cannot be read.
    let mut builder = crate::engine::Builder::start(store.clone(), db.engine_schema());
    builder.stop();
    builder.queue([("Task".into(), "lookup".into())]);
    *db.builder() = Some(builder);
    crate::database::BEFORE_SCHEMA_RECOVERY.with_borrow_mut(|hook| {
        *hook = Some(Box::new(|| {
            crate::engine::FAIL_NEXT_INDEX_REFRESH.set(true);
        }))
    });
    crate::store::FAIL_NEXT_COMMIT.set(true);
    let error = db.set_schema(schema("b")).unwrap_err();
    assert_eq!(error.code, ErrorCode::Io);
    assert!(error.message.contains("refreshing index state also failed"));
    db.wait_for_index_builds().unwrap();
    assert!(db.index_status().unwrap().iter().all(|index| index.built));
    assert_eq!(db.schema(), Some(schema("a")));
    assert!(db.check().unwrap().is_clean());
    drop(store);
    db.close().unwrap();
}
