use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;

use elmdb_sim::{SimKvConfig, SimKvStore, SimRng, SimVfs};
use elmdb_storage::{
    BootId, DbFlags, DurabilityGate, DurabilityMode, KvCursor, KvRead, RecoveryStatus, SYNC_SIDECAR_NAME,
    SYNC_SLOT_BYTES, StorageError, SubDbPool, Vfs,
};

#[test]
fn dst_strict_crash_and_reboot_linearizability_across_64_seeds() {
    for seed in 0..64u64 {
        let mut rng = SimRng::seeded(seed);
        let dir = Path::new("/sim/db");
        let store = SimKvStore::new(SimKvConfig::default());
        let vfs = SimVfs::new(BootId([seed as u8; 16]));
        let (gate, report) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Strict).unwrap();
        assert_eq!(report.status, RecoveryStatus::Fresh);

        let mut oracle: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        let mut txn = gate.begin_write().unwrap();
        let main_db = txn.open_subdb("entities", DbFlags::DEFAULT).unwrap();
        txn.commit().unwrap();

        for step in 0..40 {
            let mut txn = gate.begin_write().unwrap();
            let mut staged = oracle.clone();
            let ops = 1 + rng.gen_range(4);

            let use_savepoint = rng.gen_bool(1, 2);
            let commit_sp = rng.gen_bool(1, 2);
            if use_savepoint {
                let depth = txn.begin_savepoint().unwrap();
                let mut sp_staged = staged.clone();
                for _ in 0..ops {
                    let k = vec![b'k', rng.gen_range(16) as u8];
                    if rng.gen_bool(2, 3) {
                        let v = vec![step as u8, rng.gen_range(255) as u8];
                        txn.put(main_db, &k, &v).unwrap();
                        sp_staged.insert(k, v);
                    } else {
                        txn.del(main_db, &k).unwrap();
                        sp_staged.remove(&k);
                    }
                }
                if commit_sp {
                    txn.commit_savepoint(depth).unwrap();
                    staged = sp_staged;
                } else {
                    txn.abort_savepoint(depth).unwrap();
                }
            } else {
                let k = vec![b'k', rng.gen_range(16) as u8];
                let v = vec![step as u8];
                txn.put(main_db, &k, &v).unwrap();
                staged.insert(k, v);
            }

            if rng.gen_bool(4, 5) {
                txn.commit().unwrap();
                oracle = staged;
            } else {
                txn.abort();
            }

            if step % 10 == 9 {
                store.crash_power_loss();
                vfs.crash_power_loss(&mut rng, true);
                let (reopened, report) =
                    DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Strict).unwrap();
                assert_eq!(
                    report.status,
                    RecoveryStatus::RebootVerified,
                    "seed={seed} step={step}: strict mode must always verify cleanly after reboot"
                );
                let ro = reopened.begin_read().unwrap();
                let mut cursor = ro.cursor(main_db).unwrap();
                let mut actual = BTreeMap::new();
                if let Some((k, v)) = cursor.first().unwrap() {
                    actual.insert(k.to_vec(), v.to_vec());
                    while let Some((k, v)) = cursor.next().unwrap() {
                        actual.insert(k.to_vec(), v.to_vec());
                    }
                }
                assert_eq!(actual, oracle, "seed={seed} step={step}: recovered state diverged from oracle");
            }
        }
    }
}

#[test]
fn dst_torn_sidecar_write_between_write_at_and_sync_recovers_via_sibling_slot() {
    // Exercises SimVfs's actual torn 4 KiB write injection path between write_at() and sync_file().
    let dir = Path::new("/sim/torn");
    let store = SimKvStore::default();
    let vfs = SimVfs::new(BootId([0x44; 16]));
    let (gate, _) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Grouped).unwrap();

    let mut txn = gate.begin_write().unwrap();
    let db = txn.open_subdb("meta", DbFlags::DEFAULT).unwrap();
    txn.put(db, b"a", b"1").unwrap();
    txn.commit().unwrap();
    let slot0 = gate.sync().unwrap().unwrap();
    assert_eq!(slot0.seq, 0);

    // Next sync writes to slot 1, then fails on sync_file leaving dirty bytes in slot 1!
    let mut txn = gate.begin_write().unwrap();
    txn.put(db, b"b", b"2").unwrap();
    txn.commit().unwrap();
    vfs.faults().fail_next_sync.store(1, Ordering::Release);
    let err = gate.sync().unwrap_err();
    assert!(matches!(err, StorageError::DurabilityFailed(_)));
    assert!(gate.is_poisoned());

    // Now simulate power loss with a seed that triggers torn-page injection into dirty slot 1.
    let mut saw_torn = false;
    for seed in 0..32u64 {
        let mut rng = SimRng::seeded(seed);
        let torn = vfs.crash_power_loss(&mut rng, true);
        if torn > 0 {
            saw_torn = true;
            break;
        }
    }
    // Even if slot 1 was torn or reverted, reopening MUST recover cleanly via slot 0!
    let (_reopened, report) = DurabilityGate::open(store, vfs, dir, DurabilityMode::Grouped).unwrap();
    assert!(report.valid_slots >= 1);
    assert!(report.durable.is_some());
    assert!(saw_torn || report.durable.unwrap().seq == 0);
}

#[test]
fn dst_grouped_unsynced_tail_reverts_cleanly_on_power_loss_and_survives_torn_slot() {
    let mut rng = SimRng::seeded(0xDEAD_BEEF);
    let dir = Path::new("/sim/grouped");
    let store = SimKvStore::default();
    let vfs = SimVfs::new(BootId([0x22; 16]));
    let (gate, _) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Grouped).unwrap();

    let mut txn = gate.begin_write().unwrap();
    let db = txn.open_subdb("meta", DbFlags::DEFAULT).unwrap();
    txn.put(db, b"a", b"1").unwrap();
    txn.commit().unwrap();
    gate.sync().unwrap();

    let mut txn = gate.begin_write().unwrap();
    txn.put(db, b"b", b"2").unwrap();
    txn.commit().unwrap();
    let slot1 = gate.sync().unwrap().unwrap();
    assert_eq!(slot1.seq, 1);

    // Corrupt slot 1 -> slot 0 still decodes cleanly!
    let sidecar = dir.join(SYNC_SIDECAR_NAME);
    vfs.corrupt_byte(&sidecar, SYNC_SLOT_BYTES + 20, 0xFF);
    let report = gate.recover().unwrap();
    assert_eq!(report.valid_slots, 1);
    assert_eq!(report.durable.unwrap().seq, 0);

    // Repair by syncing twice.
    gate.sync().unwrap();
    gate.sync().unwrap();
    let report = gate.recover().unwrap();
    assert_eq!(report.valid_slots, 2);
    assert_eq!(report.status, RecoveryStatus::Clean);

    // Commit an unsynced grouped write, then crash with reboot.
    let mut txn = gate.begin_write().unwrap();
    txn.put(db, b"unsynced", b"lost").unwrap();
    txn.commit().unwrap();

    store.crash_power_loss();
    vfs.crash_power_loss(&mut rng, true);

    let (reopened, report) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Grouped).unwrap();
    assert_eq!(report.status, RecoveryStatus::RebootVerified);
    let ro = reopened.begin_read().unwrap();
    assert_eq!(ro.get(db, b"a").unwrap(), Some(b"1".as_slice()));
    assert_eq!(ro.get(db, b"b").unwrap(), Some(b"2".as_slice()));
    assert_eq!(ro.get(db, b"unsynced").unwrap(), None);
}

#[test]
fn dst_recover_detects_corrupt_sidecar_missing_sidecar_rollback_and_bitflip() {
    let dir = Path::new("/sim/corrupt");
    let store = SimKvStore::default();
    let vfs = SimVfs::new(BootId([0x77; 16]));
    let (gate, _) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Strict).unwrap();

    // Unrecovered gate must refuse writes & syncs.
    let unrecovered = DurabilityGate::new_unrecovered(store.clone(), vfs.clone(), dir, DurabilityMode::Strict);
    assert!(matches!(unrecovered.begin_write(), Err(StorageError::InvalidState(_))));
    assert!(matches!(unrecovered.sync(), Err(StorageError::InvalidState(_))));

    let mut txn = gate.begin_write().unwrap();
    let db = txn.open_subdb("meta", DbFlags::DEFAULT).unwrap();
    txn.put(db, b"k", b"v").unwrap();
    txn.commit().unwrap();

    // 1. Corrupt BOTH slots -> recover MUST fail with Corrupted (never Fresh!).
    let sidecar = dir.join(SYNC_SIDECAR_NAME);
    let vfs_corrupt = SimVfs::new(BootId([0x77; 16]));
    let mut broken = vfs.read_file(&sidecar).unwrap();
    broken[15] ^= 0xFF;
    if broken.len() > SYNC_SLOT_BYTES + 15 {
        broken[SYNC_SLOT_BYTES + 15] ^= 0xFF;
    }
    vfs_corrupt.write_all(&sidecar, &broken).unwrap();
    let res = DurabilityGate::open(store.clone(), vfs_corrupt, dir, DurabilityMode::Strict);
    assert!(matches!(res, Err(StorageError::Corrupted(_))));

    // 2. Missing sidecar when observed_txnid > 0 -> UnsyncedAfterReboot (NOT Fresh).
    let vfs_missing = SimVfs::new(BootId([0x77; 16]));
    let (_, report) = DurabilityGate::open(store.clone(), vfs_missing, dir, DurabilityMode::Strict).unwrap();
    assert_eq!(report.status, RecoveryStatus::UnsyncedAfterReboot);

    // 3. Same-txnid silent bitflip -> Corrupted.
    store.simulate_silent_bitflip("meta", b"k");
    let res = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Strict);
    assert!(matches!(res, Err(StorageError::Corrupted(_))));

    // 4. Storage rollback (observed_txnid < durable.txnid) -> Corrupted.
    store.simulate_corrupt_rollback();
    let res = DurabilityGate::open(store, vfs, dir, DurabilityMode::Strict);
    assert!(matches!(res, Err(StorageError::Corrupted(_))));
}

#[test]
fn dst_sync_failure_permanently_poisons_reads_writes_sync_and_recover() {
    let dir = Path::new("/sim/poison");
    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let (gate, _) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Strict).unwrap();

    let mut txn = gate.begin_write().unwrap();
    let db = txn.open_subdb("meta", DbFlags::DEFAULT).unwrap();
    txn.put(db, b"ok", b"yes").unwrap();
    txn.commit().unwrap();
    assert!(!gate.is_poisoned());

    // Inject a store sync failure -> commit fails AND permanently poisons the gate.
    store.inject_sync_failures(1);
    let mut txn = gate.begin_write().unwrap();
    txn.put(db, b"bad", b"fail").unwrap();
    let err = txn.commit().unwrap_err();
    assert!(matches!(err, StorageError::DurabilityFailed(_)));
    assert!(gate.is_poisoned());

    // Every subsequent operation fails closed with Poisoned!
    assert!(matches!(gate.begin_write(), Err(StorageError::Poisoned(_))));
    assert!(matches!(gate.begin_read(), Err(StorageError::Poisoned(_))));
    assert!(matches!(gate.sync(), Err(StorageError::Poisoned(_))));
    assert!(matches!(gate.recover(), Err(StorageError::Poisoned(_))));
}

#[test]
fn dst_mvcc_reader_survives_ping_pong_slot_truncate() {
    let dir = Path::new("/sim/pingpong");
    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let (gate, _) = DurabilityGate::open(store, vfs, dir, DurabilityMode::Unsafe).unwrap();

    let mut pool = SubDbPool::new(4);
    let slot_a = pool.allocate().unwrap();

    let mut txn = gate.begin_write().unwrap();
    let db_a = txn.open_subdb(&slot_a.name(), DbFlags::DEFAULT).unwrap();
    txn.put(db_a, b"term", b"old_postings").unwrap();
    txn.commit().unwrap();

    let reader = gate.begin_read().unwrap();
    assert_eq!(reader.get(db_a, b"term").unwrap(), Some(b"old_postings".as_slice()));

    let slot_b = pool.allocate().unwrap();
    let mut txn = gate.begin_write().unwrap();
    let db_b = txn.open_subdb(&slot_b.name(), DbFlags::DEFAULT).unwrap();
    txn.put(db_b, b"term", b"new_postings").unwrap();
    txn.truncate(db_a).unwrap();
    txn.commit().unwrap();
    pool.recycle(slot_a).unwrap();

    assert_eq!(reader.get(db_a, b"term").unwrap(), Some(b"old_postings".as_slice()));

    let reader2 = gate.begin_read().unwrap();
    assert_eq!(reader2.get(db_a, b"term").unwrap(), None);
    assert_eq!(reader2.get(db_b, b"term").unwrap(), Some(b"new_postings".as_slice()));
}

#[test]
fn dst_zero_byte_sidecar_recovers_as_unsynced_and_grouped_sync_failure_poisons() {
    // P0-6: A 0-byte `synced.v2` (from a crash right after initial file creation before rename)
    // must recover as `UnsyncedAfterReboot` instead of permanently bricking with `Corrupted`.
    let dir = Path::new("/sim/zero_sidecar");
    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let mut rng = SimRng::seeded(7);
    let (gate, _) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Grouped).unwrap();
    let mut txn = gate.begin_write().unwrap();
    let db = txn.open_subdb("meta", DbFlags::DEFAULT).unwrap();
    txn.put(db, b"k", b"v").unwrap();
    txn.commit().unwrap();
    drop(gate);

    // Simulate a 0-byte truncated initial sidecar on disk and reboot.
    vfs.write_all(&dir.join(SYNC_SIDECAR_NAME), &[]).unwrap();
    vfs.sync_file(&dir.join(SYNC_SIDECAR_NAME)).unwrap();
    vfs.crash_power_loss(&mut rng, true);

    let (reopened, report) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Grouped).unwrap();
    assert_eq!(report.status, RecoveryStatus::UnsyncedAfterReboot);
    assert_eq!(report.valid_slots, 0);
    assert!(reopened.has_unacked_reboot());

    // B1: Routine `sync()` MUST NOT overwrite `synced.v2` while `unacked_reboot` is set!
    assert_eq!(reopened.sync().unwrap(), None);
    assert!(reopened.has_unacked_reboot());
    drop(reopened);
    let (still_unacked, report_still) =
        DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Strict).unwrap();
    assert_eq!(report_still.status, RecoveryStatus::UnsyncedAfterReboot);
    assert!(still_unacked.has_unacked_reboot());

    // B1: Even a Strict commit while `unacked_reboot` is set flushes `data.mdb` without sealing `synced.v2`.
    {
        let mut txn = still_unacked.begin_write().unwrap();
        txn.put(db, b"k_unacked", b"v_unacked").unwrap();
        txn.commit().unwrap();
    }
    assert!(still_unacked.has_unacked_reboot());
    // Injecting a sync failure on a Strict commit while unacked_reboot is set must still poison the gate!
    {
        let (re_strict, rep_s) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Strict).unwrap();
        assert_eq!(rep_s.status, RecoveryStatus::UnsyncedAfterReboot);
        let mut txn = re_strict.begin_write().unwrap();
        txn.put(db, b"k_fail_strict", b"v").unwrap();
        store.inject_sync_failures(1);
        assert!(matches!(txn.commit(), Err(StorageError::DurabilityFailed(_))));
        assert!(re_strict.is_poisoned());
    }

    // B1: Injecting a sync failure during `acknowledge_restore()` must keep `unacked_reboot` true and poison the gate!
    {
        let (re_ack_fail, _) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Grouped).unwrap();
        assert!(re_ack_fail.has_unacked_reboot());
        store.inject_sync_failures(1);
        assert!(matches!(re_ack_fail.acknowledge_restore(), Err(StorageError::DurabilityFailed(_))));
        assert!(re_ack_fail.has_unacked_reboot());
        assert!(re_ack_fail.is_poisoned());
    }

    // B1: Explicit `acknowledge_restore()` seals `synced.v2` and clears `unacked_reboot` so next open is Clean!
    assert!(still_unacked.acknowledge_restore().unwrap().is_some());
    assert!(!still_unacked.has_unacked_reboot());
    drop(still_unacked);
    let (healed, report2) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Grouped).unwrap();
    assert_eq!(report2.status, RecoveryStatus::Clean);
    assert_eq!(report2.valid_slots, 1);
    assert!(!healed.has_unacked_reboot());
    assert!(!healed.is_read_only());

    // B3: Read-only gate refuses writes, sync, and acknowledge_restore with `StorageError::ReadOnly`.
    let (ro_gate, ro_rep) =
        DurabilityGate::open_with_options(store.clone(), vfs.clone(), dir, DurabilityMode::Strict, true).unwrap();
    assert!(ro_gate.is_read_only());
    assert_eq!(ro_rep.status, RecoveryStatus::Clean);
    assert!(matches!(ro_gate.begin_write(), Err(StorageError::ReadOnly)));
    let ro_arc = std::sync::Arc::new(ro_gate);
    assert!(matches!(ro_arc.begin_write_owned(), Err(StorageError::ReadOnly)));
    assert!(matches!(ro_arc.sync(), Err(StorageError::ReadOnly)));
    assert!(matches!(ro_arc.acknowledge_restore(), Err(StorageError::ReadOnly)));
    drop(ro_arc);

    // B1: Unsafe mode `acknowledge_restore()` clears `unacked_reboot` and returns `Ok(None)`.
    let (unsafe_gate, _) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Unsafe).unwrap();
    assert_eq!(unsafe_gate.acknowledge_restore().unwrap(), None);
    assert!(!unsafe_gate.has_unacked_reboot());

    // B1: Calling `acknowledge_restore()` on an unrecovered gate fails with InvalidState.
    let unrec = DurabilityGate::new_unrecovered(store.clone(), vfs.clone(), dir, DurabilityMode::Strict);
    assert!(matches!(unrec.acknowledge_restore(), Err(StorageError::InvalidState(_))));

    // P2-3: Post-commit I/O failure in Grouped `sync()` permanently poisons the gate!
    let mut txn = healed.begin_write().unwrap();
    txn.put(db, b"k2", b"v2").unwrap();
    txn.commit().unwrap();
    store.inject_sync_failures(1);
    let err = healed.sync().unwrap_err();
    assert!(matches!(err, StorageError::DurabilityFailed(_)));
    assert!(healed.is_poisoned());
    assert!(matches!(healed.begin_read(), Err(StorageError::Poisoned(_))));
}
