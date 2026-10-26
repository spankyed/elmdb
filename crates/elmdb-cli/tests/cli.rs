//! The `elmdb` command, run as a user runs it: on a database the library wrote

use std::process::Command;

use elmdb::{Batch, Database, OpenOptions};
use serde_json::json;

fn elmdb(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_elmdb")).args(args).output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr),
    )
}

#[test]
fn inspects_checks_backs_up_dumps_and_loads_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let schema = elmdb::schema::Schema::from_json(&json!({
        "$schema": elmdb::schema::SCHEMA_LANGUAGE,
        "types": { "Note": { "fields": { "title": { "type": "string" } } } }
    }))
    .unwrap();
    {
        let db = Database::open(&path, OpenOptions { schema: Some(schema), ..Default::default() }).unwrap();
        let batch: Batch = serde_json::from_value(
            json!({ "ops": [{ "op": "create", "type": "Note", "id": "Note-1", "fields": { "title": "t" } }] }),
        )
        .unwrap();
        db.write(&batch, true).unwrap();
        db.bulk_load(&[elmdb::engine::maintenance::Load::Entity {
            id: "Note-2".into(),
            fields: Default::default(),
            trashed: false,
        }])
        .unwrap();
        db.close().unwrap();
    }
    let p = path.to_str().unwrap();
    let (code, out) = elmdb(&["inspect", p]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("\"Note\""), "{out}");
    let (code, out) = elmdb(&["check", p]);
    assert_eq!(code, 1, "a row lacking a required field is reported: {out}");
    assert!(out.contains("lacks title"), "{out}");
    let target = dir.path().join("copy");
    let (code, out) = elmdb(&["backup", p, target.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    assert!(target.join("data.mdb").is_file());
    let dump = dir.path().join("db.dump");
    let (code, out) = elmdb(&["dump", p, dump.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("\"entities\": 2"), "{out}");
    let loaded = dir.path().join("loaded");
    let (code, out) = elmdb(&["load", dump.to_str().unwrap(), loaded.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = elmdb(&["check", loaded.to_str().unwrap()]);
    assert_eq!(code, 1, "the load holds the row lacking its field too: {out}");
    assert!(out.contains("lacks title"), "{out}");
    let (code, out) = elmdb(&["check", dir.path().join("nothing").to_str().unwrap()]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("NO_DATABASE"), "{out}");
}
