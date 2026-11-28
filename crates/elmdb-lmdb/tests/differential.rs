use std::sync::Arc;
use std::thread;

use elmdb_lmdb::{LmdbConfig, LmdbStore};
use elmdb_sim::{SimKvConfig, SimKvStore};
use elmdb_storage::{
    DbFlags, DurabilityGate, DurabilityMode, KvCursor, KvRead, KvStore, KvWrite, OsVfs, RecoveryStatus, StorageError,
    SubDbHandle, SubDbPool,
};
use proptest::prelude::*;
use tempfile::TempDir;

#[derive(Debug, Clone)]
enum Op {
    PutBytes(u8, Vec<u8>),
    PutNoOverwriteBytes(u8, Vec<u8>),
    DelBytes(u8),
    PutInt(u64, Vec<u8>),
    DelInt(u64),
    BeginSp,
    CommitSp,
    AbortSp,
    TruncateBytes,
    CommitTxn,
    AbortTxn,
    CheckSeekBytes(u8),
    CheckSeekInt(u64),
}

fn arb_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (0u8..24, prop::collection::vec(any::<u8>(), 0..32)).prop_map(|(k, v)| Op::PutBytes(k, v)),
        3 => (0u8..24, prop::collection::vec(any::<u8>(), 0..16)).prop_map(|(k, v)| Op::PutNoOverwriteBytes(k, v)),
        3 => (0u8..24).prop_map(Op::DelBytes),
        5 => (0u64..32, prop::collection::vec(any::<u8>(), 0..16)).prop_map(|(k, v)| Op::PutInt(k, v)),
        2 => (0u64..32).prop_map(Op::DelInt),
        2 => Just(Op::BeginSp),
        2 => Just(Op::CommitSp),
        2 => Just(Op::AbortSp),
        1 => Just(Op::TruncateBytes),
        3 => Just(Op::CommitTxn),
        1 => Just(Op::AbortTxn),
        2 => (0u8..26).prop_map(Op::CheckSeekBytes),
        2 => (0u64..34).prop_map(Op::CheckSeekInt),
    ]
}

fn collect_all<R: KvRead>(txn: &R, db: SubDbHandle) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut cur = txn.cursor(db).unwrap();
    let mut out = Vec::new();
    if let Some((k, v)) = cur.first().unwrap() {
        out.push((k.to_vec(), v.to_vec()));
        while let Some((k, v)) = cur.next().unwrap() {
            out.push((k.to_vec(), v.to_vec()));
        }
        // Calling next() again at EOF must stay None (no silent wrap-around).
        assert_eq!(cur.next().unwrap(), None);
    }
    // Verify reverse iteration is exact mirror.
    let mut rev = Vec::new();
    if let Some((k, v)) = cur.last().unwrap() {
        rev.push((k.to_vec(), v.to_vec()));
        while let Some((k, v)) = cur.prev().unwrap() {
            rev.push((k.to_vec(), v.to_vec()));
        }
        // Calling prev() again at BOF must stay None (no silent wrap-around).
        assert_eq!(cur.prev().unwrap(), None);
    }
    rev.reverse();
    assert_eq!(out, rev, "reverse cursor iteration diverged from forward iteration");
    assert_eq!(out.len() as u64, txn.entries(db).unwrap());
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn lmdb_matches_sim_kv_bit_for_bit(ops in prop::collection::vec(arb_op(), 1..90)) {
        let tmp = TempDir::new().unwrap();
        let lmdb = LmdbStore::open(
            tmp.path(),
            LmdbConfig { map_size: 8 * 1024 * 1024, ..LmdbConfig::default() },
        ).unwrap();
        let sim = SimKvStore::new(SimKvConfig::default());

        let (l_bytes, l_ints) = {
            let mut tx = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
            let b = tx.open_subdb("bytes", DbFlags::DEFAULT).unwrap();
            let i = tx.open_subdb("ints", DbFlags::INTEGER_KEY).unwrap();
            let (_, _g) = tx.commit_raw().unwrap();
            (b, i)
        };
        let (s_bytes, s_ints) = {
            let mut tx = sim.begin_write(()).unwrap();
            let b = tx.open_subdb("bytes", DbFlags::DEFAULT).unwrap();
            let i = tx.open_subdb("ints", DbFlags::INTEGER_KEY).unwrap();
            let (_, _g) = tx.commit_raw().unwrap();
            (b, i)
        };

        let mut l_tx = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
        let mut s_tx = sim.begin_write(()).unwrap();

        for op in ops {
            match op {
                Op::PutBytes(k, v) => {
                    let key = [b'k', k];
                    l_tx.put(l_bytes, &key, &v).unwrap();
                    s_tx.put(s_bytes, &key, &v).unwrap();
                }
                Op::PutNoOverwriteBytes(k, v) => {
                    let key = [b'k', k];
                    let lr = l_tx.put_no_overwrite(l_bytes, &key, &v).unwrap();
                    let sr = s_tx.put_no_overwrite(s_bytes, &key, &v).unwrap();
                    prop_assert_eq!(lr, sr);
                }
                Op::DelBytes(k) => {
                    let key = [b'k', k];
                    let lr = l_tx.del(l_bytes, &key).unwrap();
                    let sr = s_tx.del(s_bytes, &key).unwrap();
                    prop_assert_eq!(lr, sr);
                }
                Op::PutInt(k, v) => {
                    let key = (k * 1000).to_ne_bytes();
                    l_tx.put(l_ints, &key, &v).unwrap();
                    s_tx.put(s_ints, &key, &v).unwrap();
                }
                Op::DelInt(k) => {
                    let key = (k * 1000).to_ne_bytes();
                    let lr = l_tx.del(l_ints, &key).unwrap();
                    let sr = s_tx.del(s_ints, &key).unwrap();
                    prop_assert_eq!(lr, sr);
                }
                Op::BeginSp => {
                    if l_tx.savepoint_depth() < 4 {
                        let ld = l_tx.begin_savepoint().unwrap();
                        let sd = s_tx.begin_savepoint().unwrap();
                        prop_assert_eq!(ld, sd);
                    }
                }
                Op::CommitSp => {
                    let depth = l_tx.savepoint_depth();
                    prop_assert_eq!(depth, s_tx.savepoint_depth());
                    if depth > 0 {
                        l_tx.commit_savepoint(depth).unwrap();
                        s_tx.commit_savepoint(depth).unwrap();
                    }
                }
                Op::AbortSp => {
                    let depth = l_tx.savepoint_depth();
                    prop_assert_eq!(depth, s_tx.savepoint_depth());
                    if depth > 0 {
                        l_tx.abort_savepoint(depth).unwrap();
                        s_tx.abort_savepoint(depth).unwrap();
                    }
                }
                Op::TruncateBytes => {
                    l_tx.truncate(l_bytes).unwrap();
                    s_tx.truncate(s_bytes).unwrap();
                }
                Op::CommitTxn => {
                    while l_tx.savepoint_depth() > 0 {
                        let d = l_tx.savepoint_depth();
                        l_tx.commit_savepoint(d).unwrap();
                        s_tx.commit_savepoint(d).unwrap();
                    }
                    let (_, g_l) = l_tx.commit_raw().unwrap();
                    drop(g_l);
                    let _ = s_tx.commit_raw().unwrap();

                    let l_ro = lmdb.begin_read().unwrap();
                    let s_ro = sim.begin_read().unwrap();
                    prop_assert_eq!(collect_all(&l_ro, l_bytes), collect_all(&s_ro, s_bytes));
                    prop_assert_eq!(collect_all(&l_ro, l_ints), collect_all(&s_ro, s_ints));
                    drop(l_ro);
                    drop(s_ro);

                    l_tx = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
                    s_tx = sim.begin_write(()).unwrap();
                }
                Op::AbortTxn => {
                    l_tx.abort();
                    s_tx.abort();
                    l_tx = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
                    s_tx = sim.begin_write(()).unwrap();
                }
                Op::CheckSeekBytes(k) => {
                    let key = [b'k', k];
                    prop_assert_eq!(
                        l_tx.get(l_bytes, &key).unwrap(),
                        s_tx.get(s_bytes, &key).unwrap(),
                    );
                    let mut lc = l_tx.cursor(l_bytes).unwrap();
                    let mut sc = s_tx.cursor(s_bytes).unwrap();
                    prop_assert_eq!(
                        lc.seek_ge(&key).unwrap().map(|(a, b)| (a.to_vec(), b.to_vec())),
                        sc.seek_ge(&key).unwrap().map(|(a, b)| (a.to_vec(), b.to_vec())),
                    );
                }
                Op::CheckSeekInt(k) => {
                    let key = (k * 1000).to_ne_bytes();
                    prop_assert_eq!(
                        l_tx.get(l_ints, &key).unwrap(),
                        s_tx.get(s_ints, &key).unwrap(),
                    );
                    let mut lc = l_tx.cursor(l_ints).unwrap();
                    let mut sc = s_tx.cursor(s_ints).unwrap();
                    prop_assert_eq!(
                        lc.seek_ge(&key).unwrap().map(|(a, b)| (a.to_vec(), b.to_vec())),
                        sc.seek_ge(&key).unwrap().map(|(a, b)| (a.to_vec(), b.to_vec())),
                    );
                }
            }
        }

        while l_tx.savepoint_depth() > 0 {
            let d = l_tx.savepoint_depth();
            l_tx.commit_savepoint(d).unwrap();
            s_tx.commit_savepoint(d).unwrap();
        }
        let (_, g_l) = l_tx.commit_raw().unwrap();
        drop(g_l);
        let _ = s_tx.commit_raw().unwrap();

        let l_ro = lmdb.begin_read().unwrap();
        let s_ro = sim.begin_read().unwrap();
        prop_assert_eq!(collect_all(&l_ro, l_bytes), collect_all(&s_ro, s_bytes));
        prop_assert_eq!(collect_all(&l_ro, l_ints), collect_all(&s_ro, s_ints));
    }
}

#[test]
fn lmdb_empty_values_and_fixed_8_byte_int_keys_parity() {
    fn exercise<S: KvStore>(store: &S) {
        let mut tx = store.begin_write(store.lock_map_read()).unwrap();
        let b = tx.open_subdb("bytes", DbFlags::DEFAULT).unwrap();
        let i = tx.open_subdb("ints", DbFlags::INTEGER_KEY).unwrap();
        // Empty value must succeed cleanly without C NULL memcpy UB.
        tx.put(b, b"empty", &[]).unwrap();
        assert_eq!(tx.get(b, b"empty").unwrap(), Some(&[][..]));
        // 4-byte key in INTEGER_KEY DB must fail (fixed 8-byte u64 only).
        assert!(matches!(tx.put(i, &1u32.to_ne_bytes(), b"x"), Err(StorageError::BadValSize { len: 4 })));
        // Invalid sub-DB names must fail identically.
        assert!(matches!(tx.open_subdb("", DbFlags::DEFAULT), Err(StorageError::BadValSize { len: 0 })));
        assert!(matches!(tx.open_subdb("a\0b", DbFlags::DEFAULT), Err(StorageError::InvalidState(_))));
        let (_, _g) = tx.commit_raw().unwrap();
    }

    let tmp = TempDir::new().unwrap();
    let lmdb = LmdbStore::open(tmp.path(), LmdbConfig { map_size: 4 * 1024 * 1024, ..LmdbConfig::default() }).unwrap();
    let sim = SimKvStore::default();
    exercise(&lmdb);
    exercise(&sim);

    // Empty write txn parity: txnid must NOT increment on no-op commit!
    let l_before = lmdb.last_txnid().unwrap();
    let s_before = sim.last_txnid().unwrap();
    let (_, g_l) = lmdb.begin_write(lmdb.lock_map_read()).unwrap().commit_raw().unwrap();
    drop(g_l);
    let _ = sim.begin_write(()).unwrap().commit_raw().unwrap();
    assert_eq!(lmdb.last_txnid().unwrap(), l_before);
    assert_eq!(sim.last_txnid().unwrap(), s_before);
}

#[test]
fn lmdb_reopen_preopens_subdbs_safely_across_ro_drops_threads_and_flag_checks() {
    let tmp = TempDir::new().unwrap();
    let cfg = LmdbConfig { map_size: 4 * 1024 * 1024, ..LmdbConfig::default() };

    {
        let lmdb = LmdbStore::open(tmp.path(), cfg.clone()).unwrap();
        let mut tx = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
        let b = tx.open_subdb("bytes", DbFlags::DEFAULT).unwrap();
        let i = tx.open_subdb("ints", DbFlags::INTEGER_KEY).unwrap();
        tx.put(b, b"k", b"v").unwrap();
        tx.put(i, &42u64.to_ne_bytes(), b"row42").unwrap();
        let (_, _g) = tx.commit_raw().unwrap();
    }

    // Reopen fresh LmdbStore -> cold cache!
    let lmdb = Arc::new(LmdbStore::open(tmp.path(), cfg).unwrap());

    // 1. Open RO, drop it, open a second RO, and read: DBI handle MUST stay alive!
    {
        let ro1 = lmdb.begin_read().unwrap();
        let b = lmdb.open_subdb_ro(&ro1, "bytes", DbFlags::DEFAULT).unwrap().unwrap();
        assert_eq!(ro1.get(b, b"k").unwrap(), Some(b"v".as_slice()));
    }
    {
        let ro2 = lmdb.begin_read().unwrap();
        let b = lmdb.open_subdb_ro(&ro2, "bytes", DbFlags::DEFAULT).unwrap().unwrap();
        let i = lmdb.open_subdb_ro(&ro2, "ints", DbFlags::INTEGER_KEY).unwrap().unwrap();
        assert_eq!(ro2.get(b, b"k").unwrap(), Some(b"v".as_slice()));
        assert_eq!(ro2.get(i, &42u64.to_ne_bytes()).unwrap(), Some(b"row42".as_slice()));

        // Cold-cache flag mismatch MUST be rejected!
        assert!(matches!(lmdb.open_subdb_ro(&ro2, "ints", DbFlags::DEFAULT), Err(StorageError::InvalidState(_))));
    }

    // 2. Write-side flag mismatch on existing sub-DB MUST be rejected!
    {
        let mut rw = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
        assert!(matches!(rw.open_subdb("ints", DbFlags::DEFAULT), Err(StorageError::InvalidState(_))));
        rw.abort();
    }

    // 3. MVCC snapshot visibility: sub-DB created AFTER `ro_old` started returns None on `ro_old`!
    let ro_old = lmdb.begin_read().unwrap();
    std::thread::scope(|s| {
        let lmdb_w = Arc::clone(&lmdb);
        s.spawn(move || {
            let mut rw = lmdb_w.begin_write(lmdb_w.lock_map_read()).unwrap();
            let late = rw.open_subdb("late_db", DbFlags::DEFAULT).unwrap();
            rw.put(late, b"x", b"1").unwrap();
            let (_, _g) = rw.commit_raw().unwrap();
        })
        .join()
        .unwrap();
    });
    assert_eq!(lmdb.open_subdb_ro(&ro_old, "late_db", DbFlags::DEFAULT).unwrap(), None);
    drop(ro_old);

    let ro_new = lmdb.begin_read().unwrap();
    let late = lmdb.open_subdb_ro(&ro_new, "late_db", DbFlags::DEFAULT).unwrap().unwrap();
    assert_eq!(ro_new.get(late, b"x").unwrap(), Some(b"1".as_slice()));
    drop(ro_new);

    // 4. Concurrent multi-threaded RO opens and reads must be race-free.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let store = Arc::clone(&lmdb);
        handles.push(thread::spawn(move || {
            for _ in 0..50 {
                let ro = store.begin_read().unwrap();
                let b = store.open_subdb_ro(&ro, "bytes", DbFlags::DEFAULT).unwrap().unwrap();
                let i = store.open_subdb_ro(&ro, "ints", DbFlags::INTEGER_KEY).unwrap().unwrap();
                assert_eq!(ro.get(b, b"k").unwrap(), Some(b"v".as_slice()));
                assert_eq!(ro.get(i, &42u64.to_ne_bytes()).unwrap(), Some(b"row42".as_slice()));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn lmdb_ping_pong_subdb_pool_never_hits_dbs_full_and_preserves_mvcc() {
    let tmp = TempDir::new().unwrap();
    // Deliberately tiny max_dbs = 8 to prove 500 ping-pong cycles never exhaust DBIs!
    let lmdb =
        LmdbStore::open(tmp.path(), LmdbConfig { map_size: 8 * 1024 * 1024, max_dbs: 8, ..LmdbConfig::default() })
            .unwrap();
    let (gate, _) = DurabilityGate::open(lmdb, OsVfs::new(), tmp.path(), DurabilityMode::Strict).unwrap();

    let mut pool = SubDbPool::new(4);
    let mut active_slot = pool.allocate().unwrap();
    let mut active_db = {
        let mut tx = gate.begin_write().unwrap();
        let db = tx.open_subdb(&active_slot.name(), DbFlags::DEFAULT).unwrap();
        tx.put(db, b"gen", b"0").unwrap();
        tx.commit().unwrap();
        db
    };

    for epoch in 1..=500u32 {
        let pinned_reader = gate.begin_read().unwrap();
        let prev_epoch = format!("{}", epoch - 1);
        assert_eq!(pinned_reader.get(active_db, b"gen").unwrap(), Some(prev_epoch.as_bytes()));

        let staging_slot = pool.allocate().unwrap();
        // Run the writer on a separate OS thread while `pinned_reader` stays open on this thread,
        // exercising true cross-thread MVCC isolation alongside strict non-recursive L2 `MapGate`.
        let staging_db = std::thread::scope(|s| {
            let gate_ref = &gate;
            let slot_name = staging_slot.name();
            s.spawn(move || {
                let mut tx = gate_ref.begin_write().unwrap();
                let db = tx.open_subdb(&slot_name, DbFlags::DEFAULT).unwrap();
                let cur_epoch = format!("{epoch}");
                tx.put(db, b"gen", cur_epoch.as_bytes()).unwrap();
                tx.truncate(active_db).unwrap();
                tx.commit().unwrap();
                db
            })
            .join()
            .unwrap()
        });

        assert_eq!(pinned_reader.get(active_db, b"gen").unwrap(), Some(prev_epoch.as_bytes()));
        drop(pinned_reader);

        pool.recycle(active_slot).unwrap();
        active_slot = staging_slot;
        active_db = staging_db;
    }

    assert_eq!(pool.high_water(), 2);
    let report = gate.recover().unwrap();
    assert_eq!(report.status, RecoveryStatus::Clean);
}

#[test]
fn lmdb_aborted_subdb_creation_does_not_poison_dbi_cache() {
    let tmp = TempDir::new().unwrap();
    let lmdb = LmdbStore::open(tmp.path(), LmdbConfig { map_size: 4 * 1024 * 1024, ..LmdbConfig::default() }).unwrap();

    {
        let mut tx = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
        let db = tx.open_subdb("ephemeral", DbFlags::DEFAULT).unwrap();
        tx.put(db, b"a", b"1").unwrap();
        tx.abort();
    }

    let ro = lmdb.begin_read().unwrap();
    assert_eq!(lmdb.open_subdb_ro(&ro, "ephemeral", DbFlags::DEFAULT).unwrap(), None);
    drop(ro);

    let mut tx = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
    let db = tx.open_subdb("ephemeral", DbFlags::DEFAULT).unwrap();
    tx.put(db, b"a", b"2").unwrap();
    let (_, g) = tx.commit_raw().unwrap();
    drop(g);

    let ro = lmdb.begin_read().unwrap();
    let db_ro = lmdb.open_subdb_ro(&ro, "ephemeral", DbFlags::DEFAULT).unwrap().unwrap();
    assert_eq!(ro.get(db_ro, b"a").unwrap(), Some(b"2".as_slice()));
}

#[test]
#[ignore = "child process helper invoked by lmdb_batch2_single_open_boot_id_truncation_reader_check_and_zero_hash_raw_commit"]
fn child_print_boot_id_and_read_helper() {
    let dir = std::path::PathBuf::from(std::env::var("ELMDB_TEST_LMDB_DIR").expect("ELMDB_TEST_LMDB_DIR"));
    let bid = elmdb_lmdb::platform_boot_id();
    let hex: String = bid.0.iter().map(|b| format!("{b:02x}")).collect();
    println!("BOOT_ID={hex}");
    let lmdb = LmdbStore::open(&dir, LmdbConfig { max_readers: 4, ..LmdbConfig::default() }).unwrap();
    let ro = lmdb.begin_read().unwrap();
    let db = lmdb.open_subdb_ro(&ro, "data", DbFlags::DEFAULT).unwrap().unwrap();
    assert_eq!(ro.get(db, b"hello").unwrap(), Some(b"world".as_slice()));
}

#[test]
fn lmdb_batch2_single_open_boot_id_truncation_reader_check_and_zero_hash_raw_commit() {
    use elmdb_storage::BootId;

    let tmp = TempDir::new().unwrap();
    let lmdb =
        LmdbStore::open(tmp.path(), LmdbConfig { map_size: 4 * 1024 * 1024, max_readers: 4, ..LmdbConfig::default() })
            .unwrap();

    // P0-5: Second in-process open of the same directory MUST fail with DatabaseLocked.
    assert!(matches!(LmdbStore::open(tmp.path(), LmdbConfig::default()), Err(StorageError::DatabaseLocked(_))));

    // P2-3: `commit_raw` returns exact `txnid` from RAM with zero SHA-256 (`[0u8; 32]`),
    // while `active_receipt()` returns the matching `txnid` and real non-zero `roots_sha256`.
    let mut tx = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
    let db = tx.open_subdb("data", DbFlags::DEFAULT).unwrap();
    tx.put(db, b"hello", b"world").unwrap();
    for i in 0..40u32 {
        let key = format!("pad-{i:04}");
        let val = vec![0xABu8; 2048];
        tx.put(db, key.as_bytes(), &val).unwrap();
    }
    let (raw_receipt, g) = tx.commit_raw().unwrap();
    drop(g);
    assert!(raw_receipt.txnid >= 1);
    assert_eq!(raw_receipt.roots_sha256, [0u8; 32], "commit_raw must not hash data.mdb on Grouped/Unsafe commits");
    let full_receipt = lmdb.active_receipt().unwrap();
    assert_eq!(full_receipt.txnid, raw_receipt.txnid);
    assert_ne!(full_receipt.roots_sha256, [0u8; 32], "active_receipt must compute real roots_sha256");

    // P0-9 + P1-1: Real child processes return identical non-zero OS BootId and never exhaust tiny `max_readers = 4`.
    let parent_bid = elmdb_lmdb::platform_boot_id();
    assert_ne!(parent_bid, BootId([0u8; 16]), "OS boot_id must be non-zero");
    let parent_hex: String = parent_bid.0.iter().map(|b| format!("{b:02x}")).collect();
    for _ in 0..6 {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child_print_boot_id_and_read_helper", "--ignored", "--nocapture"])
            .env("ELMDB_TEST_LMDB_DIR", tmp.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "child helper failed: {}", String::from_utf8_lossy(&out.stderr));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let child_hex = stdout.lines().find_map(|l| l.strip_prefix("BOOT_ID=")).expect("child must print BOOT_ID");
        assert_eq!(child_hex, parent_hex, "platform_boot_id() must be stable across independent OS processes");
    }

    // Drop `lmdb` -> single-open lock released AFTER mdb_env_close -> immediate reopen works!
    drop(lmdb);
    let reopened = LmdbStore::open(tmp.path(), LmdbConfig::default()).unwrap();
    let page_size = reopened.page_size();
    drop(reopened);

    // P2-1: Truncating `data.mdb` to 2 pages (keeping valid meta pages 0 & 1 intact, but below `meta.last_pg`)
    // MUST be rejected with `StorageError::Corrupted` BEFORE `mdb_env_open` (no SIGBUS!).
    let data_path = tmp.path().join("data.mdb");
    let orig_len = std::fs::metadata(&data_path).unwrap().len();
    assert!(orig_len > (page_size as u64) * 3, "expected data.mdb > 3 pages, got {orig_len}");
    std::fs::OpenOptions::new().write(true).open(&data_path).unwrap().set_len((page_size as u64) * 2).unwrap();
    let trunc_err = LmdbStore::open(tmp.path(), LmdbConfig::default()).err().expect("truncated data.mdb must fail");
    assert!(
        matches!(trunc_err, StorageError::Corrupted(_)),
        "expected StorageError::Corrupted on truncated data.mdb, got {trunc_err:?}"
    );
}

#[test]
fn lmdb_batch2_review_stress_concurrent_grow_sync_dbi_commit_thundering_herd_and_reopen_mapsize() {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};

    // 1. P0-1 + P1-3: Concurrent `grow_map(Some(target))` thundering herd + `sync()` / `last_txnid()` / `needs_map_growth()`
    //    must NEVER SIGSEGV or exponentially over-grow past `target`!
    let tmp = TempDir::new().unwrap();
    let initial_map = 256 * 1024;
    let lmdb = LmdbStore::open(tmp.path(), LmdbConfig { map_size: initial_map, ..LmdbConfig::default() }).unwrap();
    let (gate, _) = DurabilityGate::open(lmdb.clone(), OsVfs::new(), tmp.path(), DurabilityMode::Grouped).unwrap();
    let gate = Arc::new(gate);

    {
        let mut tx = gate.begin_write().unwrap();
        let db = tx.open_subdb("seed", DbFlags::DEFAULT).unwrap();
        tx.put(db, b"k", b"v").unwrap();
        tx.commit().unwrap();
    }

    // 8 threads race `grow_map(Some(1 MiB))` while another thread hammers `sync()` + `last_txnid()` + `needs_map_growth()`.
    let target_1m = 1024 * 1024;
    let barrier = Arc::new(Barrier::new(9));
    let done = Arc::new(AtomicBool::new(false));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let g = Arc::clone(&gate);
        let b = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            b.wait();
            g.grow_map(Some(target_1m)).unwrap()
        }));
    }
    {
        let g = Arc::clone(&gate);
        let s = lmdb.clone();
        let b = Arc::clone(&barrier);
        let d = Arc::clone(&done);
        handles.push(std::thread::spawn(move || {
            b.wait();
            while !d.load(Ordering::Relaxed) {
                let _ = g.sync().unwrap();
                let _ = g.last_txnid().unwrap();
                let _ = s.needs_map_growth().unwrap();
            }
            0
        }));
    }
    let mut grown_sizes = Vec::new();
    for h in handles.drain(..8) {
        grown_sizes.push(h.join().unwrap());
    }
    done.store(true, Ordering::Relaxed);
    handles.pop().unwrap().join().unwrap();
    assert!(
        grown_sizes.iter().all(|&sz| sz == target_1m),
        "thundering herd must grow once to 1 MiB, got {grown_sizes:?}"
    );
    assert_eq!(gate.map_size(), target_1m);

    // 2. P0-2: Tight-loop concurrent `LmdbRoTxn::begin()` + `open_subdb_existing()` + `get()`
    //    vs `LmdbRwTxn::open_subdb()` + `commit_raw()` across 120 brand-new sub-DBs must NEVER hit EINVAL!
    let stop_readers = Arc::new(AtomicBool::new(false));
    let mut reader_threads = Vec::new();
    for _ in 0..4 {
        let s = lmdb.clone();
        let stop = Arc::clone(&stop_readers);
        reader_threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let ro = s.begin_read().unwrap();
                for i in (0..120u32).rev() {
                    let name = format!("dyn_db_{i}");
                    if let Some(db) = s.open_subdb_ro(&ro, &name, DbFlags::DEFAULT).unwrap() {
                        assert_eq!(ro.get(db, b"v").unwrap(), Some(name.as_bytes()));
                        break;
                    }
                }
            }
        }));
    }
    for i in 0..120u32 {
        let name = format!("dyn_db_{i}");
        let mut tx = gate.begin_write().unwrap();
        let db = tx.open_subdb(&name, DbFlags::DEFAULT).unwrap();
        tx.put(db, b"v", name.as_bytes()).unwrap();
        tx.commit().unwrap();
    }
    stop_readers.store(true, Ordering::Release);
    for h in reader_threads {
        h.join().unwrap();
    }

    // 3. P1-2: Reopening the 1 MiB grown DB with a smaller requested `map_size` (256 KiB) MUST preserve
    //    the on-disk 1 MiB `mm_mapsize` instead of shrinking it!
    gate.sync().unwrap();
    drop(gate);
    drop(lmdb);
    let reopened = LmdbStore::open(tmp.path(), LmdbConfig { map_size: initial_map, ..LmdbConfig::default() }).unwrap();
    assert!(
        reopened.map_size() >= target_1m,
        "reopen with smaller map_size must preserve on-disk mm_mapsize >= {target_1m}, got {}",
        reopened.map_size()
    );
}
