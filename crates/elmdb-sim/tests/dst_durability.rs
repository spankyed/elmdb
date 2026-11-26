use std::collections::BTreeMap;
use std::path::Path;

use elmdb_sim::{SimKvConfig, SimKvStore, SimRng, SimVfs};
use elmdb_storage::{
    BootId, DbFlags, DurabilityGate, DurabilityMode, KvCursor, KvRead, RecoveryStatus, SYNC_SIDECAR_NAME,
    SYNC_SLOT_BYTES, StorageError, SubDbPool,
};

#[test]
fn dst_strict_crash_and_reboot_linearizability_across_64_seeds() {
    for seed in 0..64u64 {
        let mut rng = SimRng::seeded(seed);
        let dir = Path::new("/sim/db");
        let store = SimKvStore::new(SimKvConfig::default());
        let vfs = SimVfs::new(BootId([seed as u8; 16]));
        let gate = DurabilityGate::new(store.clone(), vfs.clone(), dir, DurabilityMode::Strict);

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

            // Periodically inject power-loss + reboot and verify strict linearizability.
            if step % 10 == 9 {
                store.crash_power_loss();
                vfs.crash_power_loss(&mut rng, true);
                let reopened = DurabilityGate::new(store.clone(), vfs.clone(), dir, DurabilityMode::Strict);
                let report = reopened.recover().unwrap();
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
fn dst_grouped_unsynced_tail_reverts_cleanly_on_power_loss_and_survives_torn_slot() {
    let mut rng = SimRng::seeded(0xDEAD_BEEF);
    let dir = Path::new("/sim/grouped");
    let store = SimKvStore::default();
    let vfs = SimVfs::new(BootId([0x22; 16]));
    let gate = DurabilityGate::new(store.clone(), vfs.clone(), dir, DurabilityMode::Grouped);

    let mut txn = gate.begin_write().unwrap();
    let db = txn.open_subdb("meta", DbFlags::DEFAULT).unwrap();
    txn.put(db, b"a", b"1").unwrap();
    txn.commit().unwrap();
    gate.sync().unwrap();

    // Write a second synced commit so both slots 0 and 1 are populated.
    let mut txn = gate.begin_write().unwrap();
    txn.put(db, b"b", b"2").unwrap();
    txn.commit().unwrap();
    let slot1 = gate.sync().unwrap().unwrap();
    assert_eq!(slot1.seq, 1);

    // Corrupt slot 1 (simulating a torn 4 KiB write on slot 1) -> slot 0 still decodes cleanly!
    let sidecar = dir.join(SYNC_SIDECAR_NAME);
    vfs.corrupt_byte(&sidecar, SYNC_SLOT_BYTES + 20, 0xFF);
    let report = gate.recover().unwrap();
    assert_eq!(report.valid_slots, 1);
    assert_eq!(report.durable.unwrap().seq, 0);

    // Repair by syncing slot 2 (goes into slot 0) and slot 3 (overwrites torn slot 1).
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

    let reopened = DurabilityGate::new(store.clone(), vfs.clone(), dir, DurabilityMode::Grouped);
    let report = reopened.recover().unwrap();
    assert_eq!(report.status, RecoveryStatus::RebootVerified);
    let ro = reopened.begin_read().unwrap();
    assert_eq!(ro.get(db, b"a").unwrap(), Some(b"1".as_slice()));
    assert_eq!(ro.get(db, b"b").unwrap(), Some(b"2".as_slice()));
    assert_eq!(ro.get(db, b"unsynced").unwrap(), None);
}

#[test]
fn dst_sync_failure_permanently_poisons_durability_gate() {
    let dir = Path::new("/sim/poison");
    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let gate = DurabilityGate::new(store.clone(), vfs.clone(), dir, DurabilityMode::Strict);

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

    // Every subsequent begin_write or sync MUST fail with Poisoned.
    assert!(matches!(gate.begin_write(), Err(StorageError::Poisoned(_))));
    assert!(matches!(gate.sync(), Err(StorageError::Poisoned(_))));
}

#[test]
fn dst_mvcc_reader_survives_ping_pong_slot_truncate() {
    let dir = Path::new("/sim/pingpong");
    let store = SimKvStore::default();
    let vfs = SimVfs::default();
    let gate = DurabilityGate::new(store, vfs, dir, DurabilityMode::Unsafe);

    let mut pool = SubDbPool::new(4);
    let slot_a = pool.allocate().unwrap();

    let mut txn = gate.begin_write().unwrap();
    let db_a = txn.open_subdb(&slot_a.name(), DbFlags::DEFAULT).unwrap();
    txn.put(db_a, b"term", b"old_postings").unwrap();
    txn.commit().unwrap();

    // Pin an active MVCC reader on slot_a.
    let reader = gate.begin_read().unwrap();
    assert_eq!(reader.get(db_a, b"term").unwrap(), Some(b"old_postings".as_slice()));

    // Rebuild into staging slot_b, flip, and O(1) truncate slot_a while `reader` is still alive!
    let slot_b = pool.allocate().unwrap();
    let mut txn = gate.begin_write().unwrap();
    let db_b = txn.open_subdb(&slot_b.name(), DbFlags::DEFAULT).unwrap();
    txn.put(db_b, b"term", b"new_postings").unwrap();
    txn.truncate(db_a).unwrap();
    txn.commit().unwrap();
    pool.recycle(slot_a).unwrap();

    // Old reader STILL sees old snapshot intact!
    assert_eq!(reader.get(db_a, b"term").unwrap(), Some(b"old_postings".as_slice()));

    // New reader sees empty slot_a and populated slot_b!
    let reader2 = gate.begin_read().unwrap();
    assert_eq!(reader2.get(db_a, b"term").unwrap(), None);
    assert_eq!(reader2.get(db_b, b"term").unwrap(), Some(b"new_postings".as_slice()));
}
