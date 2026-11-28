//! Real OS/LMDB/durability/lifecycle failpoint seam tests (compiled only with `--features failpoints`).
//!
//! Exercises every injected production seam on real `LmdbStore` + `OsVfs` + `Database`:
//! - `LMDB_TXN_COMMIT` (`mdb_txn_commit` failure rolls back cleanly without leaking `MDB_txn` or poisoning)
//! - `LMDB_ENV_SYNC` (`mdb_env_sync` failure permanently poisons `DurabilityGate`)
//! - `LMDB_ENV_SET_MAPSIZE` (`mdb_env_set_mapsize` failure sets `map_broken` and fails closed)
//! - `SIDECAR_WRITE`, `SIDECAR_FSYNC`, `SIDECAR_RENAME` (initial and alternating sidecar failures poison & clean temp files)
//! - `FS_PUBLISH_RENAME` (`dump` and `backup_into` clean up staging artifacts without publishing partial target)
//! - `CLOSE_INNER_BEFORE_DRAIN` (deterministic coordination between `close()` after `handle.take()` and in-flight `enter_op`)
//! - `SKIP_GROUPED_DURABLE_PIN` (negative-control failpoint leaves `DurabilityMode::Grouped` unpinned while still sealing `synced.v2`)

#![cfg(feature = "failpoints")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use elmdb::protocol::Schema;
use elmdb::{BackupInto, CanonicalValue, Database, DurabilityMode, ErrorCode, OpenOptions, WireOp};
use elmdb_storage::failpoints::{self, points};

fn simple_schema() -> Schema {
    serde_json::from_value(serde_json::json!({
        "types": {
            "Note": {
                "fields": { "body": { "type": "string", "optional": true } }
            }
        }
    }))
    .unwrap()
}

fn create_note(db: &Database, id: &str, body: &str) -> elmdb::Result<elmdb::BatchOutcome> {
    db.execute_batch(&[WireOp::Create {
        type_name: "Note".into(),
        external_id: Some(id.into()),
        fields: vec![("body".into(), CanonicalValue::String(body.into()))],
    }])
}

#[test]
fn lmdb_txn_commit_failpoint_aborts_cleanly_and_fails_closed_without_persisting() {
    // 1. Raw LmdbStore layer: injected commit failure aborts MDB_txn cleanly without leaking or wedging.
    {
        use elmdb_storage::{DbFlags, KvStore, KvWrite};

        let dir = tempfile::tempdir().unwrap();
        let store = elmdb_lmdb::LmdbStore::open(dir.path(), elmdb_lmdb::LmdbConfig::default()).unwrap();
        let dbi = {
            let mut tx = store.begin_write(store.lock_map_read()).unwrap();
            let dbi = tx.open_subdb("items", DbFlags::DEFAULT).unwrap();
            tx.put(dbi, b"k1", b"v1").unwrap();
            let _ = tx.commit_raw().unwrap();
            dbi
        };
        failpoints::fail_once_at(points::LMDB_TXN_COMMIT, dir.path());
        {
            let mut tx = store.begin_write(store.lock_map_read()).unwrap();
            tx.put(dbi, b"k2", b"must_not_persist").unwrap();
            let err = tx.commit_raw().err().expect("LMDB_TXN_COMMIT failpoint must fail commit_raw");
            assert!(matches!(err, elmdb_storage::StorageError::Io(_)), "got: {err:?}");
        }
        // Subsequent write on the same store must succeed cleanly (no leaked MDB_txn).
        {
            let mut tx = store.begin_write(store.lock_map_read()).unwrap();
            tx.put(dbi, b"k3", b"v3").unwrap();
            let _ = tx.commit_raw().unwrap();
        }
        let ro = store.begin_read().unwrap();
        use elmdb_storage::KvRead;
        assert_eq!(ro.get(dbi, b"k1").unwrap(), Some(b"v1".as_slice()));
        assert_eq!(ro.get(dbi, b"k2").unwrap(), None);
        assert_eq!(ro.get(dbi, b"k3").unwrap(), Some(b"v3".as_slice()));
    }

    // 2. Database / DurabilityGate layer: uncertain commit I/O error fails closed (`DurabilityFailed` -> `Poisoned`)
    // and reopening recovers cleanly with `n1` intact and `n2` absent.
    {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path(), OpenOptions::default()).unwrap();
        db.set_schema(&simple_schema()).unwrap();
        create_note(&db, "n1", "ok").unwrap();

        failpoints::fail_once_at(points::LMDB_TXN_COMMIT, dir.path());
        let err = create_note(&db, "n2", "must_fail").unwrap_err();
        assert_eq!(err.code, ErrorCode::DurabilityFailed, "got: {err:?}");
        assert_eq!(db.get_row("n1").unwrap_err().code, ErrorCode::Poisoned);
        let _ = db.close();

        let reopened = Database::open(dir.path(), OpenOptions::default()).unwrap();
        assert!(reopened.get_row("n1").unwrap().is_some());
        assert!(reopened.get_row("n2").unwrap().is_none(), "aborted commit must not persist n2");
        create_note(&reopened, "n3", "after_reopen").unwrap();
        assert!(reopened.check(None).unwrap().clean);
        reopened.close().unwrap();
    }
}

#[test]
fn lmdb_env_sync_failpoint_permanently_poisons_gate() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), OpenOptions { durability: DurabilityMode::Grouped, ..OpenOptions::default() })
        .unwrap();
    db.set_schema(&simple_schema()).unwrap();
    create_note(&db, "n1", "hello").unwrap();

    failpoints::fail_once_at(points::LMDB_ENV_SYNC, dir.path());
    let err = db.flush().unwrap_err();
    assert_eq!(err.code, ErrorCode::DurabilityFailed, "got: {err:?}");

    // Gate must now be permanently poisoned for both reads and writes!
    assert_eq!(db.get_row("n1").unwrap_err().code, ErrorCode::Poisoned);
    assert_eq!(create_note(&db, "n2", "blocked").unwrap_err().code, ErrorCode::Poisoned);
    assert_eq!(db.flush().unwrap_err().code, ErrorCode::Poisoned);
    let _ = db.close();
}

#[test]
fn lmdb_env_set_mapsize_failpoint_fails_closed_as_corrupted() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(
        dir.path(),
        OpenOptions { map_size: Some(256 * 1024), durability: DurabilityMode::Grouped, ..OpenOptions::default() },
    )
    .unwrap();
    db.set_schema(&simple_schema()).unwrap();

    failpoints::fail_once_at(points::LMDB_ENV_SET_MAPSIZE, dir.path());
    // Write strictly more than the 256 KiB map of INCOMPRESSIBLE data (so Zstd row compression
    // cannot shrink it) so MapFull -> grow_map -> mdb_env_set_mapsize fires on all page sizes!
    let mut state = 0x9e37_79b9_u32;
    let big: String = (0..300 * 1024)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from(b'!' + (state >> 24) as u8 % 90)
        })
        .collect();
    let err = create_note(&db, "huge", &big).unwrap_err();
    assert_eq!(err.code, ErrorCode::Corrupted, "failed map grow must fail closed as Corrupted: {err:?}");

    // Subsequent reads/writes must stay failed-closed (`Poisoned` / `Corrupted`).
    let read_err = db.get_row("huge").unwrap_err();
    assert!(matches!(read_err.code, ErrorCode::Poisoned | ErrorCode::Corrupted), "got: {read_err:?}");
    let _ = db.close();
}

#[test]
fn sidecar_write_fsync_and_rename_failpoints_poison_and_clean_temp_files() {
    // 1. Initial sidecar creation during bootstrap: SIDECAR_RENAME fails -> temp file removed, synced.v2 unpublished.
    {
        let dir = tempfile::tempdir().unwrap();
        failpoints::fail_once_at(points::SIDECAR_RENAME, dir.path());
        let err =
            Database::open(dir.path(), OpenOptions { durability: DurabilityMode::Strict, ..OpenOptions::default() })
                .err()
                .expect("initial sidecar rename failpoint must fail bootstrap open");
        assert_eq!(err.code, ErrorCode::DurabilityFailed, "got: {err:?}");
        assert!(!dir.path().join("synced.v2").exists(), "failed rename must not publish synced.v2");
        let stray: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".synced.v2.tmp"))
            .collect();
        assert!(stray.is_empty(), "failed rename must remove temp sidecar: {stray:?}");
    }

    // 2. Initial sidecar creation during bootstrap: SIDECAR_FSYNC fails -> temp file removed, synced.v2 unpublished.
    {
        let dir = tempfile::tempdir().unwrap();
        failpoints::fail_once_at(points::SIDECAR_FSYNC, dir.path());
        let err =
            Database::open(dir.path(), OpenOptions { durability: DurabilityMode::Strict, ..OpenOptions::default() })
                .err()
                .expect("initial sidecar fsync failpoint must fail bootstrap open");
        assert_eq!(err.code, ErrorCode::DurabilityFailed, "got: {err:?}");
        assert!(!dir.path().join("synced.v2").exists(), "failed fsync must not publish synced.v2");
        let stray: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".synced.v2.tmp"))
            .collect();
        assert!(stray.is_empty(), "failed fsync must remove temp sidecar: {stray:?}");
    }

    // 3. Alternating slot update: SIDECAR_WRITE & SIDECAR_FSYNC fail -> gate poisoned, prior sidecar stays valid on disk.
    for fp in [points::SIDECAR_WRITE, points::SIDECAR_FSYNC] {
        let dir = tempfile::tempdir().unwrap();
        let db =
            Database::open(dir.path(), OpenOptions { durability: DurabilityMode::Strict, ..OpenOptions::default() })
                .unwrap();
        db.set_schema(&simple_schema()).unwrap();
        create_note(&db, "n1", "persisted").unwrap();

        failpoints::fail_once_at(fp, dir.path());
        let err = create_note(&db, "n2", "fails_sidecar").unwrap_err();
        assert_eq!(err.code, ErrorCode::DurabilityFailed, "fp={fp}, got={err:?}");
        assert_eq!(db.get_row("n1").unwrap_err().code, ErrorCode::Poisoned);
        let _ = db.close();

        // Reopening recovers cleanly from the intact prior sidecar slot!
        let reopened = Database::open(dir.path(), OpenOptions::default()).unwrap();
        assert!(reopened.get_row("n1").unwrap().is_some());
        assert!(reopened.check(None).unwrap().clean);
        reopened.close().unwrap();
    }
}

#[test]
fn fs_publish_failpoint_aborts_dump_and_backup_without_publishing_partial_target() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path(), OpenOptions::default()).unwrap();
    db.set_schema(&simple_schema()).unwrap();
    create_note(&db, "n1", "hello").unwrap();

    // 1. Streaming dump file publication failure leaves no target file behind.
    let dump_path = out_dir.path().join("snapshot.elmdb");
    failpoints::fail_once_at(points::FS_PUBLISH_RENAME, out_dir.path());
    let err = elmdb::dump::dump(&db, &dump_path).unwrap_err();
    assert_eq!(err.code, ErrorCode::Io);
    assert!(!dump_path.exists(), "failed dump publish must not leave target file");

    // 2. Backup directory publication failure leaves no target directory behind.
    let bak_path = out_dir.path().join("backup-dir");
    failpoints::fail_once_at(points::FS_PUBLISH_RENAME, out_dir.path());
    let err = db.backup_into(&bak_path, BackupInto::New).unwrap_err();
    assert_eq!(err.code, ErrorCode::Io);
    assert!(!bak_path.exists(), "failed backup publish must not leave target directory");

    db.close().unwrap();
}

#[test]
fn close_inner_before_drain_run_point_coordinates_deterministic_close_race() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(dir.path(), OpenOptions::default()).unwrap());
    db.set_schema(&simple_schema()).unwrap();

    let hit_seam = Arc::new(AtomicBool::new(false));
    let db_check = Arc::clone(&db);
    let hit_flag = Arc::clone(&hit_seam);
    failpoints::run_once_here(points::CLOSE_INNER_BEFORE_DRAIN, move || {
        hit_flag.store(true, Ordering::SeqCst);
        // Once `close_inner` has taken `handle`, any new operation MUST immediately see `Closed`!
        let err = db_check.get_row("any").unwrap_err();
        assert_eq!(err.code, ErrorCode::Closed);
    });

    db.close_within(Some(Duration::from_secs(5))).unwrap();
    assert!(hit_seam.load(Ordering::SeqCst), "CLOSE_INNER_BEFORE_DRAIN run_point must have fired");
}

#[test]
fn skip_grouped_durable_pin_failpoint_leaves_grouped_flush_unpinned_for_negative_control() {
    // 1. Real Database path: Grouped flush still seals `synced.v2` while unpinned.
    let dir = tempfile::tempdir().unwrap();
    let _guard = failpoints::fail_while_at(points::SKIP_GROUPED_DURABLE_PIN, dir.path());
    let db = Database::open(dir.path(), OpenOptions { durability: DurabilityMode::Grouped, ..OpenOptions::default() })
        .unwrap();
    db.set_schema(&simple_schema()).unwrap();
    create_note(&db, "n1", "grouped_but_unpinned").unwrap();
    db.flush().unwrap();
    assert!(dir.path().join("synced.v2").exists(), "Grouped flush must still write synced.v2 when unpinned");
    db.close().unwrap();

    // 2. DurabilityGate<SimKvStore, SimVfs> proof: SKIP_GROUPED_DURABLE_PIN actually skips installing
    //    the durable pin across sync(), grow_map(), and adopt_resized_map() so unsynced D+1..D+4
    //    overwrites corrupt snapshot D on power loss!
    use elmdb_sim::{SimKvConfig, SimKvStore, SimVfs};
    use elmdb_storage::{BootId, DbFlags, DurabilityGate, KvRead, KvStore, StorageError};

    for exercise in ["sync", "grow_map", "adopt_resized_map"] {
        let sim_dir_str = format!("/elmdb-fp-skip-pin-sim-{exercise}");
        let sim_dir = std::path::Path::new(&sim_dir_str);
        let _sim_guard = failpoints::fail_while_at(points::SKIP_GROUPED_DURABLE_PIN, sim_dir);
        let sim = SimKvStore::new(SimKvConfig::default());
        let vfs = SimVfs::new(BootId([0x77; 16]));
        let (gate, _) = DurabilityGate::open(sim.clone(), vfs, sim_dir, DurabilityMode::Grouped).unwrap();
        let sub = {
            let mut w = gate.begin_write().unwrap();
            let d = w.open_subdb("items", DbFlags::DEFAULT).unwrap();
            w.put(d, b"k", b"durable-D").unwrap();
            w.commit().unwrap();
            d
        };
        gate.sync().unwrap();
        match exercise {
            "grow_map" => {
                gate.grow_map(Some(sim.map_size() * 2)).unwrap();
            }
            "adopt_resized_map" => {
                sim.inject_read_map_resized(1);
                let _ro = gate.begin_read().unwrap();
            }
            _ => {}
        }
        for step in 1..=4u8 {
            let mut w = gate.begin_write().unwrap();
            w.put(sub, b"k", &[step; 64]).unwrap();
            w.commit().unwrap();
        }
        sim.crash_power_loss();
        let ro = sim.begin_read().unwrap();
        assert!(
            matches!(ro.get(sub, b"k"), Err(StorageError::Corrupted(_))),
            "SKIP_GROUPED_DURABLE_PIN ({exercise}) must leave Grouped snapshot D unpinned so freelist reuse corrupts D"
        );
    }
}
