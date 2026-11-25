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

/// Deletes every type's count (`d/c`): derived data that differs from what primary data derives, as an elmdb whose
/// format changed derived data in place leaves a database an older one built (`docs/architecture/format.md`, Versions)
fn without_counts(path: &std::path::Path) -> usize {
    use heed::types::Bytes;
    // SAFETY: LMDB requires that one process not open an environment twice; the database at `path` is closed, and this
    // is the one environment open on it, dropped before the function returns
    let env = unsafe { heed::EnvOpenOptions::new().max_dbs(8).open(path) }.unwrap();
    let db: heed::Database<Bytes, Bytes> = env
        .open_databases_and_commit(env.read_txn().unwrap(), |opener, rtxn| -> heed::Result<_> {
            Ok(heed::CommitOrAbort::<_, ()>::Commit(opener.open_database(rtxn, Some(elmdb::format::DB_NAME))?))
        })
        .unwrap()
        .unwrap_commit()
        .unwrap();
    let mut wtxn = env.write_txn().unwrap();
    let prefix = elmdb::format::Prefix::Count.key();
    let keys: Vec<Vec<u8>> =
        db.prefix_iter(&wtxn, prefix.as_bytes()).unwrap().map(|entry| entry.unwrap().0.to_vec()).collect();
    for key in &keys {
        db.delete(&mut wtxn, key).unwrap();
    }
    wtxn.commit().unwrap();
    keys.len()
}

#[test]
fn rebuilds_derived_data_that_differs_from_what_primary_data_derives() {
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
        db.close().unwrap();
    }
    assert!(without_counts(&path) > 0, "the database held a count to delete");
    let p = path.to_str().unwrap();
    let (code, out) = elmdb(&["check", p]);
    assert_eq!(code, 1, "the missing count is a difference: {out}");
    assert!(out.contains("\"differences\": [\n    {"), "{out}");

    let (code, out) = elmdb(&["rebuild", p]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("Rebuilt every index of"), "{out}");
    let (code, out) = elmdb(&["check", p]);
    assert_eq!(code, 0, "the rebuilt database checks clean: {out}");

    // It writes, and takes a directory with no database for a mistake rather than making one there
    let nothing = dir.path().join("nothing");
    let (code, out) = elmdb(&["rebuild", nothing.to_str().unwrap()]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("NO_DATABASE"), "{out}");
    assert!(!nothing.exists(), "a database was made where there was none");
}

#[test]
fn backup_replaces_an_earlier_backup_and_refuses_a_file_a_database_opened_for_writing_or_what_no_backup_leaves() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let schema = elmdb::schema::Schema::from_json(&json!({
        "$schema": elmdb::schema::SCHEMA_LANGUAGE,
        "types": { "Note": { "fields": { "title": { "type": "string" } } } }
    }))
    .unwrap();
    let write_note = |path: &std::path::Path, id: &str, title: &str| {
        let db = Database::open(path, OpenOptions { schema: Some(schema.clone()), ..Default::default() }).unwrap();
        let batch: Batch = serde_json::from_value(
            json!({ "ops": [{ "op": "create", "type": "Note", "id": id, "fields": { "title": title } }] }),
        )
        .unwrap();
        db.write(&batch, true).unwrap();
        db.close().unwrap();
    };
    let notes = |path: &std::path::Path| {
        let db = Database::open(path, OpenOptions { read_only: true, ..Default::default() }).unwrap();
        let live = db.stats().unwrap().types["Note"].live;
        db.close().unwrap();
        live
    };
    write_note(&source, "Note-1", "first");
    let src = source.to_str().unwrap();

    // A new directory, then the same one again: an earlier backup, read since and left a copy a stopped run made, is
    // replaced
    let backups = dir.path().join("backups");
    let (code, out) = elmdb(&["backup", src, backups.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = elmdb(&["check", backups.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    std::fs::write(backups.join(".elmdb-backup-stopped"), b"partial").unwrap();
    write_note(&source, "Note-2", "second");
    let (code, out) = elmdb(&["backup", src, backups.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(notes(&backups), 2, "the earlier backup was replaced");

    // An existing empty directory, and one reached through a symbolic link
    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let (code, out) = elmdb(&["backup", src, empty.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    assert!(empty.join("data.mdb").is_file());
    #[cfg(unix)]
    {
        let linked = dir.path().join("linked");
        std::fs::create_dir(&linked).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&linked, &link).unwrap();
        let (code, out) = elmdb(&["backup", src, link.to_str().unwrap()]);
        assert_eq!(code, 0, "{out}");
        assert!(linked.join("data.mdb").is_file());
    }

    // A file is refused without being changed
    let file = dir.path().join("existing-file");
    std::fs::write(&file, b"keep-me").unwrap();
    let (code, out) = elmdb(&["backup", src, file.to_str().unwrap()]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("IO") && out.contains("is a file"), "{out}");
    assert_eq!(std::fs::read(&file).unwrap(), b"keep-me");

    // A database opened for writing is refused without touching `data.mdb` or `synced`: the backup an earlier run
    // made, once opened to write, is one too
    let existing_db = dir.path().join("existing-db");
    write_note(&existing_db, "Note-dst", "destination");
    write_note(&empty, "Note-3", "written in place");
    for target in [&existing_db, &empty] {
        let data_before = std::fs::read(target.join("data.mdb")).unwrap();
        let synced_before = std::fs::read(target.join("synced")).unwrap();
        let (code, out) = elmdb(&["backup", src, target.to_str().unwrap()]);
        assert_eq!(code, 2, "{out}");
        assert!(out.contains("TARGET_IN_USE") && out.contains("opened for writing"), "{out}");
        assert_eq!(std::fs::read(target.join("data.mdb")).unwrap(), data_before);
        assert_eq!(std::fs::read(target.join("synced")).unwrap(), synced_before);
    }

    // A directory holding anything a backup doesn't leave is refused untouched: a project's, named where its backup
    // directory was meant, and another program's LMDB database, whose `data.mdb` a backup would replace
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("package.json"), b"{}").unwrap();
    let other_lmdb = dir.path().join("other-lmdb");
    std::fs::create_dir(&other_lmdb).unwrap();
    std::fs::write(other_lmdb.join("data.mdb"), b"another program's").unwrap();
    std::fs::write(other_lmdb.join("lock.mdb"), b"").unwrap();
    std::fs::write(other_lmdb.join("notes.txt"), b"").unwrap();
    for (target, held) in [(&project, "package.json"), (&other_lmdb, "notes.txt")] {
        let before: Vec<_> = std::fs::read_dir(target).unwrap().map(|e| e.unwrap().file_name()).collect();
        let (code, out) = elmdb(&["backup", src, target.to_str().unwrap()]);
        assert_eq!(code, 2, "{out}");
        assert!(out.contains("TARGET_IN_USE") && out.contains(held) && out.contains("no backup leaves"), "{out}");
        let after: Vec<_> = std::fs::read_dir(target).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(after, before, "nothing made or replaced");
    }
}
