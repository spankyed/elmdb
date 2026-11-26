use elmdb_lmdb::{LmdbConfig, LmdbStore};
use elmdb_sim::{SimKvConfig, SimKvStore};
use elmdb_storage::{
    DbFlags, DurabilityGate, DurabilityMode, KvCursor, KvRead, KvStore, KvWrite, OsVfs, RecoveryStatus, SubDbHandle,
    SubDbPool,
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
    }
    // Verify reverse iteration is exact mirror.
    let mut rev = Vec::new();
    if let Some((k, v)) = cur.last().unwrap() {
        rev.push((k.to_vec(), v.to_vec()));
        while let Some((k, v)) = cur.prev().unwrap() {
            rev.push((k.to_vec(), v.to_vec()));
        }
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

        // Initialize one byte-keyed sub-DB and one MDB_INTEGERKEY sub-DB in both backends.
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
                    // Use non-monotonic native-endian u64 values (including > 255) to exercise integer sorting!
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

                    // Verify committed read-only views match bit-for-bit.
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

        // Clean final commit & check.
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
fn lmdb_ping_pong_subdb_pool_never_hits_dbs_full_and_preserves_mvcc() {
    let tmp = TempDir::new().unwrap();
    // Deliberately tiny max_dbs = 8 to prove 500 ping-pong cycles never exhaust DBIs!
    let lmdb =
        LmdbStore::open(tmp.path(), LmdbConfig { map_size: 8 * 1024 * 1024, max_dbs: 8, ..LmdbConfig::default() })
            .unwrap();
    let gate = DurabilityGate::new(lmdb, OsVfs::new(), tmp.path(), DurabilityMode::Strict);

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
        // Hold an active MVCC reader across the ping-pong flip + truncate!
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

        // Old pinned MVCC reader STILL sees pre-truncate value!
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

    // Open a new sub-DB and abort -> must NOT leak into DBI cache!
    {
        let mut tx = lmdb.begin_write().unwrap();
        let db = tx.open_subdb("ephemeral", DbFlags::DEFAULT).unwrap();
        tx.put(db, b"a", b"1").unwrap();
        tx.abort();
    }

    let ro = lmdb.begin_read().unwrap();
    assert_eq!(lmdb.open_subdb_ro(&ro, "ephemeral", DbFlags::DEFAULT).unwrap(), None);
    drop(ro);

    // Subsequent committed creation succeeds cleanly.
    let mut tx = lmdb.begin_write().unwrap();
    let db = tx.open_subdb("ephemeral", DbFlags::DEFAULT).unwrap();
    tx.put(db, b"a", b"2").unwrap();
    tx.commit_raw().unwrap();

    let ro = lmdb.begin_read().unwrap();
    let db_ro = lmdb.open_subdb_ro(&ro, "ephemeral", DbFlags::DEFAULT).unwrap().unwrap();
    assert_eq!(ro.get(db_ro, b"a").unwrap(), Some(b"2".as_slice()));
}
