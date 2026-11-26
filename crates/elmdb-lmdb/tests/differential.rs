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
            let mut tx = lmdb.begin_write().unwrap();
            let b = tx.open_subdb("bytes", DbFlags::DEFAULT).unwrap();
            let i = tx.open_subdb("ints", DbFlags::INTEGER_KEY).unwrap();
            tx.commit_raw().unwrap();
            (b, i)
        };
        let (s_bytes, s_ints) = {
            let mut tx = sim.begin_write().unwrap();
            let b = tx.open_subdb("bytes", DbFlags::DEFAULT).unwrap();
            let i = tx.open_subdb("ints", DbFlags::INTEGER_KEY).unwrap();
            tx.commit_raw().unwrap();
            (b, i)
        };

        let mut l_tx = lmdb.begin_write().unwrap();
        let mut s_tx = sim.begin_write().unwrap();

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
                    l_tx.commit_raw().unwrap();
                    s_tx.commit_raw().unwrap();

                    let l_ro = lmdb.begin_read().unwrap();
                    let s_ro = sim.begin_read().unwrap();
                    prop_assert_eq!(collect_all(&l_ro, l_bytes), collect_all(&s_ro, s_bytes));
                    prop_assert_eq!(collect_all(&l_ro, l_ints), collect_all(&s_ro, s_ints));
                    drop(l_ro);
                    drop(s_ro);

                    l_tx = lmdb.begin_write().unwrap();
                    s_tx = sim.begin_write().unwrap();
                }
                Op::AbortTxn => {
                    l_tx.abort();
                    s_tx.abort();
                    l_tx = lmdb.begin_write().unwrap();
                    s_tx = sim.begin_write().unwrap();
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
        l_tx.commit_raw().unwrap();
        s_tx.commit_raw().unwrap();

        let l_ro = lmdb.begin_read().unwrap();
        let s_ro = sim.begin_read().unwrap();
        prop_assert_eq!(collect_all(&l_ro, l_bytes), collect_all(&s_ro, s_bytes));
        prop_assert_eq!(collect_all(&l_ro, l_ints), collect_all(&s_ro, s_ints));
    }
}

#[test]
fn lmdb_empty_values_and_fixed_8_byte_int_keys_parity() {
    fn exercise<S: KvStore>(store: &S) {
        let mut tx = store.begin_write().unwrap();
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
        tx.commit_raw().unwrap();
    }

    let tmp = TempDir::new().unwrap();
    let lmdb = LmdbStore::open(tmp.path(), LmdbConfig { map_size: 4 * 1024 * 1024, ..LmdbConfig::default() }).unwrap();
    let sim = SimKvStore::default();
    exercise(&lmdb);
    exercise(&sim);

    // Empty write txn parity: txnid must NOT increment on no-op commit!
    let l_before = lmdb.last_txnid().unwrap();
    let s_before = sim.last_txnid().unwrap();
    lmdb.begin_write().unwrap().commit_raw().unwrap();
    sim.begin_write().unwrap().commit_raw().unwrap();
    assert_eq!(lmdb.last_txnid().unwrap(), l_before);
    assert_eq!(sim.last_txnid().unwrap(), s_before);
}

#[test]
fn lmdb_reopen_preopens_subdbs_safely_across_ro_drops_threads_and_flag_checks() {
    let tmp = TempDir::new().unwrap();
    let cfg = LmdbConfig { map_size: 4 * 1024 * 1024, ..LmdbConfig::default() };

    {
        let lmdb = LmdbStore::open(tmp.path(), cfg.clone()).unwrap();
        let mut tx = lmdb.begin_write().unwrap();
        let b = tx.open_subdb("bytes", DbFlags::DEFAULT).unwrap();
        let i = tx.open_subdb("ints", DbFlags::INTEGER_KEY).unwrap();
        tx.put(b, b"k", b"v").unwrap();
        tx.put(i, &42u64.to_ne_bytes(), b"row42").unwrap();
        tx.commit_raw().unwrap();
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
        let mut rw = lmdb.begin_write().unwrap();
        assert!(matches!(rw.open_subdb("ints", DbFlags::DEFAULT), Err(StorageError::InvalidState(_))));
        rw.abort();
    }

    // 3. MVCC snapshot visibility: sub-DB created AFTER `ro_old` started returns None on `ro_old`!
    let ro_old = lmdb.begin_read().unwrap();
    {
        let mut rw = lmdb.begin_write().unwrap();
        let late = rw.open_subdb("late_db", DbFlags::DEFAULT).unwrap();
        rw.put(late, b"x", b"1").unwrap();
        rw.commit_raw().unwrap();
    }
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
        let staging_db = {
            let mut tx = gate.begin_write().unwrap();
            let db = tx.open_subdb(&staging_slot.name(), DbFlags::DEFAULT).unwrap();
            let cur_epoch = format!("{epoch}");
            tx.put(db, b"gen", cur_epoch.as_bytes()).unwrap();
            tx.truncate(active_db).unwrap();
            tx.commit().unwrap();
            db
        };

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
        let mut tx = lmdb.begin_write().unwrap();
        let db = tx.open_subdb("ephemeral", DbFlags::DEFAULT).unwrap();
        tx.put(db, b"a", b"1").unwrap();
        tx.abort();
    }

    let ro = lmdb.begin_read().unwrap();
    assert_eq!(lmdb.open_subdb_ro(&ro, "ephemeral", DbFlags::DEFAULT).unwrap(), None);
    drop(ro);

    let mut tx = lmdb.begin_write().unwrap();
    let db = tx.open_subdb("ephemeral", DbFlags::DEFAULT).unwrap();
    tx.put(db, b"a", b"2").unwrap();
    tx.commit_raw().unwrap();

    let ro = lmdb.begin_read().unwrap();
    let db_ro = lmdb.open_subdb_ro(&ro, "ephemeral", DbFlags::DEFAULT).unwrap().unwrap();
    assert_eq!(ro.get(db_ro, b"a").unwrap(), Some(b"2".as_slice()));
}
