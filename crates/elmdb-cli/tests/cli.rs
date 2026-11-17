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

/// The record beside a data file, `synced`, as a power loss leaves it to the next boot: each slot that reads written
/// as of no boot, which is another boot than any (`docs/architecture/format.md`, Beside the data file)
fn of_another_boot(record: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    // The magic, the version, the count, the mode, the state, the commit, the meta body and the roots' checksum come
    // before the boot; the notice's byte and u64 after it, and the slot's checksum of all of them last
    const BOOT: usize = 8 + 4 + 8 + 1 + 1 + 8 + 136 + 32;
    const FIELDS: usize = BOOT + 32 + 1 + 8;
    let mut record = record.to_vec();
    for slot in record.chunks_mut(4096) {
        let whole = slot.len() >= FIELDS + 32 && sha2::Sha256::digest(&slot[..FIELDS])[..] == slot[FIELDS..FIELDS + 32];
        if whole {
            slot[BOOT..BOOT + 32].fill(0);
            let checksum = sha2::Sha256::digest(&slot[..FIELDS]);
            slot[FIELDS..FIELDS + 32].copy_from_slice(&checksum);
        }
    }
    record
}

#[test]
fn every_command_says_a_power_loss_restored_the_database_until_it_is_acknowledged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let note = |id: &str| -> Batch {
        serde_json::from_value(json!({ "ops": [{ "op": "create", "type": "Note", "id": id, "fields": {} }] })).unwrap()
    };
    let schema = elmdb::schema::Schema::from_json(&json!({
        "$schema": elmdb::schema::SCHEMA_LANGUAGE,
        "types": { "Note": { "fields": {} } }
    }))
    .unwrap();
    // A commit synced, and one after it that isn't when the machine loses power
    let options =
        OpenOptions { schema: Some(schema), sync_idle_ms: 600_000, sync_interval_ms: 600_000, ..Default::default() };
    let db = Database::open(&path, options).unwrap();
    db.write(&note("Note-1"), true).unwrap();
    let synced = db.sequence().unwrap();
    db.write(&note("Note-2"), false).unwrap();
    let lost = dir.path().join("lost");
    std::fs::create_dir(&lost).unwrap();
    std::fs::copy(path.join("data.mdb"), lost.join("data.mdb")).unwrap();
    std::fs::write(lost.join("synced"), of_another_boot(&std::fs::read(path.join("synced")).unwrap())).unwrap();
    db.close().unwrap();
    let lost = lost.to_str().unwrap();
    // The open that restores it and every open after say so, with the sequence it opened at
    for _ in 0..2 {
        let (code, out) = elmdb(&["inspect", lost]);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("Restored: ") && out.contains(&format!("at sequence {synced};")), "{out}");
    }
    let (code, out) = elmdb(&["acknowledge-restore", lost]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("Acknowledged"), "{out}");
    let (code, out) = elmdb(&["inspect", lost]);
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("Restored"), "said again once acknowledged: {out}");
    let (_, out) = elmdb(&["acknowledge-restore", lost]);
    assert!(out.contains("No restore to acknowledge"), "{out}");
    // It writes, and takes a directory with no database for a mistake rather than making one there
    let nothing = dir.path().join("nothing");
    let (code, out) = elmdb(&["acknowledge-restore", nothing.to_str().unwrap()]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("NO_DATABASE"), "{out}");
    assert!(!nothing.exists(), "a database was made where there was none");
}
