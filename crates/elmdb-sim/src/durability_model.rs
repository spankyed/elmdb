//! Model 2 (`durability_model`): Shuttle concurrency model over real
//! `elmdb_storage::DurabilityGate<SimKvStore, SimVfs>`.
//!
//! Verifies:
//! - Concurrent Grouped commits, `sync()`, and `grow_map()` / `adopt_resized_map()` preserve
//!   monotonic `txnid`, durable-pin protection (`pinned_txnid <= durable_txnid`), and crash safety
//!   across 3+ post-remap unsynced commits (`D+1..D+3`, triggering `SimKvStore`'s freelist-reuse check).
//! - `unacked_reboot` blocks sidecar sealing (`sync()` returns `Ok(None)`) and `copy_compacted()`
//!   across concurrent threads until `acknowledge_restore()` succeeds.
//! - Permanent fail-stop poisoning on sync failure blocks all subsequent concurrent readers,
//!   writers, and syncers with `StorageError::Poisoned`.
//! - Concurrent peer commits between `prepare_durable_pin` and `active_receipt`, plus `MapResized`
//!   and `peer_commit_on_adopt` races, retry cleanly without ever leaving `pinned > sealed`.

use std::path::Path;

use elmdb_storage::locks::model::{Arc, AtomicBool, Ordering, explore, thread};
use elmdb_storage::{BootId, DbFlags, DurabilityGate, DurabilityMode, KvRead, RecoveryStatus, StorageError};

use crate::{SimKvConfig, SimKvStore, SimRng, SimVfs};

fn fresh_grouped_gate() -> (SimKvStore, SimVfs, Arc<DurabilityGate<SimKvStore, SimVfs>>) {
    let store = SimKvStore::new(SimKvConfig::default());
    let vfs = SimVfs::new(BootId([0xA1; 16]));
    let (gate, report) = DurabilityGate::open(store.clone(), vfs.clone(), "/sim-db", DurabilityMode::Grouped)
        .expect("fresh grouped open");
    assert_eq!(report.status, RecoveryStatus::Fresh);
    (store, vfs, Arc::new(gate))
}

#[test]
fn grouped_concurrent_writes_sync_and_remap_preserve_pin_and_crash_safety() {
    explore(|| {
        let (store, vfs, gate) = fresh_grouped_gate();

        // Seed subdb and initial durable commit so Grouped mode holds a live durable pin.
        {
            let mut w = gate.begin_write().expect("seed write");
            let db = w.open_subdb("data", DbFlags::DEFAULT).expect("open subdb");
            w.put(db, b"seed", b"v0").expect("put seed");
            w.commit().expect("commit seed");
        }
        let seed_slot = gate.sync().expect("seed sync").expect("grouped slot");
        assert!(store.pinned_txnid().is_some_and(|p| p <= seed_slot.txnid));

        let t_w1 = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let mut w = gate.begin_write().expect("w1");
                let db = w.open_subdb("data", DbFlags::DEFAULT).expect("subdb");
                w.put(db, b"k1", b"v1").expect("put k1");
                w.commit().expect("commit k1")
            })
        };

        let t_sync = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || gate.sync().expect("concurrent sync"))
        };

        let t_grow = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || gate.grow_map(None).expect("concurrent grow_map"))
        };

        let r1 = t_w1.join().unwrap();
        let _s = t_sync.join().unwrap();
        let new_size = t_grow.join().unwrap();

        assert!(r1.txnid >= 2);
        assert!(new_size >= SimKvConfig::default().map_size * 2);
        // After sync + grow_map, Grouped mode must ALWAYS still hold a durable pin <= durable_txnid!
        let durable = store.durable_txnid();
        assert!(
            store.pinned_txnid().is_some_and(|p| p <= durable),
            "Grouped durable pin {:?} must be present and <= durable_txnid {durable}",
            store.pinned_txnid()
        );

        // Perform 3 unsynced commits (D+1, D+2, D+3) and simulate sudden power loss + reboot:
        // 3 unsynced commits cross SimKvStore's `freed_at + 2` freelist-reuse threshold, proving
        // the durable pin genuinely protected durable pages across concurrent sync & remap!
        for i in 0..3u8 {
            let mut w = gate.begin_write().expect("post-remap write");
            let db = w.open_subdb("data", DbFlags::DEFAULT).expect("subdb");
            w.put(db, &[b'x', i], b"unsynced").expect("put");
            w.commit().expect("commit");
        }
        let mut rng = SimRng::seeded(7);
        store.crash_power_loss();
        let _ = vfs.crash_power_loss(&mut rng, true);

        let (reopened, report) =
            DurabilityGate::open(store.clone(), vfs.clone(), "/sim-db", DurabilityMode::Grouped).expect("reopen");
        assert!(
            matches!(report.status, RecoveryStatus::RebootVerified | RecoveryStatus::Clean),
            "unexpected recovery status after pinned crash: {:?}",
            report.status
        );
        let ro = reopened.begin_read().expect("read after crash");
        let db = reopened.open_subdb_ro(&ro, "data", DbFlags::DEFAULT).expect("open ro").expect("exists");
        assert_eq!(ro.get(db, b"seed").expect("get seed").as_deref(), Some(&b"v0"[..]));
    });
}

#[test]
fn unacked_reboot_blocks_sidecar_sealing_and_backup_until_acknowledged() {
    explore(|| {
        let store = SimKvStore::new(SimKvConfig::default());
        let vfs = SimVfs::new(BootId([0x11; 16]));
        {
            let (gate, _) =
                DurabilityGate::open(store.clone(), vfs.clone(), "/sim-db", DurabilityMode::Grouped).expect("open");
            let mut w = gate.begin_write().expect("write");
            let db = w.open_subdb("data", DbFlags::DEFAULT).expect("subdb");
            w.put(db, b"a", b"1").expect("put");
            w.commit().expect("commit");
            // Intentionally skip sync and simulate reboot with a different BootId while unsynced tail remains!
        }
        let mut rng = SimRng::seeded(19);
        let _ = vfs.crash_power_loss(&mut rng, true);

        let (gate, report) =
            DurabilityGate::open(store.clone(), vfs.clone(), "/sim-db", DurabilityMode::Grouped).expect("reopen");
        assert_eq!(report.status, RecoveryStatus::UnsyncedAfterReboot);
        assert!(gate.has_unacked_reboot());
        // Before acknowledgment, backup (`copy_compacted`) MUST fail closed!
        assert!(matches!(gate.copy_compacted(Path::new("/sim-bak")), Err(StorageError::DurabilityFailed(_))));

        let gate = Arc::new(gate);

        let t_writer = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let mut w = gate.begin_write().expect("write during unacked reboot");
                let db = w.open_subdb("data", DbFlags::DEFAULT).expect("subdb");
                w.put(db, b"b", b"2").expect("put");
                w.commit().expect("commit");
            })
        };

        let t_syncer = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let slot = gate.sync().expect("sync during unacked reboot must flush without failing");
                if slot.is_some() {
                    // `sync()` may ONLY seal `synced.v2` (`Some(slot)`) once `acknowledge_restore()`
                    // has cleared `unacked_reboot`!
                    assert!(!gate.has_unacked_reboot(), "sync() sealed sidecar while unacked_reboot was still set");
                }
            })
        };

        let t_ack = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let slot = gate.acknowledge_restore().expect("acknowledge_restore");
                assert!(slot.is_some(), "acknowledge_restore in Grouped mode must seal a SyncSlot");
            })
        };

        t_writer.join().unwrap();
        t_syncer.join().unwrap();
        t_ack.join().unwrap();

        assert!(!gate.has_unacked_reboot());
        // After acknowledgment, writes and syncs must seal cleanly.
        let mut w = gate.begin_write().expect("post-ack write");
        let db = w.open_subdb("data", DbFlags::DEFAULT).expect("subdb");
        w.put(db, b"c", b"3").expect("put");
        w.commit().expect("commit");
        assert!(gate.sync().expect("post-ack sync").is_some(), "post-ack sync must seal sidecar");
    });
}

#[test]
fn sync_failure_permanently_poisons_concurrent_readers_and_writers() {
    explore(|| {
        let (store, _vfs, gate) = fresh_grouped_gate();
        {
            let mut w = gate.begin_write().expect("seed");
            let db = w.open_subdb("data", DbFlags::DEFAULT).expect("subdb");
            w.put(db, b"k", b"v").expect("put");
            w.commit().expect("commit");
        }
        store.inject_sync_failures(1);

        let sync_failed = Arc::new(AtomicBool::new(false));

        let t_sync = {
            let gate = Arc::clone(&gate);
            let sync_failed = Arc::clone(&sync_failed);
            thread::spawn(move || {
                let err = gate.sync().expect_err("injected sync failure must fail");
                assert!(matches!(err, StorageError::DurabilityFailed(_) | StorageError::Poisoned(_)));
                sync_failed.store(true, Ordering::SeqCst);
            })
        };

        let t_reader = {
            let gate = Arc::clone(&gate);
            let sync_failed = Arc::clone(&sync_failed);
            thread::spawn(move || {
                let already_poisoned = sync_failed.load(Ordering::SeqCst);
                match gate.begin_read() {
                    Ok(_ro) => {
                        assert!(!already_poisoned, "reader begun AFTER sync failure poisoned gate must fail closed");
                    }
                    Err(StorageError::Poisoned(_)) => {}
                    Err(other) => panic!("unexpected reader error: {other:?}"),
                }
            })
        };

        let t_writer = {
            let gate = Arc::clone(&gate);
            let sync_failed = Arc::clone(&sync_failed);
            thread::spawn(move || {
                let already_poisoned = sync_failed.load(Ordering::SeqCst);
                match gate.begin_write() {
                    Ok(mut w) => {
                        assert!(!already_poisoned, "writer begun AFTER sync failure poisoned gate must fail closed");
                        let db = w.open_subdb("data", DbFlags::DEFAULT).expect("subdb");
                        w.put(db, b"k2", b"v2").expect("put");
                        match w.commit() {
                            Ok(_) | Err(StorageError::Poisoned(_)) => {}
                            Err(other) => panic!("unexpected commit error: {other:?}"),
                        }
                    }
                    Err(StorageError::Poisoned(_)) => {}
                    Err(other) => panic!("unexpected writer error: {other:?}"),
                }
            })
        };

        t_sync.join().unwrap();
        t_reader.join().unwrap();
        t_writer.join().unwrap();

        assert!(gate.is_poisoned(), "gate must stay permanently poisoned");
        assert!(matches!(gate.begin_read(), Err(StorageError::Poisoned(_))));
        assert!(matches!(gate.begin_write(), Err(StorageError::Poisoned(_))));
        assert!(matches!(gate.sync(), Err(StorageError::Poisoned(_))));
    });
}

#[test]
fn grouped_pin_race_and_map_resized_retries_safely_under_concurrency() {
    explore(|| {
        let (store, _vfs, gate) = fresh_grouped_gate();
        {
            let mut w = gate.begin_write().expect("seed");
            let db = w.open_subdb("data", DbFlags::DEFAULT).expect("subdb");
            w.put(db, b"k0", b"v0").expect("put");
            w.commit().expect("commit");
        }
        // Phase 1: Inject ONLY a peer commit between `prepare_durable_pin` (P) and `active_receipt` (S)
        // so `persist_durable_locked` genuinely exercises the `P <= S` pin-before-receipt path!
        store.inject_peer_commit_on_receipt(1);

        let t_sync = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || gate.sync().expect("sync must seal P <= S cleanly"))
        };

        let t_writer = {
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                let mut w = gate.begin_write().expect("concurrent write");
                let db = w.open_subdb("data", DbFlags::DEFAULT).expect("subdb");
                w.put(db, b"k1", b"v1").expect("put");
                w.commit().expect("commit")
            })
        };

        let slot = t_sync.join().unwrap().expect("grouped sync slot");
        let _ = t_writer.join().unwrap();

        let pinned = store.pinned_txnid().expect("pin must be installed after sync");
        assert!(pinned <= slot.txnid, "pin {pinned} must never exceed sealed txnid {}", slot.txnid);

        // Phase 2: Inject `MapResized` on pin + peer commit on adopt, proving `adopt_and_pin_grouped`
        // retries cleanly and still leaves `pinned <= sealed`!
        store.inject_pin_map_resized(1);
        store.inject_peer_commit_on_adopt(1);
        let slot2 = gate.sync().expect("sync after MapResized + peer commit on adopt").expect("slot2");
        let pinned2 = store.pinned_txnid().expect("pin after adopt");
        assert!(pinned2 <= slot2.txnid, "pin {pinned2} must never exceed sealed txnid {}", slot2.txnid);
    });
}
