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

    // B1: Routine `sync()` in Grouped mode flushes `data.mdb` (`store.sync`) WITHOUT overwriting `synced.v2`!
    assert_eq!(reopened.sync().unwrap(), None);
    assert!(reopened.has_unacked_reboot());
    {
        let (re_sync_fail, _) = DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Grouped).unwrap();
        assert!(re_sync_fail.has_unacked_reboot());
        store.inject_sync_failures(1);
        assert!(matches!(re_sync_fail.sync(), Err(StorageError::DurabilityFailed(_))));
        assert!(re_sync_fail.is_poisoned());
    }
    // B1: Unsafe mode `acknowledge_restore()` clears an actual `unacked_reboot == true` and returns `Ok(None)`.
    {
        let (unsafe_unacked, u_rep) =
            DurabilityGate::open(store.clone(), vfs.clone(), dir, DurabilityMode::Unsafe).unwrap();
        assert_eq!(u_rep.status, RecoveryStatus::UnsyncedAfterReboot);
        assert!(unsafe_unacked.has_unacked_reboot());
        assert_eq!(unsafe_unacked.acknowledge_restore().unwrap(), None);
        assert!(!unsafe_unacked.has_unacked_reboot());
    }
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
        assert!(matches!(re_ack_fail.acknowledge_restore(), Err(StorageError::Poisoned(_))));
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

    // B3: Read-only gate refuses writes, sync, grow_map, and acknowledge_restore with `StorageError::ReadOnly`.
    let (ro_gate, ro_rep) =
        DurabilityGate::open_with_options(store.clone(), vfs.clone(), dir, DurabilityMode::Strict, true).unwrap();
    assert!(ro_gate.is_read_only());
    assert_eq!(ro_rep.status, RecoveryStatus::Clean);
    assert!(matches!(ro_gate.begin_write(), Err(StorageError::ReadOnly)));
    assert!(matches!(ro_gate.grow_map(None), Err(StorageError::ReadOnly)));
    let ro_arc = std::sync::Arc::new(ro_gate);
    assert!(matches!(ro_arc.begin_write_owned(), Err(StorageError::ReadOnly)));
    assert!(matches!(ro_arc.sync(), Err(StorageError::ReadOnly)));
    assert!(matches!(ro_arc.acknowledge_restore(), Err(StorageError::ReadOnly)));
    drop(ro_arc);

    // B1: Calling `acknowledge_restore()` on an unrecovered gate fails with InvalidState.
    let unrec = DurabilityGate::new_unrecovered(store.clone(), vfs.clone(), dir, DurabilityMode::Strict);
    assert!(matches!(unrec.acknowledge_restore(), Err(StorageError::InvalidState(_))));

    // Verify SimKvStore::grow_map never shrinks, respects explicit larger target, and doubles cleanly on `None`.
    {
        use elmdb_storage::KvStore;
        let orig_map = store.map_size();
        assert_eq!(store.grow_map(Some(1)).unwrap(), orig_map);
        assert_eq!(store.grow_map(Some(orig_map + 4096)).unwrap(), orig_map + 4096);
        assert_eq!(store.grow_map(None).unwrap(), (orig_map + 4096) * 2);
    }

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

#[test]
fn dst_pr268_grouped_durable_pin_prevents_freelist_reuse_corruption_and_guards_unacked_reboot() {
    use std::path::PathBuf;

    use elmdb_storage::{KvStore, KvWrite};

    let mut rng = SimRng::seeded(0x268);

    // Part A: Demonstrate the exact freelist page-reuse failure mode when NO reader/pin holds `D`.
    {
        let store = SimKvStore::new(SimKvConfig::default());
        // Before first sync (`durable.txnid == 0`), multiple commits + power loss must NOT mark txnid 0 corrupted.
        let db = {
            let mut rw = store.begin_write(()).unwrap();
            let db = rw.open_subdb("data", DbFlags::DEFAULT).unwrap();
            rw.put(db, b"k", b"pre0").unwrap();
            let _ = rw.commit_raw().unwrap();
            db
        };
        for i in 1..=4 {
            let mut rw = store.begin_write(()).unwrap();
            rw.put(db, b"k", format!("pre{i}").as_bytes()).unwrap();
            let _ = rw.commit_raw().unwrap();
        }
        store.crash_power_loss();
        let ro_empty = store.begin_read().unwrap();
        assert_eq!(ro_empty.txnid(), 0);
        assert!(
            ro_empty.open_subdb_existing("data", DbFlags::DEFAULT).unwrap().is_none(),
            "commits before first sync (durable.txnid == 0) must not corrupt txnid 0 on power loss"
        );
        drop(ro_empty);

        let mut rw = store.begin_write(()).unwrap();
        let db = rw.open_subdb("data", DbFlags::DEFAULT).unwrap();
        rw.put(db, b"k", b"durable_v1").unwrap();
        let (r1, ()) = rw.commit_raw().unwrap();
        store.sync(&()).unwrap();
        assert_eq!(store.durable_txnid(), r1.txnid);

        // Clean (no-op) commits after sync must NOT trigger freelist reuse corruption on power loss!
        for _ in 0..4 {
            let rw = store.begin_write(()).unwrap();
            let _ = rw.commit_raw().unwrap();
        }
        store.crash_power_loss();
        assert_eq!(store.begin_read().unwrap().get(db, b"k").unwrap(), Some(b"durable_v1".as_slice()));

        // Holding TWO simultaneous `SimRoTxn`s at D: dropping ONE leaves the second still protecting D
        // across D+1..D+3 so a power loss STILL reads D intact!
        let ro_guard1 = store.begin_read().unwrap();
        let ro_guard2 = store.begin_read().unwrap();
        drop(ro_guard1);
        for i in 2..=4 {
            let mut rw = store.begin_write(()).unwrap();
            rw.put(db, b"k", format!("v{i}").as_bytes()).unwrap();
            let _ = rw.commit_raw().unwrap();
        }
        drop(ro_guard2);
        store.crash_power_loss();
        assert_eq!(
            store.begin_read().unwrap().get(db, b"k").unwrap(),
            Some(b"durable_v1".as_slice()),
            "second concurrent reader at D must keep D's pages protected after first reader drops"
        );

        // Only 2 unsynced commits (D+1, D+2) with NO reader do NOT recycle D's pages yet (LMDB has 2 meta slots);
        // kills `final_snap.txnid >= freed_at + 2` -> `<` mutant!
        for i in 1..=2 {
            let mut rw = store.begin_write(()).unwrap();
            rw.put(db, b"k", format!("two_commits_{i}").as_bytes()).unwrap();
            let _ = rw.commit_raw().unwrap();
        }
        store.crash_power_loss();
        assert_eq!(
            store.begin_read().unwrap().get(db, b"k").unwrap(),
            Some(b"durable_v1".as_slice()),
            "D+1..D+2 without D+3 must not recycle D's pages"
        );

        // Open TWO readers at D and drop BOTH (WITHOUT crashing in between) so refcount goes 0 -> 1 -> 2 -> 1 -> 0;
        // kills `SimRoTxn::drop` `*count -= 1` -> `+=` / `/=` mutants when D+1..D+3 then recycles D's pages!
        let r_a = store.begin_read().unwrap();
        let r_b = store.begin_read().unwrap();
        drop(r_a);
        drop(r_b);

        // Now with ZERO readers at <= D (even with a newer reader at D+1!), D+1..D+3 recycles D's pages!
        let mut rw = store.begin_write(()).unwrap();
        rw.put(db, b"k", b"d_plus_1").unwrap();
        let _ = rw.commit_raw().unwrap();
        let newer_reader = store.begin_read().unwrap();
        assert!(newer_reader.txnid() > r1.txnid);
        for i in 2..=3 {
            let mut rw = store.begin_write(()).unwrap();
            rw.put(db, b"k", format!("recycled_{i}").as_bytes()).unwrap();
            let _ = rw.commit_raw().unwrap();
        }
        drop(newer_reader);

        store.crash_power_loss();
        // Meta page STILL matches D, while reading the tree fails with `Corrupted`!
        assert_eq!(store.active_receipt().unwrap().txnid, r1.txnid);
        let ro = store.begin_read().unwrap();
        assert!(matches!(ro.get(db, b"k"), Err(StorageError::Corrupted(_))));
        assert!(matches!(ro.cursor(db), Err(StorageError::Corrupted(_))));
        assert!(matches!(ro.entries(db), Err(StorageError::Corrupted(_))));
        assert!(matches!(ro.open_subdb_existing("data", DbFlags::DEFAULT), Err(StorageError::Corrupted(_))));
        // Verify `adopt_resized_map_locked` returns real `map_size` (kills Ok(0)/Ok(1) mutants).
        assert_eq!(store.adopt_resized_map_locked(&(), false).unwrap(), store.map_size());
    }

    // Part B: `DurabilityGate` in `Grouped` mode pins D on `sync()` so D+1..D+10 NEVER corrupt D on power loss.
    let vfs = SimVfs::new(BootId([0x11; 16]));
    let store = SimKvStore::new(SimKvConfig::default());
    let dir = PathBuf::from("/sim-grouped-pin");
    let (gate, rep) = DurabilityGate::open(store.clone(), vfs.clone(), &dir, DurabilityMode::Grouped).unwrap();
    assert_eq!(rep.status, RecoveryStatus::Fresh);

    let db = {
        let mut tx = gate.begin_write().unwrap();
        let db = tx.open_subdb("data", DbFlags::DEFAULT).unwrap();
        tx.put(db, b"k", b"synced_d").unwrap();
        tx.commit().unwrap();
        db
    };
    let slot = gate.sync().unwrap().expect("Grouped sync seals durable slot");
    assert_eq!(store.pinned_txnid(), Some(slot.txnid));

    // Commit 8 unsynced transactions past D with ZERO user readers open.
    for i in 1..=8 {
        let mut tx = gate.begin_write().unwrap();
        tx.put(db, b"k", format!("unsynced_{i}").as_bytes()).unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(store.pinned_txnid(), Some(slot.txnid), "pin must stay at durable snapshot D until next sync()");

    // Power loss + reboot: D's pages were never recycled, so recovery is `RebootVerified` and data reads intact!
    drop(gate);
    store.crash_power_loss();
    vfs.crash_power_loss(&mut rng, true);
    let (reopened, rep) = DurabilityGate::open(store.clone(), vfs.clone(), &dir, DurabilityMode::Grouped).unwrap();
    assert_eq!(rep.status, RecoveryStatus::RebootVerified);
    assert_eq!(store.pinned_txnid(), Some(slot.txnid), "reopening RebootVerified in Grouped mode must re-pin D");
    let ro = reopened.begin_read().unwrap();
    assert_eq!(ro.get(db, b"k").unwrap(), Some(b"synced_d".as_slice()));
    drop(ro);

    // Part C: `SameBootUnsyncedTail` reopen immediately seals & pins `D+k` so subsequent commits + power loss stay intact.
    // First sync in the new boot session so `synced.v2` carries the current `boot_id`.
    let slot_boot2 = reopened.sync().unwrap().expect("sync stamps current boot_id");
    assert_eq!(store.pinned_txnid(), Some(slot_boot2.txnid));
    for i in 1..=2 {
        let mut tx = reopened.begin_write().unwrap();
        tx.put(db, b"k", format!("tail_{i}").as_bytes()).unwrap();
        tx.commit().unwrap();
    }
    let tail_txnid = reopened.last_txnid().unwrap();
    assert!(tail_txnid > slot_boot2.txnid);
    drop(reopened);
    // Process crash WITHOUT OS reboot (page cache survives, in-process pin is lost).
    store.crash_process_same_boot();

    let (same_boot, rep) = DurabilityGate::open(store.clone(), vfs.clone(), &dir, DurabilityMode::Grouped).unwrap();
    assert_eq!(rep.status, RecoveryStatus::SameBootUnsyncedTail);
    assert_eq!(store.durable_txnid(), tail_txnid, "SameBootUnsyncedTail must be sealed on Grouped open");
    assert_eq!(store.pinned_txnid(), Some(tail_txnid), "SameBootUnsyncedTail pin must point to newly sealed tail");

    // 5 more unsynced commits after SameBootUnsyncedTail reopen, then power loss -> recovers intact at `tail_txnid`!
    for i in 1..=5 {
        let mut tx = same_boot.begin_write().unwrap();
        tx.put(db, b"k", format!("after_tail_{i}").as_bytes()).unwrap();
        tx.commit().unwrap();
    }
    drop(same_boot);
    store.crash_power_loss();
    vfs.crash_power_loss(&mut rng, true);
    let (after_tail_reboot, rep) =
        DurabilityGate::open(store.clone(), vfs.clone(), &dir, DurabilityMode::Grouped).unwrap();
    assert_eq!(rep.status, RecoveryStatus::RebootVerified);
    let ro = after_tail_reboot.begin_read().unwrap();
    assert_eq!(ro.get(db, b"k").unwrap(), Some(b"tail_2".as_slice()));
    drop(ro);

    // Part D: `unacked_reboot` blocks BOTH `copy_compacted` (Item 1.3) and `grow_map` (Item 1.1 trap 3) until `acknowledge_restore()`.
    let mut tx = after_tail_reboot.begin_write().unwrap();
    tx.put(db, b"k", b"unacked_tail").unwrap();
    tx.commit().unwrap();
    drop(after_tail_reboot);
    // Reboot WITHOUT losing page cache (simulating OS flushed dirty pages past sidecar before reboot).
    store.crash_process_same_boot();
    vfs.crash_power_loss(&mut rng, true);
    let (unacked, rep) = DurabilityGate::open(store.clone(), vfs.clone(), &dir, DurabilityMode::Grouped).unwrap();
    assert_eq!(rep.status, RecoveryStatus::UnsyncedAfterReboot);
    assert!(unacked.has_unacked_reboot());
    assert_eq!(store.pinned_txnid(), None, "unacked_reboot must NOT silently seal or pin");
    assert!(matches!(unacked.copy_compacted(&PathBuf::from("/sim-bak")), Err(StorageError::DurabilityFailed(_))));
    assert!(matches!(unacked.grow_map(Some(128 * 1024 * 1024)), Err(StorageError::DurabilityFailed(_))));
    // After explicit `acknowledge_restore()`, pin is acquired and `grow_map` succeeds!
    let ack_slot = unacked.acknowledge_restore().unwrap().expect("acknowledge seals sidecar");
    assert!(!unacked.has_unacked_reboot());
    assert_eq!(store.pinned_txnid(), Some(ack_slot.txnid));
    assert!(unacked.grow_map(Some(128 * 1024 * 1024)).is_ok());

    // Sanity check `clear_durable_pin_for_test` + `lock_scope_id` distinctiveness.
    store.clear_durable_pin_for_test();
    assert_eq!(store.pinned_txnid(), None);
    let store2 = SimKvStore::new(SimKvConfig::default());
    assert_ne!(store.lock_scope_id(), store2.lock_scope_id());

    // Part E (P0-1 + P1-1): Fresh Grouped DB -> commit WITHOUT `sync()` -> `grow_map()` MUST BOTH seal
    // `synced.v2` AND acquire the durable pin so subsequent unsynced commits + power loss stay intact;
    // AND if post-remap re-pin fails, `DurabilityGate` MUST poison fail-closed instead of running unpinned!
    {
        let vfs_e = SimVfs::new(BootId([0x77; 16]));
        let store_e = SimKvStore::new(SimKvConfig::default());
        let dir_e = PathBuf::from("/sim-fresh-grow-pin");
        let (gate_e, rep_e) =
            DurabilityGate::open(store_e.clone(), vfs_e.clone(), &dir_e, DurabilityMode::Grouped).unwrap();
        assert_eq!(rep_e.status, RecoveryStatus::Fresh);
        assert_eq!(store_e.pinned_txnid(), None);

        let db_e = {
            let mut tx = gate_e.begin_write().unwrap();
            let db_e = tx.open_subdb("data", DbFlags::DEFAULT).unwrap();
            tx.put(db_e, b"k", b"before_grow").unwrap();
            tx.commit().unwrap();
            db_e
        };
        let tx_before_grow = gate_e.last_txnid().unwrap();
        // `grow_map` on Fresh/unpinned Grouped DB seals AND pins `tx_before_grow` in exactly ONE sidecar write
        // (kills `receipt.txnid >= sealed_txnid` double-seal mutant in `verify_and_seal_after_remap_locked`)!
        let _ = gate_e.grow_map(Some(128 * 1024 * 1024)).unwrap();
        assert_eq!(store_e.durable_txnid(), tx_before_grow);
        assert_eq!(store_e.pinned_txnid(), Some(tx_before_grow), "grow_map on Fresh Grouped DB MUST acquire pin");
        let next_slot_e = gate_e.sync().unwrap().expect("sync slot");
        assert_eq!(
            next_slot_e.seq, 1,
            "grow_map without peer commit must seal sidecar only once (seq 0 -> next seq 1)"
        );

        // 5 unsynced commits + power loss -> D (`before_grow`) stays 100% intact!
        for i in 1..=5 {
            let mut tx = gate_e.begin_write().unwrap();
            tx.put(db_e, b"k", format!("after_grow_{i}").as_bytes()).unwrap();
            tx.commit().unwrap();
        }
        drop(gate_e);
        store_e.crash_power_loss();
        vfs_e.crash_power_loss(&mut rng, true);
        let (re_e, rep_after) =
            DurabilityGate::open(store_e.clone(), vfs_e.clone(), &dir_e, DurabilityMode::Grouped).unwrap();
        assert_eq!(rep_after.status, RecoveryStatus::RebootVerified);
        let ro_e = re_e.begin_read().unwrap();
        assert_eq!(ro_e.get(db_e, b"k").unwrap(), Some(b"before_grow".as_slice()));
        drop(ro_e);

        // P1-1: If post-remap re-pin fails inside `grow_map()`, the gate MUST permanently poison fail-closed!
        store_e.inject_pin_failures(1);
        let grow_err = re_e.grow_map(Some(256 * 1024 * 1024)).unwrap_err();
        assert!(matches!(grow_err, StorageError::DurabilityFailed(_)), "expected DurabilityFailed, got {grow_err:?}");
        assert!(re_e.is_poisoned(), "failed post-remap re-pin MUST poison Grouped DurabilityGate");
        assert!(matches!(re_e.begin_read(), Err(StorageError::Poisoned(_))));
    }

    // Part F: Exhaustive `DurabilityGate` mode / read_only / unrecovered / `MapResized` / peer-commit remap coverage!
    {
        // 1. Strict mode `sync()` and `grow_map()` NEVER acquire a Grouped durable pin, and non-Corrupted
        //    `grow_map` / `adopt_resized_map` failures in Strict mode do NOT poison the gate!
        let vfs_s = SimVfs::new(BootId([0x81; 16]));
        let store_s = SimKvStore::new(SimKvConfig::default());
        let dir_s = PathBuf::from("/sim-strict-no-pin");
        let (strict_gate, _) =
            DurabilityGate::open(store_s.clone(), vfs_s.clone(), &dir_s, DurabilityMode::Strict).unwrap();
        {
            let mut tx = strict_gate.begin_write().unwrap();
            let d = tx.open_subdb("t", DbFlags::DEFAULT).unwrap();
            tx.put(d, b"a", b"b").unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(store_s.pinned_txnid(), None, "Strict commit must not acquire Grouped pin");
        strict_gate.sync().unwrap();
        assert_eq!(store_s.pinned_txnid(), None, "Strict sync must not acquire Grouped pin");
        strict_gate.grow_map(Some(128 * 1024 * 1024)).unwrap();
        assert_eq!(store_s.pinned_txnid(), None, "Strict grow_map must not acquire Grouped pin");
        store_s.inject_remap_failures(1);
        assert!(matches!(strict_gate.grow_map(None), Err(StorageError::ReadersFull)));
        assert!(!strict_gate.is_poisoned(), "Strict non-Corrupted grow_map error must NOT poison gate");
        // `begin_read` hitting `MapResized` in Strict mode adopts cleanly without pinning, and non-Corrupted adopt error does not poison.
        store_s.inject_read_map_resized(1);
        let _ro = strict_gate.begin_read().unwrap();
        assert_eq!(store_s.pinned_txnid(), None);
        store_s.inject_read_map_resized(1);
        store_s.inject_remap_failures(1);
        assert!(matches!(strict_gate.begin_read(), Err(StorageError::ReadersFull)));
        assert!(!strict_gate.is_poisoned(), "Strict non-Corrupted adopt error must NOT poison gate");

        // 2. Unrecovered Grouped gate `grow_map()` does NOT pin or seal; read-only Grouped open & adopt do NOT pin or seal!
        let unrec_g = DurabilityGate::new_unrecovered(store_s.clone(), vfs_s.clone(), &dir_s, DurabilityMode::Grouped);
        unrec_g.grow_map(None).unwrap();
        assert_eq!(store_s.pinned_txnid(), None, "unrecovered Grouped grow_map must not acquire pin");

        let (ro_g, rep_ro) =
            DurabilityGate::open_with_options(store_s.clone(), vfs_s.clone(), &dir_s, DurabilityMode::Grouped, true)
                .unwrap();
        assert_eq!(rep_ro.status, RecoveryStatus::Clean);
        assert_eq!(store_s.pinned_txnid(), None, "read-only Grouped recover must not acquire pin");
        store_s.inject_read_map_resized(1);
        let _ro2 = ro_g.begin_read().unwrap();
        assert_eq!(store_s.pinned_txnid(), None, "read-only Grouped adopt must not acquire pin");
        drop(ro_g);

        // 3. Writable Grouped gate: `sync()` hitting `MapResized` on `refresh_durable_pin` routes through
        //    `adopt_and_pin_grouped`, AND a peer commit landing during `adopt_resized_map` is sealed by
        //    `verify_and_seal_after_remap_locked` before returning!
        let (grp, _) = DurabilityGate::open(store_s.clone(), vfs_s.clone(), &dir_s, DurabilityMode::Grouped).unwrap();
        assert!(store_s.pinned_txnid().is_some());
        let pre_txnid = grp.last_txnid().unwrap();
        store_s.inject_pin_map_resized(1);
        store_s.inject_peer_commit_on_adopt(1);
        grp.sync().unwrap();
        assert_eq!(
            store_s.durable_txnid(),
            pre_txnid + 1,
            "verify_and_seal_after_remap_locked must seal cross-process commit made during adopt"
        );
        assert_eq!(store_s.pinned_txnid(), Some(pre_txnid + 1));

        // 4. Writable Grouped gate: `adopt_resized_map_internal` failing to re-pin poisons fail-closed!
        store_s.inject_read_map_resized(1);
        store_s.inject_pin_failures(1);
        let adopt_err = grp.begin_read().err().expect("failed Grouped adopt re-pin must error");
        assert!(matches!(adopt_err, StorageError::DurabilityFailed(_)), "got {adopt_err:?}");
        assert!(grp.is_poisoned(), "failed Grouped adopt re-pin MUST poison gate");
    }
}
