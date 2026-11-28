//! Real Linux read-only bind-mount verification (`scripts/check-read-only-mount.py`).
//!
//! Proves inside a private Linux mount namespace (`sudo -n unshare --mount --propagation private`)
//! that v2 `read_only: true` opens succeed cleanly on an `EROFS` mount without creating or writing
//! `synced.v2.lock` or `lock.mdb`, serve queries & `check(None)` cleanly, reject mutations with
//! `ErrorCode::ReadOnly`, and that writable opens fail closed with `ErrorCode::Io`.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

use elmdb::protocol::Schema;
use elmdb::{CanonicalValue, Database, DurabilityMode, ErrorCode, OpenOptions, WireOp};

fn simple_schema() -> Schema {
    serde_json::from_value(serde_json::json!({
        "types": {
            "Item": {
                "fields": { "value": { "type": "string", "optional": true } }
            }
        }
    }))
    .unwrap()
}

struct ReadOnlyBindMount(PathBuf);

impl Drop for ReadOnlyBindMount {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.0).status();
    }
}

#[test]
#[ignore = "run by scripts/check-read-only-mount.py inside a private Linux mount namespace"]
fn read_only_mount_opens_without_writing_locks_and_blocks_writes() {
    assert_eq!(std::env::var("ELMDB_TEST_READ_ONLY_MOUNT").as_deref(), Ok("1"));
    let dir = tempfile::tempdir().unwrap();
    {
        let db =
            Database::open(dir.path(), OpenOptions { durability: DurabilityMode::Grouped, ..OpenOptions::default() })
                .unwrap();
        db.set_schema(&simple_schema()).unwrap();
        db.execute_batch(&[WireOp::Create {
            type_name: "Item".into(),
            external_id: Some("proof".into()),
            fields: vec![("value".into(), CanonicalValue::String("registered".into()))],
        }])
        .unwrap();
        db.flush().unwrap();
        db.close().unwrap();
    }

    // Remove lock files so the read-only mount contains ONLY `data.mdb` + `synced.v2`.
    let sidecar_lock = dir.path().join("synced.v2.lock");
    let lmdb_lock = dir.path().join("lock.mdb");
    let _ = std::fs::remove_file(&sidecar_lock);
    let _ = std::fs::remove_file(&lmdb_lock);
    assert!(!sidecar_lock.exists());
    assert!(!lmdb_lock.exists());

    assert!(Command::new("mount").arg("--bind").arg(dir.path()).arg(dir.path()).status().unwrap().success());
    let _mount = ReadOnlyBindMount(dir.path().to_path_buf());
    assert!(Command::new("mount").args(["-o", "remount,bind,ro"]).arg(dir.path()).status().unwrap().success());

    // 1. Read-only open MUST succeed on a real EROFS mount without creating lock files.
    let ro_db = Database::open(
        dir.path(),
        OpenOptions { read_only: true, durability: DurabilityMode::Grouped, ..OpenOptions::default() },
    )
    .expect("read_only open must succeed on real EROFS mount");

    assert!(!sidecar_lock.exists(), "read_only open must never create synced.v2.lock");
    assert!(!lmdb_lock.exists(), "read_only open must never create lock.mdb");

    let row = ro_db.get_row("proof").unwrap().expect("proof row must be readable");
    assert_eq!(row.fields.get("value").and_then(|v| v.as_str()), Some("registered"));
    assert!(ro_db.check(None).unwrap().clean);

    // 2. Mutations on a read_only handle must fail closed with `ReadOnly`.
    let err = ro_db
        .execute_batch(&[WireOp::Create {
            type_name: "Item".into(),
            external_id: Some("denied".into()),
            fields: vec![("value".into(), CanonicalValue::String("no".into()))],
        }])
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ReadOnly);
    ro_db.close().unwrap();

    // 3. Writable open on a real EROFS mount must fail closed.
    let rw_err = Database::open(
        dir.path(),
        OpenOptions { read_only: false, durability: DurabilityMode::Grouped, ..OpenOptions::default() },
    )
    .err()
    .expect("writable open must fail on real EROFS mount");
    assert!(matches!(rw_err.code, ErrorCode::Io | ErrorCode::DurabilityFailed), "got: {rw_err:?}");

    println!("ELMDB_READ_ONLY_MOUNT_OK");
}
