//! Cross-process declarations cannot be paired with an earlier handle's cached schema.

use super::{batch, write};
use crate::engine::watch::Unwatched;
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
    assert_eq!(db.unwatch(watch), Unwatched::Ended);
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
    crate::failpoints::run_once_here(crate::failpoints::points::SCHEMA_WRITE, move || change_in_child(&path));
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
    crate::failpoints::run_once_here(crate::failpoints::points::SCHEMA_WRITE, move || run_child(&path, "insert"));
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
    crate::failpoints::run_once_here(crate::failpoints::points::SCHEMA_RECOVERY, move || change_in_child(&path));
    crate::failpoints::fail_once_here(crate::failpoints::points::COMMIT);
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
    crate::failpoints::run_once_here(crate::failpoints::points::SCHEMA_RECOVERY, || {
        crate::failpoints::fail_once_here(crate::failpoints::points::INDEX_REFRESH)
    });
    crate::failpoints::fail_once_here(crate::failpoints::points::COMMIT);
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

/// A schema stored before its canonical form gained a field: its stored text lacks the field, and its stored
/// fingerprint is that text's. An open that changes nothing validates against that stored fingerprint, so the database
/// reads as it did; only an open that writes a schema stores a fingerprint of today's form
#[test]
fn a_schema_stored_before_its_canonical_form_grew_a_field_still_opens_and_reads() {
    let dir = tempfile::tempdir().unwrap();
    let with_kind = Schema::from_json(&json!({"$schema":crate::schema::SCHEMA_LANGUAGE,
        "relations":{"refs":{"cardinality":"many-to-many"}},
        "types":{"Task":{"fields":{"a":{"type":"string"}}}}}))
    .unwrap();
    let options =
        |schema: Option<Schema>, read_only: bool| OpenOptions { schema, read_only, syncs: false, ..Default::default() };
    {
        let db = Database::open(dir.path(), options(Some(with_kind.clone()), false)).unwrap();
        write(&db, vec![json!({ "op": "create", "type": "Task", "id": "Task-1", "fields": { "a": "match" } })])
            .unwrap();
        // The stored text as an older canonical form wrote it, its stored fingerprint left as it is
        let store = db.store().unwrap();
        store
            .write(
                |txn, d| {
                    let key = crate::store::meta_key(crate::format::meta::SCHEMA);
                    let held = String::from_utf8(d.main.get(txn, &key)?.unwrap().to_vec()).unwrap();
                    let older = held.replace(",\"weighted\":false", "");
                    assert_ne!(older, held, "the older form lacks the field");
                    // Its fingerprint is the one that form had: the SHA-256 of the text as stored
                    use sha2::Digest;
                    let fingerprint: String =
                        sha2::Sha256::digest(older.as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
                    crate::store::put_meta(txn, d.main, crate::format::meta::SCHEMA, older.as_bytes())?;
                    crate::store::put_meta(txn, d.main, crate::format::meta::SCHEMA_HASH, fingerprint.as_bytes())
                },
                false,
            )
            .unwrap();
        drop(store);
        db.close().unwrap();
    }
    let reads = || {
        serde_json::from_value::<crate::engine::plan::Reads>(json!({"reads":[{"read":"query",
            "plan":{"start":{"from":"types","types":["Task"]},"terminal":{"answer":"count"}}}]}))
        .unwrap()
    };
    for (schema, read_only) in [(None, false), (None, true), (Some(with_kind.clone()), true)] {
        let db = Database::open(dir.path(), options(schema.clone(), read_only)).unwrap();
        db.read(&reads()).unwrap_or_else(|e| panic!("given {:?}, read-only {read_only}: {e:?}", schema.is_some()));
        assert!(db.check().unwrap().is_clean());
        db.close().unwrap();
    }
    // A writable open with the schema stores today's form and its fingerprint, and reads as before
    let db = Database::open(dir.path(), options(Some(with_kind), false)).unwrap();
    db.read(&reads()).unwrap();
}
