//! Differential fidelity suite verifying `SimKvStore` + `SimVfs` matches real `LmdbStore` + `OsVfs`
//! across all four critical storage/durability invariants:
//! 1. Multi-snapshot MVCC reader visibility across interleaved puts/overwrites/deletes/truncates.
//! 2. Freelist page reuse: Grouped durable pin keeps synced pages intact on disk across `D+1..D+10`
//!    overwrites, while unpinned commits overwrite `D`'s pages (matching `SimKvStore`'s corruption model).
//! 3. External `MAP_RESIZED` adoption on `begin_read` and `begin_write`.
//! 4. Crash & reboot `DurabilityGate` `RecoveryStatus` transitions (`Fresh`, `Clean`,
//!    `SameBootUnsyncedTail`, `RebootVerified`, `UnsyncedAfterReboot` + `acknowledge_restore()`).

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use elmdb_lmdb::{LmdbConfig, LmdbStore};
use elmdb_sim::{SimKvConfig, SimKvStore, SimVfs};
use elmdb_storage::{
    BootId, DbFlags, DurabilityGate, DurabilityMode, KvCursor, KvRead, KvStore, KvWrite, OsVfs, RecoveryStatus,
    StorageError, Vfs, VfsLockGuard,
};
use tempfile::TempDir;

fn payload(tag: u8, idx: usize, len: usize) -> Vec<u8> {
    let mut state = (tag as u32).wrapping_mul(0x9e37_79b9) ^ (idx as u32).wrapping_mul(2_654_435_761);
    (0..len)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 24) as u8
        })
        .collect()
}

/// Thin `OsVfs` wrapper with a controllable `BootId` so real `LmdbStore` tests can deterministically
/// simulate OS reboots alongside `SimVfs`.
#[derive(Clone)]
struct BootSwitchOsVfs {
    inner: OsVfs,
    boot_gen: Arc<AtomicU8>,
}

impl BootSwitchOsVfs {
    fn new(gen_id: u8) -> Self {
        Self { inner: OsVfs::new(), boot_gen: Arc::new(AtomicU8::new(gen_id)) }
    }

    fn set_boot_gen(&self, gen_id: u8) {
        self.boot_gen.store(gen_id, Ordering::SeqCst);
    }
}

impl Vfs for BootSwitchOsVfs {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>, StorageError> {
        self.inner.read_file(path)
    }
    fn read_at(&self, path: &Path, offset: u64, len: usize) -> Result<Vec<u8>, StorageError> {
        self.inner.read_at(path, offset, len)
    }
    fn file_len(&self, path: &Path) -> Result<u64, StorageError> {
        self.inner.file_len(path)
    }
    fn write_all(&self, path: &Path, data: &[u8]) -> Result<(), StorageError> {
        self.inner.write_all(path, data)
    }
    fn write_at(&self, path: &Path, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        self.inner.write_at(path, offset, data)
    }
    fn sync_file(&self, path: &Path) -> Result<(), StorageError> {
        self.inner.sync_file(path)
    }
    fn sync_dir(&self, dir: &Path) -> Result<(), StorageError> {
        self.inner.sync_dir(dir)
    }
    fn rename(&self, from: &Path, to: &Path) -> Result<(), StorageError> {
        self.inner.rename(from, to)
    }
    fn remove_file(&self, path: &Path) -> Result<(), StorageError> {
        self.inner.remove_file(path)
    }
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
    fn boot_id(&self) -> BootId {
        BootId([self.boot_gen.load(Ordering::SeqCst); 16])
    }
    fn lock_file(&self, path: &Path) -> Result<VfsLockGuard, StorageError> {
        self.inner.lock_file(path)
    }
    fn lock_file_shared_if_exists(&self, path: &Path) -> Result<Option<VfsLockGuard>, StorageError> {
        self.inner.lock_file_shared_if_exists(path)
    }
}

fn collect_all<R: KvRead>(ro: &R, db: elmdb_storage::SubDbHandle) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut cursor = ro.cursor(db).unwrap();
    let mut cur = cursor.first().unwrap();
    while let Some((k, v)) = cur {
        out.push((k.to_vec(), v.to_vec()));
        cur = cursor.next().unwrap();
    }
    out
}

#[test]
fn sim_and_lmdb_reader_snapshot_visibility_matches() {
    let dir = TempDir::new().unwrap();
    let lmdb = LmdbStore::open(dir.path(), LmdbConfig { map_size: 8 * 1024 * 1024, ..LmdbConfig::default() }).unwrap();
    let sim = SimKvStore::new(SimKvConfig { map_size: 8 * 1024 * 1024, ..SimKvConfig::default() });

    let (l_db, s_db) = {
        let mut ltx = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
        let l_db = ltx.open_subdb("items", DbFlags::DEFAULT).unwrap();
        ltx.put(l_db, b"a", b"v1").unwrap();
        ltx.put(l_db, b"b", b"v1").unwrap();
        let _ = ltx.commit_raw().unwrap();

        let mut stx = sim.begin_write(sim.lock_map_read()).unwrap();
        let s_db = stx.open_subdb("items", DbFlags::DEFAULT).unwrap();
        stx.put(s_db, b"a", b"v1").unwrap();
        stx.put(s_db, b"b", b"v1").unwrap();
        let _ = stx.commit_raw().unwrap();
        (l_db, s_db)
    };

    // Capture readers at T1 on worker threads (so `LmdbRoTxn`'s non-recursive L2 MapGate allows
    // simultaneous readers + writers, matching real concurrent usage).
    let (tx1, rx1) = std::sync::mpsc::channel();
    let (go1_tx, go1_rx) = std::sync::mpsc::channel();
    let lmdb_r1 = lmdb.clone();
    let sim_r1 = sim.clone();
    let t1 = std::thread::spawn(move || {
        let l_ro = lmdb_r1.begin_read().unwrap();
        let s_ro = sim_r1.begin_read().unwrap();
        assert_eq!(l_ro.txnid(), s_ro.txnid());
        tx1.send(()).unwrap();
        go1_rx.recv().unwrap();

        // Even after T2 overwrote `a`, deleted `b`, and inserted `c`, and T3 truncated the sub-DB,
        // T1 readers MUST still observe T1's exact snapshot!
        assert_eq!(l_ro.entries(l_db).unwrap(), s_ro.entries(s_db).unwrap());
        assert_eq!(collect_all(&l_ro, l_db), collect_all(&s_ro, s_db));
        assert_eq!(collect_all(&l_ro, l_db), vec![(b"a".to_vec(), b"v1".to_vec()), (b"b".to_vec(), b"v1".to_vec())]);
        let mut lc = l_ro.cursor(l_db).unwrap();
        let mut sc = s_ro.cursor(s_db).unwrap();
        assert_eq!(
            lc.seek_ge(b"b").unwrap().map(|(k, v)| (k.to_vec(), v.to_vec())),
            sc.seek_ge(b"b").unwrap().map(|(k, v)| (k.to_vec(), v.to_vec())),
        );
    });
    rx1.recv().unwrap();

    // T2: overwrite `a`, delete `b`, insert `c`.
    {
        let mut ltx = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
        ltx.put(l_db, b"a", b"v2").unwrap();
        ltx.del(l_db, b"b").unwrap();
        ltx.put(l_db, b"c", b"v2").unwrap();
        let _ = ltx.commit_raw().unwrap();

        let mut stx = sim.begin_write(sim.lock_map_read()).unwrap();
        stx.put(s_db, b"a", b"v2").unwrap();
        stx.del(s_db, b"b").unwrap();
        stx.put(s_db, b"c", b"v2").unwrap();
        let _ = stx.commit_raw().unwrap();
    }

    // Capture T2 readers on another thread.
    let (tx2, rx2) = std::sync::mpsc::channel();
    let (go2_tx, go2_rx) = std::sync::mpsc::channel();
    let lmdb_r2 = lmdb.clone();
    let sim_r2 = sim.clone();
    let t2 = std::thread::spawn(move || {
        let l_ro = lmdb_r2.begin_read().unwrap();
        let s_ro = sim_r2.begin_read().unwrap();
        assert_eq!(l_ro.txnid(), s_ro.txnid());
        tx2.send(()).unwrap();
        go2_rx.recv().unwrap();
        assert_eq!(l_ro.entries(l_db).unwrap(), s_ro.entries(s_db).unwrap());
        assert_eq!(collect_all(&l_ro, l_db), collect_all(&s_ro, s_db));
        assert_eq!(collect_all(&l_ro, l_db), vec![(b"a".to_vec(), b"v2".to_vec()), (b"c".to_vec(), b"v2".to_vec())]);
    });
    rx2.recv().unwrap();

    // T3: truncate sub-DB.
    {
        let mut ltx = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
        ltx.truncate(l_db).unwrap();
        let _ = ltx.commit_raw().unwrap();

        let mut stx = sim.begin_write(sim.lock_map_read()).unwrap();
        stx.truncate(s_db).unwrap();
        let _ = stx.commit_raw().unwrap();
    }

    go1_tx.send(()).unwrap();
    go2_tx.send(()).unwrap();
    t1.join().unwrap();
    t2.join().unwrap();

    // T3 readers see empty sub-DB on both backends.
    let l_ro3 = lmdb.begin_read().unwrap();
    let s_ro3 = sim.begin_read().unwrap();
    assert_eq!(l_ro3.txnid(), s_ro3.txnid());
    assert_eq!(l_ro3.entries(l_db).unwrap(), 0);
    assert_eq!(s_ro3.entries(s_db).unwrap(), 0);
    assert!(collect_all(&l_ro3, l_db).is_empty());
    assert!(collect_all(&s_ro3, s_db).is_empty());
}

/// Child helper for `sim_and_lmdb_freelist_page_reuse_pinned_vs_unpinned` so an LMDB debug
/// `SIGABRT` on a deliberately corrupted unpinned snapshot is caught cleanly without aborting
/// the parent test process.
#[test]
#[ignore = "child helper invoked by sim_and_lmdb_freelist_page_reuse_pinned_vs_unpinned"]
fn child_verify_lmdb_meta_restored_snapshot() {
    let dir = std::env::var("ELMDB_SIM_DIFF_DIR").expect("ELMDB_SIM_DIFF_DIR");
    let expected_tag: u8 = std::env::var("ELMDB_SIM_DIFF_TAG").expect("tag").parse().unwrap();
    let store = LmdbStore::open(&dir, LmdbConfig { map_size: 8 * 1024 * 1024, ..LmdbConfig::default() }).unwrap();
    let ro = store.begin_read().unwrap();
    let db = ro.open_subdb_existing("blobs", DbFlags::DEFAULT).unwrap().expect("blobs subdb");
    for i in 0..12usize {
        let key = format!("k{i:02}");
        let got = ro.get(db, key.as_bytes()).unwrap().expect("key present");
        let want = payload(expected_tag, i, 16_384);
        assert_eq!(got, want.as_slice(), "key {key} corrupted/overwritten");
    }
}

#[test]
fn sim_and_lmdb_freelist_page_reuse_pinned_vs_unpinned() {
    // 1. PINNED (`DurabilityGate` in Grouped mode): D+1..D+10 heavy 16 KiB overwrites MUST leave
    //    D's pages 100% intact on BOTH `SimKvStore` and real `LmdbStore`!
    {
        let dir = TempDir::new().unwrap();
        let lmdb = LmdbStore::open(
            dir.path(),
            LmdbConfig { map_size: 8 * 1024 * 1024, no_sync: true, ..LmdbConfig::default() },
        )
        .unwrap();
        let page_size = lmdb.page_size();
        let (l_gate, _) = DurabilityGate::open(lmdb, OsVfs::new(), dir.path(), DurabilityMode::Grouped).unwrap();

        let sim = SimKvStore::new(SimKvConfig { map_size: 8 * 1024 * 1024, ..SimKvConfig::default() });
        let s_vfs = SimVfs::new(BootId([0x33; 16]));
        let (s_gate, _) = DurabilityGate::open(sim.clone(), s_vfs, "/sim-pin", DurabilityMode::Grouped).unwrap();

        // Seed 12 * 16 KiB values and sync at D.
        {
            let mut lw = l_gate.begin_write().unwrap();
            let ldb = lw.open_subdb("blobs", DbFlags::DEFAULT).unwrap();
            let mut sw = s_gate.begin_write().unwrap();
            let sdb = sw.open_subdb("blobs", DbFlags::DEFAULT).unwrap();
            for i in 0..12usize {
                let key = format!("k{i:02}");
                let val = payload(1, i, 16_384);
                lw.put(ldb, key.as_bytes(), &val).unwrap();
                sw.put(sdb, key.as_bytes(), &val).unwrap();
            }
            lw.commit().unwrap();
            sw.commit().unwrap();
        }
        l_gate.sync().unwrap();
        s_gate.sync().unwrap();

        // Snapshot LMDB's 2 meta pages (2 * page_size) right after D was synced.
        let meta_at_d = std::fs::read(dir.path().join("data.mdb")).unwrap()[..2 * page_size].to_vec();

        // Execute 10 heavy overwriting commits WITHOUT syncing (D+1..D+10).
        for round in 2u8..=11u8 {
            let mut lw = l_gate.begin_write().unwrap();
            let ldb = lw.open_subdb("blobs", DbFlags::DEFAULT).unwrap();
            let mut sw = s_gate.begin_write().unwrap();
            let sdb = sw.open_subdb("blobs", DbFlags::DEFAULT).unwrap();
            for i in 0..12usize {
                let key = format!("k{i:02}");
                let val = payload(round, i, 16_384);
                lw.put(ldb, key.as_bytes(), &val).unwrap();
                sw.put(sdb, key.as_bytes(), &val).unwrap();
            }
            lw.commit().unwrap();
            sw.commit().unwrap();
        }

        // Copy post-D+10 `data.mdb` and splice back D's 2 meta pages -> pinned D MUST still be intact!
        let check_dir = TempDir::new().unwrap();
        let mut torn = std::fs::read(dir.path().join("data.mdb")).unwrap();
        torn[..2 * page_size].copy_from_slice(&meta_at_d);
        std::fs::write(check_dir.path().join("data.mdb"), &torn).unwrap();

        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "child_verify_lmdb_meta_restored_snapshot"])
            .env("ELMDB_SIM_DIFF_DIR", check_dir.path())
            .env("ELMDB_SIM_DIFF_TAG", "1")
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "pinned LMDB snapshot at D must survive D+1..D+10 overwrites: {}",
            String::from_utf8_lossy(&status.stderr)
        );

        // `SimKvStore` must match: power loss recovers D cleanly!
        sim.crash_power_loss();
        let s_ro = sim.begin_read().unwrap();
        let sdb = s_ro.open_subdb_existing("blobs", DbFlags::DEFAULT).unwrap().unwrap();
        for i in 0..12usize {
            let key = format!("k{i:02}");
            assert_eq!(s_ro.get(sdb, key.as_bytes()).unwrap().unwrap(), payload(1, i, 16_384).as_slice());
        }
    }

    // 2. UNPINNED (raw `LmdbStore` and raw `SimKvStore` without any durable pin):
    //    D+1..D+10 heavy overwrites recycle and overwrite D's pages on BOTH backends!
    {
        let dir = TempDir::new().unwrap();
        let lmdb = LmdbStore::open(
            dir.path(),
            LmdbConfig { map_size: 8 * 1024 * 1024, no_sync: true, ..LmdbConfig::default() },
        )
        .unwrap();
        let page_size = lmdb.page_size();
        let sim = SimKvStore::new(SimKvConfig { map_size: 8 * 1024 * 1024, ..SimKvConfig::default() });

        let (ldb, sdb) = {
            let mut lw = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
            let ldb = lw.open_subdb("blobs", DbFlags::DEFAULT).unwrap();
            let mut sw = sim.begin_write(sim.lock_map_read()).unwrap();
            let sdb = sw.open_subdb("blobs", DbFlags::DEFAULT).unwrap();
            for i in 0..12usize {
                let key = format!("k{i:02}");
                let val = payload(1, i, 16_384);
                lw.put(ldb, key.as_bytes(), &val).unwrap();
                sw.put(sdb, key.as_bytes(), &val).unwrap();
            }
            let (_, lg) = lw.commit_raw().unwrap();
            lmdb.sync(&lg).unwrap();
            drop(lg);
            let (_, sg) = sw.commit_raw().unwrap();
            sim.sync(&sg).unwrap();
            drop(sg);
            (ldb, sdb)
        };

        let meta_at_d = std::fs::read(dir.path().join("data.mdb")).unwrap()[..2 * page_size].to_vec();

        // Commit D+1..D+10 WITHOUT holding any durable pin!
        for round in 2u8..=11u8 {
            let mut lw = lmdb.begin_write(lmdb.lock_map_read()).unwrap();
            let mut sw = sim.begin_write(sim.lock_map_read()).unwrap();
            for i in 0..12usize {
                let key = format!("k{i:02}");
                let val = payload(round, i, 16_384);
                lw.put(ldb, key.as_bytes(), &val).unwrap();
                sw.put(sdb, key.as_bytes(), &val).unwrap();
            }
            let _ = lw.commit_raw().unwrap();
            let _ = sw.commit_raw().unwrap();
        }

        let check_dir = TempDir::new().unwrap();
        let mut torn = std::fs::read(dir.path().join("data.mdb")).unwrap();
        torn[..2 * page_size].copy_from_slice(&meta_at_d);
        std::fs::write(check_dir.path().join("data.mdb"), &torn).unwrap();

        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "child_verify_lmdb_meta_restored_snapshot"])
            .env("ELMDB_SIM_DIFF_DIR", check_dir.path())
            .env("ELMDB_SIM_DIFF_TAG", "1")
            .output()
            .unwrap();
        assert!(
            !status.status.success(),
            "unpinned LMDB snapshot at D MUST be corrupted/overwritten after D+1..D+10 freelist reuse"
        );

        // `SimKvStore` must match: power loss after unpinned D+1..D+10 reports `Corrupted`!
        sim.crash_power_loss();
        let s_ro = sim.begin_read().unwrap();
        assert!(matches!(s_ro.get(sdb, b"k00"), Err(StorageError::Corrupted(_))));
    }
}

/// Child helper that grows an LMDB environment to 4 MiB and writes >512 KiB of data so the
/// parent's 512 KiB mapping is guaranteed to trigger real `MDB_MAP_RESIZED`.
#[test]
#[ignore = "child helper invoked by sim_and_lmdb_map_resized_adoption_matches"]
fn child_grow_lmdb_map_for_diff_test() {
    let dir = std::env::var("ELMDB_SIM_DIFF_RESIZE_DIR").expect("resize dir");
    let store = LmdbStore::open(&dir, LmdbConfig { map_size: 4 * 1024 * 1024, ..LmdbConfig::default() }).unwrap();
    let (gate, _) = DurabilityGate::open(store, OsVfs::new(), &dir, DurabilityMode::Strict).unwrap();
    gate.grow_map(Some(4 * 1024 * 1024)).unwrap();
    let mut w = gate.begin_write().unwrap();
    let db = w.open_subdb("data", DbFlags::DEFAULT).unwrap();
    let pad = vec![0x5Au8; 32 * 1024];
    for i in 0..24u32 {
        w.put(db, format!("pad_{i:02}").as_bytes(), &pad).unwrap();
    }
    w.put(db, b"grown_key", b"from_child").unwrap();
    w.commit().unwrap();
}

#[test]
fn sim_and_lmdb_map_resized_adoption_matches() {
    let dir = TempDir::new().unwrap();
    let lmdb = LmdbStore::open(dir.path(), LmdbConfig { map_size: 512 * 1024, ..LmdbConfig::default() }).unwrap();
    let (l_gate, _) = DurabilityGate::open(lmdb, OsVfs::new(), dir.path(), DurabilityMode::Strict).unwrap();
    {
        let mut w = l_gate.begin_write().unwrap();
        let db = w.open_subdb("data", DbFlags::DEFAULT).unwrap();
        w.put(db, b"init", b"1").unwrap();
        w.commit().unwrap();
    }

    let sim = SimKvStore::new(SimKvConfig { map_size: 512 * 1024, ..SimKvConfig::default() });
    let s_vfs = SimVfs::new(BootId([0x44; 16]));
    let (s_gate, _) = DurabilityGate::open(sim.clone(), s_vfs, "/sim-resize", DurabilityMode::Strict).unwrap();
    {
        let mut w = s_gate.begin_write().unwrap();
        let db = w.open_subdb("data", DbFlags::DEFAULT).unwrap();
        w.put(db, b"init", b"1").unwrap();
        w.commit().unwrap();
    }

    // Grow real LMDB from a child process (triggering real `MDB_MAP_RESIZED` in parent),
    // and trigger equivalent `MapResized` + 4 MiB growth on `SimKvStore`.
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "child_grow_lmdb_map_for_diff_test"])
        .env("ELMDB_SIM_DIFF_RESIZE_DIR", dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "child resize failed: {}", String::from_utf8_lossy(&out.stderr));

    s_gate.grow_map(Some(4 * 1024 * 1024)).unwrap();
    {
        let mut w = s_gate.begin_write().unwrap();
        let db = w.open_subdb("data", DbFlags::DEFAULT).unwrap();
        let pad = vec![0x5Au8; 32 * 1024];
        for i in 0..24u32 {
            w.put(db, format!("pad_{i:02}").as_bytes(), &pad).unwrap();
        }
        w.put(db, b"grown_key", b"from_child").unwrap();
        w.commit().unwrap();
    }
    sim.inject_read_map_resized(1);

    // Both gates must transparently adopt the resize on `begin_read()` and observe identical entries & map size!
    let l_ro = l_gate.begin_read().unwrap();
    let s_ro = s_gate.begin_read().unwrap();
    let ldb = l_gate.open_subdb_ro(&l_ro, "data", DbFlags::DEFAULT).unwrap().unwrap();
    let sdb = s_gate.open_subdb_ro(&s_ro, "data", DbFlags::DEFAULT).unwrap().unwrap();
    assert_eq!(l_ro.entries(ldb).unwrap(), s_ro.entries(sdb).unwrap());
    assert_eq!(l_ro.get(ldb, b"grown_key").unwrap(), Some(&b"from_child"[..]));
    assert!(l_gate.map_size() >= 4 * 1024 * 1024);
    assert!(s_gate.map_size() >= 4 * 1024 * 1024);
}

#[test]
fn sim_and_lmdb_recovery_status_transitions_match() {
    let dir = TempDir::new().unwrap();
    let l_vfs = BootSwitchOsVfs::new(0x10);
    let s_vfs = SimVfs::new(BootId([0x10; 16]));
    let sim = SimKvStore::new(SimKvConfig::default());

    let open_pair = |boot: u8, mode: DurabilityMode| {
        l_vfs.set_boot_gen(boot);
        let lmdb = LmdbStore::open(
            dir.path(),
            LmdbConfig { map_size: 4 * 1024 * 1024, no_sync: mode != DurabilityMode::Strict, ..LmdbConfig::default() },
        )
        .unwrap();
        let (lg, lr) = DurabilityGate::open(lmdb, l_vfs.clone(), dir.path(), mode).unwrap();
        let (sg, sr) = DurabilityGate::open(sim.clone(), s_vfs.clone(), "/sim-rec", mode).unwrap();
        assert_eq!(lr.status, sr.status, "RecoveryStatus mismatch at boot={boot:#x}");
        (lg, sg, lr.status)
    };

    // 1. Fresh -> commit + sync -> Clean on same boot.
    {
        let (lg, sg, status) = open_pair(0x10, DurabilityMode::Grouped);
        assert_eq!(status, RecoveryStatus::Fresh);
        let mut lw = lg.begin_write().unwrap();
        let ldb = lw.open_subdb("d", DbFlags::DEFAULT).unwrap();
        lw.put(ldb, b"k1", b"v1").unwrap();
        lw.commit().unwrap();
        lg.sync().unwrap();

        let mut sw = sg.begin_write().unwrap();
        let sdb = sw.open_subdb("d", DbFlags::DEFAULT).unwrap();
        sw.put(sdb, b"k1", b"v1").unwrap();
        sw.commit().unwrap();
        sg.sync().unwrap();
    }

    // 2. Reopen same boot -> Clean; write unsynced commit -> drop without sync.
    {
        let (lg, sg, status) = open_pair(0x10, DurabilityMode::Grouped);
        assert_eq!(status, RecoveryStatus::Clean);
        let mut lw = lg.begin_write().unwrap();
        let ldb = lw.open_subdb("d", DbFlags::DEFAULT).unwrap();
        lw.put(ldb, b"k2", b"unsynced").unwrap();
        lw.commit().unwrap();

        let mut sw = sg.begin_write().unwrap();
        let sdb = sw.open_subdb("d", DbFlags::DEFAULT).unwrap();
        sw.put(sdb, b"k2", b"unsynced").unwrap();
        sw.commit().unwrap();
    }

    // 3. Reopen SAME boot with unsynced tail -> SameBootUnsyncedTail on both; opening RW Grouped
    //    auto-seals that tail, so write another unsynced commit before simulating reboot.
    {
        let (lg, sg, status) = open_pair(0x10, DurabilityMode::Grouped);
        assert_eq!(status, RecoveryStatus::SameBootUnsyncedTail);

        let mut lw = lg.begin_write().unwrap();
        let ldb = lw.open_subdb("d", DbFlags::DEFAULT).unwrap();
        lw.put(ldb, b"k3", b"unsynced_before_reboot").unwrap();
        lw.commit().unwrap();

        let mut sw = sg.begin_write().unwrap();
        let sdb = sw.open_subdb("d", DbFlags::DEFAULT).unwrap();
        sw.put(sdb, b"k3", b"unsynced_before_reboot").unwrap();
        sw.commit().unwrap();
    }

    // 4. Simulate reboot (new BootId) with unsynced tail -> UnsyncedAfterReboot on both,
    //    then acknowledge_restore() clears unacked_reboot and seals -> next same-boot open is Clean!
    {
        let mut rng = elmdb_sim::SimRng::seeded(99);
        let _ = s_vfs.crash_power_loss(&mut rng, true);
        // Align SimVfs rebooted boot_id with BootSwitchOsVfs (both changed != 0x10).
        let (lg, sg, status) = open_pair(0x20, DurabilityMode::Grouped);
        assert_eq!(status, RecoveryStatus::UnsyncedAfterReboot);
        assert!(lg.has_unacked_reboot() && sg.has_unacked_reboot());

        lg.acknowledge_restore().unwrap();
        sg.acknowledge_restore().unwrap();
        assert!(!lg.has_unacked_reboot() && !sg.has_unacked_reboot());
    }

    // 5. Reopen on same post-ack boot -> Clean on both; reopen on another new boot -> RebootVerified on both!
    {
        let (_lg, _sg, status) = open_pair(0x20, DurabilityMode::Grouped);
        assert_eq!(status, RecoveryStatus::Clean);
    }
    {
        let mut rng = elmdb_sim::SimRng::seeded(100);
        let _ = s_vfs.crash_power_loss(&mut rng, true);
        let (_lg, _sg, status) = open_pair(0x30, DurabilityMode::Grouped);
        assert_eq!(status, RecoveryStatus::RebootVerified);
    }
}
