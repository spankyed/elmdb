//! The store: what a write commits outlives the process, the format stamp, refusals, and growing the map. Ported from
//! apack's `tests/lmdb/store.spec.ts` and `format-stamp.spec.ts`; which sync `durable()` waits for is `sync`'s.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};

use heed::types::Bytes;
use heed::{CommitOrAbort, EnvOpenOptions};

use super::*;

fn quiet() -> StoreOptions {
    StoreOptions { syncs: false, ..Default::default() }
}

fn put(store: &Store, key: &str, value: &[u8]) -> Result<()> {
    let key = crate::format::key::encode(&[key.into()]);
    store.write(|txn, db| db.put(txn, &key, value).map_err(Into::into), false)
}

fn get(store: &Store, key: &str) -> Option<Vec<u8>> {
    let key = crate::format::key::encode(&[key.into()]);
    store.read(|txn, db| Ok(db.get(txn, &key)?.map(<[u8]>::to_vec))).unwrap()
}

#[test]
fn a_test_s_database_refuses_to_grow_past_the_test_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(TEST_MAP_LIMIT / 2), ..quiet() }).unwrap();
    store.grow().unwrap();
    assert_eq!(store.grow().unwrap_err().code, ErrorCode::MapFull);
}

#[test]
fn keeps_what_was_committed_across_a_close_and_an_open() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    store.close().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    assert_eq!(get(&store, "a").as_deref(), Some(&b"1"[..]));
}

#[test]
fn a_read_only_store_refuses_a_write_and_reads_what_is_there() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    store.close().unwrap();
    let store = Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() }).unwrap();
    assert_eq!(get(&store, "a").as_deref(), Some(&b"1"[..]));
    let error = put(&store, "b", b"2").unwrap_err();
    assert_eq!(error.code, ErrorCode::ReadOnly);
    assert_eq!(store.grow().unwrap_err().code, ErrorCode::ReadOnly);
}

#[test]
fn a_windows_read_only_map_never_claims_unmapped_file_capacity() {
    assert_eq!(read_only_map_size(32 * 4096, None), 32 * 4096);
    assert_eq!(read_only_map_size(32 * 4096, Some(MAP_STEP)), 32 * 4096);
    assert_eq!(read_only_map_size(32 * 4096, Some(16 * 4096)), 16 * 4096);
    // A torn tail page leaves the file short of whole pages: the view ends at the last whole one, whatever was asked
    assert_eq!(read_only_map_size(32 * 4096 + 100, None), 32 * 4096);
    assert_eq!(read_only_map_size(32 * 4096 + 100, Some(MAP_STEP)), 32 * 4096);
}

#[cfg(windows)]
#[test]
fn a_windows_read_only_open_and_adoption_follow_the_physical_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    store.close().unwrap();
    let store =
        Store::open(dir.path(), StoreOptions { read_only: true, map_size_bytes: Some(MAP_STEP * 2), ..quiet() })
            .unwrap();
    assert!(store.map_size() as u64 <= store.file_size());
    store.adopt_map_size().unwrap();
    assert!(store.map_size() as u64 <= store.file_size());
    assert_eq!(get(&store, "a").as_deref(), Some(&b"1"[..]));
}

#[cfg(unix)]
#[test]
fn a_read_only_store_refuses_files_without_reader_registration() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    store.close().unwrap();
    let files: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().path()).collect();
    for file in &files {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o444)).unwrap();
    }
    match Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() }) {
        Err(error) => {
            assert_eq!(error.code, ErrorCode::Io);
            assert!(
                error.message.contains("Grant read access to data.mdb and write access to lock.mdb"),
                "{}",
                error.message
            );
            // A writable open refused the same files is refused for writing, not for reader registration
            let writable = Store::open(dir.path(), quiet()).err().unwrap();
            assert!(!writable.message.contains("reader registration"), "{}", writable.message);
        }
        // An elevated user can still write the lock file: it retains ordinary LMDB reader registration.
        Ok(store) => {
            assert!(std::fs::OpenOptions::new().write(true).open(dir.path().join("lock.mdb")).is_ok());
            store.close().unwrap();
        }
    }
    for file in &files {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}

#[test]
fn refuses_new_files_it_could_not_stamp_and_stamps_them_once_it_can() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    std::fs::create_dir_all(&path).unwrap();
    let canonical = path.canonicalize().unwrap();
    crate::failpoints::fail_once_at(crate::failpoints::points::STAMP, &canonical);
    let error = Store::open(&path, quiet()).err().unwrap();
    assert!(error.message.starts_with("Couldn't record the storage format of"), "{}", error.message);
    assert!(error.message.ends_with("No space left on device"), "{}", error.message);

    crate::failpoints::disarm_at(crate::failpoints::points::STAMP, &canonical);
    let store = Store::open(&path, quiet()).unwrap();
    assert_eq!(store.meta(meta::FORMAT).unwrap(), Some(format::FORMAT_VERSION.to_be_bytes().to_vec()));
    assert_eq!(store.meta(meta::KIND).unwrap().as_deref(), Some(format::KIND.as_bytes()));
}

/// Rewrites the stored format, as a database of another version would hold it
fn stamp_format(path: &Path, version: u32) {
    let store = Store::open(path, quiet()).unwrap();
    store.write(|txn, db| put_meta(txn, db.main, meta::FORMAT, &version.to_be_bytes()), false).unwrap();
    store.close().unwrap();
}

#[test]
fn refuses_a_newer_format_saying_to_update_and_an_older_one_saying_it_is_made_again() {
    let dir = tempfile::tempdir().unwrap();
    stamp_format(dir.path(), format::FORMAT_VERSION + 1);
    let newer = Store::open(dir.path(), quiet()).err().unwrap();
    assert_eq!(newer.code, ErrorCode::WrongFormat);
    assert!(newer.message.contains("update elmdb"), "{}", newer.message);

    let dir = tempfile::tempdir().unwrap();
    stamp_format(dir.path(), 0);
    let older = Store::open(dir.path(), quiet()).err().unwrap();
    assert_eq!(older.code, ErrorCode::WrongFormat);
    assert!(older.message.contains("made again") && !older.message.contains("elmdb upgrade"), "{}", older.message);
}

#[test]
fn refuses_another_programs_lmdb_files() {
    let dir = tempfile::tempdir().unwrap();
    {
        // SAFETY: a test's own new directory, opened once
        let env = unsafe { EnvOpenOptions::new().max_dbs(2).open(dir.path()) }.unwrap();
        let wtxn = env.write_txn().unwrap();
        env.create_databases_and_commit(wtxn, |opener, wtxn| -> heed::Result<_> {
            Ok(CommitOrAbort::<_, ()>::Commit(opener.create_database::<Bytes, Bytes>(wtxn, Some("theirs"))?))
        })
        .unwrap();
        env.prepare_for_closing().wait();
    }
    let error = Store::open(dir.path(), quiet()).err().unwrap();
    assert_eq!(error.code, ErrorCode::NotADatabase);
    assert!(error.message.contains("data of another program"), "{}", error.message);
}

#[test]
fn refuses_a_file_in_another_lmdb_data_version_naming_lmdb_js() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), quiet()).unwrap().close().unwrap();
    // LMDB's two meta pages each hold the data version after the magic; lmdb-js writes 2 there
    let data = dir.path().join("data.mdb");
    let mut bytes = std::fs::read(&data).unwrap();
    let magic = 0xBEEF_C0DEu32.to_le_bytes();
    let mut patched = 0;
    for at in 0..bytes.len().saturating_sub(8) {
        if bytes[at..at + 4] == magic {
            bytes[at + 4..at + 8].copy_from_slice(&2u32.to_le_bytes());
            patched += 1;
        }
    }
    assert_eq!(patched, 2, "both meta pages hold the magic");
    std::fs::write(&data, bytes).unwrap();
    let error = Store::open(dir.path(), quiet()).err().unwrap();
    assert_eq!(error.code, ErrorCode::NotADatabase);
    assert!(error.message.contains("lmdb-js"), "{}", error.message);
}

#[test]
fn grows_the_map_when_a_unit_fills_it() {
    let dir = tempfile::tempdir().unwrap();
    let page = 16 << 10;
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(64 * page), ..quiet() }).unwrap();
    let before = store.map_size();
    let value = vec![7u8; 256 << 10];
    // One unit of 8 MiB, past a 1 MiB map, so the write fills it, grows it and runs again
    store
        .write(
            |txn, db| {
                for i in 0..32u32 {
                    db.put(txn, &crate::format::key::encode(&[u64::from(i).into()]), &value)?;
                }
                Ok(())
            },
            false,
        )
        .unwrap();
    assert!(store.map_size() > before, "the map grew from {before}");
    assert_eq!(get(&store, "absent"), None);
    store.close().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    let count = store.read(|txn, db| Ok(db.len(txn)?)).unwrap();
    assert_eq!(count, 32 + 3, "the unit's 32 entries, the stamp's two, and the commit's sequence");
}

#[test]
fn a_durable_write_commits_with_its_sync_and_owes_nothing_after() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            sync_interval: Duration::from_secs(60),
            sync_idle: Duration::from_secs(60),
            ..Default::default()
        },
    )
    .unwrap();
    store.write(|txn, db| db.put(txn, b"k", b"v").map_err(Into::into), true).unwrap();
    assert_eq!(store.syncs(), 1);
    store.durable().unwrap().wait().unwrap();
    assert_eq!(store.syncs(), 1, "nothing was owed");
    store.close().unwrap();
}

/// The writer half of the next test, run as a child of this test binary
#[test]
#[ignore = "run only by the next test, as its child process"]
fn child_writer() {
    let Ok(path) = std::env::var("ELMDB_CHILD_DB") else { return };
    let store = Store::open(
        Path::new(&path),
        StoreOptions {
            sync_idle: Duration::from_secs(600),
            sync_interval: Duration::from_secs(600),
            ..Default::default()
        },
    )
    .unwrap();
    put(&store, "killed", b"committed").unwrap();
    println!("written");
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn keeps_a_write_its_process_committed_though_the_process_is_killed_as_it_returns() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::tests::child_writer", "--nocapture", "--test-threads=1"])
        .env("ELMDB_CHILD_DB", dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut written = false;
    for line in stdout.lines() {
        if line.unwrap().contains("written") {
            written = true;
            break;
        }
    }
    assert!(written, "the writer exited before writing");
    child.kill().unwrap();
    child.wait().unwrap();
    let store = Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() }).unwrap();
    assert_eq!(get(&store, "killed").as_deref(), Some(&b"committed"[..]));
}

#[test]
fn reads_run_on_other_threads_while_a_writer_writes() {
    // A read transaction without thread-local storage may be used from any thread (`read_txn_without_tls`)
    fn send<T: Send>() {}
    send::<RoTxn<'static, WithoutTls>>();

    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::open(dir.path(), quiet()).unwrap());
    put(&store, "n", &0u64.to_be_bytes()).unwrap();
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let store = std::sync::Arc::clone(&store);
            std::thread::spawn(move || {
                let mut last = 0;
                for _ in 0..200 {
                    let n = u64::from_be_bytes(get(&store, "n").unwrap().try_into().unwrap());
                    assert!(n >= last, "a later read saw an earlier commit");
                    last = n;
                }
            })
        })
        .collect();
    for n in 1..=200u64 {
        put(&store, "n", &n.to_be_bytes()).unwrap();
    }
    for reader in readers {
        reader.join().unwrap();
    }
}

/// Not a check: what one growth of the map costs, for `docs/evidence/phase-3.md`. Run with
/// `cargo test -p elmdb --release -- --ignored --nocapture store::tests::what_a_growth_costs`
#[test]
#[ignore = "a measurement, run by hand"]
fn what_a_growth_costs() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(MAP_STEP), ..quiet() }).unwrap();
    let value = vec![1u8; 64 << 10];
    let fill = |from: u32, to: u32| {
        store
            .write(
                |txn, db| {
                    for i in from..to {
                        db.put(txn, &crate::format::key::encode(&[u64::from(i).into()]), &value)?;
                    }
                    Ok(())
                },
                false,
            )
            .unwrap()
    };
    // Nearly fill the map, then time a unit that doesn't fit and one the same size that does
    fill(0, 900);
    let t = std::time::Instant::now();
    fill(900, 1000);
    let grew = t.elapsed();
    let t = std::time::Instant::now();
    fill(1000, 1100);
    let fitted = t.elapsed();
    println!(
        "map {} MiB; a 6.25 MiB unit that grew it: {grew:?}; the same unit after: {fitted:?}",
        store.map_size() >> 20
    );
}

#[test]
fn a_wait_for_the_write_transaction_ends_when_the_unit_does_or_when_it_is_stopped() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), quiet()).unwrap());
    // No unit open: the wait returns at once
    store.wait_while_unit_open(|| false);

    let waiter = |stop: Arc<AtomicBool>| {
        let store = Arc::clone(&store);
        let (sent, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            store.wait_while_unit_open(|| stop.load(Ordering::SeqCst));
            sent.send(()).unwrap();
        });
        ended
    };
    let quiet_for = Duration::from_millis(100);

    // Ended by the unit's end
    let held = store.begin().unwrap();
    let ended = waiter(Arc::new(AtomicBool::new(false)));
    assert!(ended.recv_timeout(quiet_for).is_err(), "returned while the unit was open");
    held.abort();
    store.end_unit();
    ended.recv_timeout(crate::waits::ARRIVES).expect("returned once the unit ended");

    // Ended by its stop, the unit still open
    let held = store.begin().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let ended = waiter(Arc::clone(&stop));
    assert!(ended.recv_timeout(quiet_for).is_err(), "returned while the unit was open");
    stop.store(true, Ordering::SeqCst);
    store.wake_unit_waiters();
    ended.recv_timeout(crate::waits::ARRIVES).expect("returned once stopped");
    held.abort();
    store.end_unit();
}

/// The writer half of the next test, run as a child of this test binary: it overwrites `a` and closes
#[test]
#[ignore = "run only by the next test, as its child process"]
fn child_overwriter() {
    let Ok(path) = std::env::var("ELMDB_OVERWRITE_DB") else { return };
    let store = Store::open(Path::new(&path), quiet()).unwrap();
    put(&store, "a", b"2").unwrap();
    store.close().unwrap();
}

#[test]
fn a_read_after_another_process_commits_sees_the_commit_though_a_read_transaction_was_kept() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    // A read keeps its transaction idle for the next, at this snapshot
    assert_eq!(get(&store, "a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&store, "a").as_deref(), Some(&b"1"[..]));
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::tests::child_overwriter", "--nocapture", "--test-threads=1"])
        .env("ELMDB_OVERWRITE_DB", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the child's write failed");
    assert_eq!(get(&store, "a").as_deref(), Some(&b"2"[..]));
}

/// Asks for a read of `key` at the last commit, held for later, which sends what it read to the receiver answered
fn ask_for(store: &Store, key: &str) -> (CallRead, std::sync::mpsc::Receiver<Option<Vec<u8>>>) {
    let (told, answer) = std::sync::mpsc::channel();
    let key = crate::format::key::encode(&[key.into()]);
    let read = move |txn: &RoTxn<'static, WithoutTls>, db: Db| {
        told.send(db.get(txn, &key).unwrap().map(<[u8]>::to_vec)).unwrap();
    };
    (store.ask(read).unwrap().expect("held, not run at the call"), answer)
}

#[test]
fn a_read_asked_before_another_process_grew_the_map_runs_on_its_own_snapshot_when_this_one_takes_the_new_size() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(16 * (16 << 10)), ..quiet() }).unwrap();
    put(&store, "a", b"1").unwrap();
    let (asked, answer) = ask_for(&store, "grown");
    grow_elsewhere(dir.path());
    let before = store.map_size();
    // The next read asked here is refused the map as it was, which is taken at its new size once the read waiting has
    // run on its own snapshot, and then asked again
    let (after, answer_after) = ask_for(&store, "grown");
    assert!(store.map_size() > before, "the map was taken at the size the other process grew it to");
    assert_eq!(answer.try_recv().unwrap(), None, "the read ran on a later snapshot than its call's");
    assert_eq!(store.held_for_calls(), (1, 1));
    store.run_asked(asked).unwrap();
    assert!(answer.try_recv().is_err(), "the read ran twice");
    store.run_asked(after).unwrap();
    assert_eq!(answer_after.try_recv().unwrap().as_deref(), Some(&b"yes"[..]));
    assert_eq!(get(&store, "grown").as_deref(), Some(&b"yes"[..]));
}

#[test]
fn a_listener_is_told_of_what_another_process_commits_and_not_of_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    let (told, heard) = std::sync::mpsc::channel();
    store.wake.listen(Arc::new(move || told.send(()).unwrap())).unwrap();
    assert!(store.wake.listen(Arc::new(|| {})).is_err_and(|error| error.code == ErrorCode::AlreadyListening));

    put(&store, "a", b"1").unwrap();
    assert!(heard.recv_timeout(std::time::Duration::from_millis(200)).is_err(), "its own commits aren't news");

    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::tests::child_overwriter", "--nocapture", "--test-threads=1"])
        .env("ELMDB_OVERWRITE_DB", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the child's write failed");
    heard.recv_timeout(std::time::Duration::from_secs(10)).expect("told of the other process's commit");
    assert!(store.committed_elsewhere().unwrap());

    store.wake.stop();
    assert_eq!(std::fs::read_dir(dir.path().join("wake")).unwrap().count(), 0, "a listener that stopped files nothing");
}

/// Listens on the store `ELMDB_LISTENING_DB` names, says so with a file beside it, and waits to be killed
#[test]
#[ignore = "a helper a_killed_listeners_mailbox_is_removed_by_the_next_commit runs as another process"]
fn child_listens_until_killed() {
    let Ok(path) = std::env::var("ELMDB_LISTENING_DB") else { return };
    let store = Store::open(Path::new(&path), quiet()).unwrap();
    store.wake.listen(Arc::new(|| {})).unwrap();
    std::fs::write(Path::new(&path).join("listening"), b"").unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[test]
fn a_killed_listeners_mailbox_is_removed_by_the_next_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::tests::child_listens_until_killed", "--nocapture", "--test-threads=1"])
        .env("ELMDB_LISTENING_DB", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while !dir.path().join("listening").exists() {
        assert!(started.elapsed() < std::time::Duration::from_secs(30), "the other process never listened");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(std::fs::read_dir(dir.path().join("wake")).unwrap().count(), 1, "its mailbox, filed");
    child.kill().unwrap();
    child.wait().unwrap();
    put(&store, "a", b"1").unwrap();
    assert_eq!(std::fs::read_dir(dir.path().join("wake")).unwrap().count(), 0, "removed, nobody owning it");
}

#[test]
fn a_read_runs_while_a_growth_waits_for_the_open_unit() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), quiet()).unwrap());
    put(&store, "a", b"1").unwrap();
    let held = store.begin().unwrap();
    let grower = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || store.grow())
    };
    // The growth waits for the unit's hold on the gate
    std::thread::sleep(Duration::from_millis(100));
    let (sent, read) = std::sync::mpsc::channel();
    {
        let store = Arc::clone(&store);
        std::thread::spawn(move || sent.send(get(&store, "a")).unwrap());
    }
    let value = read.recv_timeout(crate::waits::ARRIVES).expect("a read ran while a growth waited for the unit");
    assert_eq!(value.as_deref(), Some(&b"1"[..]));
    held.abort();
    store.end_unit();
    grower.join().unwrap().unwrap();
}

#[test]
fn a_backup_replaces_an_earlier_one_and_refuses_a_database_open_here() {
    let dir = tempfile::tempdir().unwrap();
    let backup = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    store.copy_to(backup.path()).unwrap();
    put(&store, "a", b"2").unwrap();
    store.copy_to(backup.path()).unwrap();
    let mut files: Vec<_> = std::fs::read_dir(backup.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    files.sort();
    assert_eq!(files, ["data.mdb", "synced.lock"], "the copy, and the decision lock it held");
    let copy = Store::open(backup.path(), quiet()).unwrap();
    assert_eq!(get(&copy, "a").as_deref(), Some(&b"2"[..]));

    for target in [dir.path(), backup.path()] {
        assert_eq!(store.copy_to(target).unwrap_err().code, ErrorCode::AlreadyOpen);
    }
    assert_eq!(get(&store, "a").as_deref(), Some(&b"2"[..]));
    assert_eq!(get(&copy, "a").as_deref(), Some(&b"2"[..]));
}

#[test]
fn a_close_returns_though_a_wait_for_durability_is_still_held() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::default()).unwrap();
    put(&store, "a", b"1").unwrap();
    let durable = store.durable().expect("the store syncs");
    let (sent, closed) = std::sync::mpsc::channel();
    std::thread::spawn(move || sent.send(store.close()).unwrap());
    closed.recv_timeout(crate::waits::ARRIVES).expect("the close returned").unwrap();
    durable.wait().unwrap();
}

#[cfg(unix)]
#[test]
#[ignore = "run only by the next test, as its child process"]
fn child_read_only_requires_registration() {
    let Ok(path) = std::env::var("ELMDB_REVIEW_READ_ONLY") else { return };
    println!("ELMDB_READ_ONLY_CHILD_ENTERED");
    let path = std::path::Path::new(&path);
    let can_register = std::fs::OpenOptions::new().write(true).open(path.join("lock.mdb")).is_ok();
    match Store::open(path, StoreOptions { read_only: true, ..quiet() }) {
        Ok(store) => {
            assert!(can_register, "a reader without lock-file access must not open beside a writer");
            store.close().unwrap();
        }
        Err(error) => assert_eq!(error.code, ErrorCode::Io),
    }
}

#[cfg(unix)]
#[test]
fn a_read_only_process_requires_registration_while_a_writer_is_open() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    let lock = directory.path().join("lock.mdb");
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o444)).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::tests::child_read_only_requires_registration", "--nocapture"])
        .env("ELMDB_REVIEW_READ_ONLY", directory.path())
        .output()
        .unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ELMDB_READ_ONLY_CHILD_ENTERED"),
        "the selected child regression must actually run"
    );
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(get(&store, "a").as_deref(), Some(&b"1"[..]));
    store.close().unwrap();
}

#[test]
fn backup_failure_owns_only_its_private_file_and_releases_destination() {
    let dir = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "value", b"old").unwrap();
    store.copy_to(target.path()).unwrap();
    put(&store, "value", b"new").unwrap();
    let sentinel = target.path().join("data.mdb.partial");
    std::fs::write(&sentinel, b"belongs to someone else").unwrap();
    // The copy is private until published: a temporary file of its own in the destination
    let destination = target.path().to_owned();
    crate::failpoints::run_once_here(crate::failpoints::points::BACKUP_READY, move || {
        assert!(
            std::fs::read_dir(&destination).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".elmdb-backup-"))
        );
    });
    crate::failpoints::fail_once_here(crate::failpoints::points::BACKUP_PUBLISH);
    let failed = store.copy_to(target.path());
    assert_eq!(failed.unwrap_err().code, ErrorCode::Io);
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"belongs to someone else");
    assert!(
        std::fs::read_dir(target.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".elmdb-backup-"))
    );
    let earlier = Store::open(target.path(), quiet()).unwrap();
    assert_eq!(get(&earlier, "value").as_deref(), Some(&b"old"[..]));
    earlier.close().unwrap();
    store.copy_to(target.path()).unwrap();
    let copied = Store::open(target.path(), quiet()).unwrap();
    assert_eq!(get(&copied, "value").as_deref(), Some(&b"new"[..]));
}

#[test]
fn backup_reserves_destination_against_open_and_another_publisher() {
    let dir = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), quiet()).unwrap());
    put(&store, "value", b"complete").unwrap();
    let (entered, ready) = std::sync::mpsc::channel();
    let (resume, continued) = std::sync::mpsc::channel();
    let destination = target.path().to_owned();
    let worker_store = store.clone();
    let worker = std::thread::spawn(move || {
        crate::failpoints::run_once_here(crate::failpoints::points::BACKUP_READY, move || {
            entered.send(()).expect("the test waits for the publisher");
            continued.recv_timeout(crate::waits::ARRIVES).expect("the test lets the publisher go on");
        });
        worker_store.copy_to(&destination)
    });
    ready.recv_timeout(crate::waits::ARRIVES).expect("publisher reached ready point");
    let opened = Store::open(target.path(), quiet());
    let published = store.copy_to(target.path());
    resume.send(()).unwrap();
    worker.join().unwrap().unwrap();
    assert!(matches!(opened, Err(Error { code: ErrorCode::AlreadyOpen, .. })));
    assert_eq!(published.unwrap_err().code, ErrorCode::AlreadyOpen);
    let copy = Store::open(target.path(), quiet()).unwrap();
    assert_eq!(get(&copy, "value").as_deref(), Some(&b"complete"[..]));
}

#[test]
fn backup_reports_a_postpublication_sync_failure_without_losing_the_complete_copy() {
    let dir = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "value", b"complete").unwrap();
    crate::failpoints::fail_once_here(crate::failpoints::points::BACKUP_PUBLISHED);
    let result = store.copy_to(target.path());
    let error = result.unwrap_err();
    assert_eq!(error.code, ErrorCode::Io);
    assert!(error.message.contains("was published") && error.message.contains("retrying backup is safe"));
    let copy = Store::open(target.path(), quiet()).unwrap();
    assert_eq!(get(&copy, "value").as_deref(), Some(&b"complete"[..]));
}

#[test]
fn backup_reports_a_record_it_could_not_remove_as_after_its_publication() {
    let dir = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "value", b"complete").unwrap();
    // A record's name that no file removal takes: a directory, with something in it
    std::fs::create_dir(target.path().join(record::RECORD)).unwrap();
    std::fs::write(target.path().join(record::RECORD).join("held"), b"").unwrap();
    let error = store.copy_to(target.path()).unwrap_err();
    assert_eq!(error.code, ErrorCode::Io);
    assert!(error.message.contains("was published") && error.message.contains("retrying backup is safe"), "{error}");
    assert!(target.path().join("data.mdb").is_file(), "the copy wasn't published");
}

#[test]
fn a_backup_holds_its_target_s_decision_lock_and_presence_lock_until_it_has_published() {
    let dir = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    // An open there earlier, closed, left its presence lock; none was ever made in `fresh`
    std::fs::write(target.path().join(record::PRESENCE), b"").unwrap();
    let fresh = tempfile::tempdir().unwrap();
    for (into, presence) in [(target.path(), true), (fresh.path(), false)] {
        let held = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen = std::sync::Arc::clone(&held);
        let at = into.to_owned();
        crate::failpoints::run_once_here(crate::failpoints::points::BACKUP_READY, move || {
            let free = |name: &str| std::fs::File::open(at.join(name)).ok().map(|file| file.try_lock().is_ok());
            *seen.lock().unwrap() = Some((free(record::RECORD_LOCK), free(record::PRESENCE)));
        });
        store.copy_to(into).unwrap();
        let held = held.lock().unwrap().take().expect("the backup reached publication");
        // An open that began meanwhile would wait on the decision lock, made where none was, before it took the
        // presence lock: neither is free until the copy is published
        assert_eq!(held, (Some(false), presence.then_some(false)), "{}", into.display());
    }
}

#[test]
fn what_a_backup_and_a_read_only_open_of_it_leave_is_left_by_a_backup_and_what_a_writable_open_adds_is_not() {
    let names = |dir: &Path| -> Vec<std::ffi::OsString> {
        let mut names: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        names
    };
    let dir = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();

    // The staged copy, before it is published, and what the published one leaves
    let staged = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = std::sync::Arc::clone(&staged);
    let at = target.path().to_owned();
    crate::failpoints::run_once_here(crate::failpoints::points::BACKUP_READY, move || {
        *seen.lock().unwrap() = names(&at);
    });
    store.copy_to(target.path()).unwrap();
    let staged = std::mem::take(&mut *staged.lock().unwrap());
    assert!(staged.iter().any(|name| name.to_string_lossy().starts_with(".elmdb-backup-")), "{staged:?}");
    let published = names(target.path());

    // A read-only open of the copy, listening for other processes' commits
    let copy = Store::open(target.path(), StoreOptions { read_only: true, ..quiet() }).unwrap();
    copy.wake.listen(Arc::new(|| {})).unwrap();
    let read = names(target.path());
    assert!(read.iter().any(|name| name == "wake"), "{read:?}");
    copy.wake.stop();
    copy.close().unwrap();
    for name in staged.iter().chain(&published).chain(&read) {
        assert!(left_by_a_backup(name), "{name:?} is left by a backup or a read-only open of it");
    }

    // A writable open leaves the record and its mode's lock, which no backup does
    Store::open(target.path(), quiet()).unwrap().close().unwrap();
    let written: Vec<_> = names(target.path()).into_iter().filter(|name| !read.contains(name)).collect();
    assert!(written.iter().any(|name| name == record::RECORD), "{written:?}");
    for name in &written {
        assert!(!left_by_a_backup(name), "{name:?} is left by a writable open");
    }
    for name in ["package.json", "data.mdb.partial", "elmdb-backup-x", "wake.lock"] {
        assert!(!left_by_a_backup(name.as_ref()), "{name}");
    }
    #[cfg(unix)]
    assert!(!left_by_a_backup(std::os::unix::ffi::OsStrExt::from_bytes(b"data.mdb\xff")));
}

/// Another program's LMDB files in `dir`: a data file naming a database of its own, and its lock file
fn another_programs_lmdb(dir: &Path) {
    // SAFETY: a test's own new directory, opened once
    let env = unsafe { EnvOpenOptions::new().max_dbs(2).open(dir) }.unwrap();
    let wtxn = env.write_txn().unwrap();
    env.create_databases_and_commit(wtxn, |opener, wtxn| -> heed::Result<_> {
        Ok(CommitOrAbort::<_, ()>::Commit(opener.create_database::<Bytes, Bytes>(wtxn, Some("theirs"))?))
    })
    .unwrap();
    env.prepare_for_closing().wait();
}

#[test]
fn a_backup_replaces_no_data_file_of_another_program_s_even_where_a_failed_open_left_its_locks() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    // Bytes LMDB never wrote, and an LMDB database naming no elmdb database
    let bytes = tempfile::tempdir().unwrap();
    std::fs::write(bytes.path().join("data.mdb"), b"another program's").unwrap();
    let lmdb = tempfile::tempdir().unwrap();
    another_programs_lmdb(lmdb.path());
    for target in [bytes.path(), lmdb.path()] {
        // An open of it is refused, and leaves the locks it made deciding: no evidence of a backup
        assert!(Store::open(target, StoreOptions { read_only: true, ..quiet() }).is_err());
        assert!(target.join(record::RECORD_LOCK).exists() && target.join(record::PRESENCE).exists());
        let before = std::fs::read(target.join("data.mdb")).unwrap();
        for into in [BackupInto::AnyClosedDatabase, BackupInto::Backups] {
            let error = store.copy_into(target, into).unwrap_err();
            assert_eq!(error.code, ErrorCode::TargetRefused, "{error}");
            assert!(error.message.contains("no elmdb database's"), "{error}");
            assert_eq!(std::fs::read(target.join("data.mdb")).unwrap(), before, "{into:?}");
        }
    }
    // An elmdb database's file is replaced: an earlier backup's under either, and a database's closed after writing
    // where the backup may replace one
    let earlier = tempfile::tempdir().unwrap();
    store.copy_to(earlier.path()).unwrap();
    Store::open(earlier.path(), StoreOptions { read_only: true, ..quiet() }).unwrap().close().unwrap();
    store.copy_into(earlier.path(), BackupInto::Backups).unwrap();
    let written = tempfile::tempdir().unwrap();
    Store::open(written.path(), quiet()).unwrap().close().unwrap();
    assert_eq!(store.copy_into(written.path(), BackupInto::Backups).unwrap_err().code, ErrorCode::TargetRefused);
    store.copy_to(written.path()).unwrap();
    assert_eq!(get(&Store::open(written.path(), quiet()).unwrap(), "a").as_deref(), Some(&b"1"[..]));
}

#[test]
fn a_backup_into_the_listeners_wake_of_the_database_it_backs_up_is_refused_and_makes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    // What listeners that stopped leave: their directory, empty, which every commit clears of what isn't a mailbox
    std::fs::create_dir(dir.path().join("wake")).unwrap();
    for into in [dir.path().join("wake"), dir.path().join("wake").join("today")] {
        let error = store.copy_to(&into).unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetRefused, "{error}");
        assert!(error.message.contains("listener's mailbox"), "{error}");
        assert_eq!(std::fs::read_dir(dir.path().join("wake")).unwrap().count(), 0, "{}", into.display());
    }
    // Named through a link to the database too
    #[cfg(unix)]
    {
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("db");
        std::os::unix::fs::symlink(dir.path(), &link).unwrap();
        assert_eq!(store.copy_to(&link.join("wake")).unwrap_err().code, ErrorCode::TargetRefused);
        assert_eq!(std::fs::read_dir(dir.path().join("wake")).unwrap().count(), 0);
    }
    // Another directory inside it is the database's to hold
    store.copy_to(&dir.path().join("backups")).unwrap();
    assert!(dir.path().join("backups").join("data.mdb").is_file());
}

#[test]
fn a_backup_held_to_backups_looks_at_its_target_under_the_decision_lock() {
    let dir = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    store.copy_into(target.path(), BackupInto::Backups).unwrap();
    let before = std::fs::read(target.path().join("data.mdb")).unwrap();
    // What an open there that wrote in place before the lock was taken leaves: seen, since it is looked for after
    let at = target.path().to_owned();
    crate::failpoints::run_once_here(crate::failpoints::points::BACKUP_DECIDING, move || {
        std::fs::write(at.join(record::RECORD), b"").unwrap();
    });
    let error = store.copy_into(target.path(), BackupInto::Backups).unwrap_err();
    assert_eq!(error.code, ErrorCode::TargetRefused, "{error}");
    assert!(error.message.contains("opened for writing"), "{error}");
    assert_eq!(std::fs::read(target.path().join("data.mdb")).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn an_open_names_a_lock_file_it_can_t_open_as_that_rather_than_one_it_can_t_make() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), quiet()).unwrap().close().unwrap();
    let lock = dir.path().join(record::RECORD_LOCK);
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();
    // Root opens it all the same, and nothing here can make it fail to
    if std::fs::File::open(&lock).is_ok() {
        return;
    }
    let error = Store::open(dir.path(), quiet()).err().unwrap();
    assert_eq!(error.code, ErrorCode::Io);
    assert!(error.message.contains("Cannot open the lock file") && error.message.contains("synced.lock"), "{error}");
}

#[test]
fn backup_creates_nested_destinations_and_leaves_only_the_complete_data_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("one/two");
    #[cfg(unix)]
    assert_eq!(backup::directories_to_sync(&target), [target.clone(), root.path().join("one"), root.path().to_owned()]);
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "value", b"complete").unwrap();
    store.copy_to(&target).unwrap();
    let mut names: Vec<_> = std::fs::read_dir(&target).unwrap().map(|e| e.unwrap().file_name()).collect();
    names.sort();
    assert_eq!(names, ["data.mdb", "synced.lock"], "the complete copy, and the decision lock it held");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(target.join("data.mdb")).unwrap().file_attributes() & 0x100,
            0,
            "a published backup must not retain FILE_ATTRIBUTE_TEMPORARY"
        );
    }
    let copy = Store::open(&target, quiet()).unwrap();
    assert_eq!(get(&copy, "value").as_deref(), Some(&b"complete"[..]));
}

#[test]
fn a_read_only_environment_requires_a_real_lock_table() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), quiet()).unwrap().close().unwrap();
    let mut options = EnvOpenOptions::new().read_txn_without_tls();
    // SAFETY: this private test database has no other open handle or writer. NO_LOCK deliberately simulates
    // LMDB's EROFS fallback; no database data is accessed through this unregistered environment.
    let env = unsafe { options.flags(EnvFlags::READ_ONLY | EnvFlags::NO_LOCK).open(dir.path()) }.unwrap();
    let error = require_registered_reader(&env, dir.path()).unwrap_err();
    assert_eq!(error.code, ErrorCode::Io);
    assert!(error.message.contains("no lock table"));
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an isolated privileged mount namespace; scripts/check-read-only-mount.py runs it in CI"]
fn read_only_mount_requires_reader_registration() {
    assert_eq!(std::env::var("ELMDB_TEST_READ_ONLY_MOUNT").as_deref(), Ok("1"));
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "proof", b"registered").unwrap();
    store.close().unwrap();
    assert!(Command::new("mount").arg("--bind").arg(dir.path()).arg(dir.path()).status().unwrap().success());
    struct Mount(std::path::PathBuf);
    impl Drop for Mount {
        fn drop(&mut self) {
            let _ = Command::new("umount").arg(&self.0).status();
        }
    }
    let _mount = Mount(dir.path().to_owned());
    assert!(Command::new("mount").args(["-o", "remount,bind,ro"]).arg(dir.path()).status().unwrap().success());
    let error = Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() })
        .err()
        .expect("a read-only mount must not open without a lock table");
    assert_eq!(error.code, ErrorCode::Io);
    assert!(error.message.contains("registration"));
    let destination = tempfile::tempdir().unwrap();
    let database =
        crate::Database::open(destination.path(), crate::OpenOptions { syncs: false, ..Default::default() }).unwrap();
    let before = database.sequence().unwrap();
    let error = database.replace_with(dir.path()).expect_err("a replacement source must register its reader");
    assert_eq!(error.code, ErrorCode::Io);
    assert!(error.message.contains("registration"));
    assert_eq!(database.sequence().unwrap(), before, "refusal must not commit a replacement");
    println!("ELMDB_READ_ONLY_MOUNT_REFUSED");
}

#[test]
fn a_new_map_is_twice_the_file_in_whole_steps_and_a_step_at_least() {
    assert_eq!(initial_map_size(0), MAP_STEP);
    assert_eq!(initial_map_size(MAP_STEP as u64), 2 * MAP_STEP);
    assert_eq!(initial_map_size(MAP_STEP as u64 + 1), 3 * MAP_STEP);
}

#[test]
fn only_a_writable_store_that_syncs_runs_group_commit_and_counts_its_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    assert!(store.durable().is_none());
    assert_eq!(store.syncs(), 0);
    assert_eq!(store.file_size(), std::fs::metadata(dir.path().join("data.mdb")).unwrap().len());
    store.close().unwrap();
    let store = Store::open(dir.path(), StoreOptions { read_only: true, ..Default::default() }).unwrap();
    assert!(store.durable().is_none(), "a read-only store has nothing to sync");
    store.close().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            sync_interval: Duration::from_secs(60),
            sync_idle: Duration::from_secs(60),
            ..Default::default()
        },
    )
    .unwrap();
    store.write(|txn, db| db.put(txn, b"k", b"1").map_err(Into::into), true).unwrap();
    store.write(|txn, db| db.put(txn, b"k", b"2").map_err(Into::into), true).unwrap();
    assert_eq!(store.syncs(), 2, "a sync for each durable write");
    store.close().unwrap();
}

#[test]
fn a_read_leaves_its_transaction_for_the_next_and_at_most_max_idle_are_kept() {
    fn nested(store: &Store, depth: usize) -> Result<()> {
        if depth == 0 { Ok(()) } else { store.read(|_, _| nested(store, depth - 1)) }
    }
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    assert_eq!(get(&store, "a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&store, "a").as_deref(), Some(&b"1"[..]));
    assert_eq!(store.idle.len(), 1, "the second read took the first's transaction");
    // Reads within reads each hold a transaction of their own
    nested(&store, MAX_IDLE + 1).unwrap();
    assert_eq!(store.idle.len(), MAX_IDLE);
}

/// Creates at `path` an elmdb database, stamped, with only the graph databases named
fn stamped_with_graphs(path: &Path, graphs: &[&str]) {
    // SAFETY: a test's own new directory, opened once
    let env = unsafe { EnvOpenOptions::new().max_dbs(8).open(path) }.unwrap();
    let wtxn = env.write_txn().unwrap();
    env.create_databases_and_commit(wtxn, |opener, wtxn| -> Result<_> {
        let db = opener.create_database::<Bytes, Bytes>(wtxn, Some(format::DB_NAME))?;
        put_meta(wtxn, db, meta::KIND, format::KIND.as_bytes())?;
        put_meta(wtxn, db, meta::FORMAT, &format::FORMAT_VERSION.to_be_bytes())?;
        for name in graphs {
            opener.create_database::<Bytes, Bytes>(wtxn, Some(name))?;
        }
        Ok(CommitOrAbort::<_, ()>::Commit(()))
    })
    .unwrap();
    env.prepare_for_closing().wait();
}

#[test]
fn a_writable_open_creates_the_graph_databases_it_lacks_and_a_read_only_one_opens_without_them() {
    let dir = tempfile::tempdir().unwrap();
    stamped_with_graphs(dir.path(), &[]);
    let store = Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() }).unwrap();
    assert!(store.db.cosine.is_none() && store.db.euclidean.is_none());
    store.close().unwrap();

    let dir = tempfile::tempdir().unwrap();
    stamped_with_graphs(dir.path(), &[format::COSINE_GRAPHS]);
    let store = Store::open(dir.path(), quiet()).unwrap();
    assert!(store.db.cosine.is_some() && store.db.euclidean.is_some(), "the one missing is created");
}

#[test]
fn a_unit_grows_the_map_first_once_the_map_is_more_than_half_used() {
    let dir = tempfile::tempdir().unwrap();
    let page = 16 << 10;
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(64 * page), ..quiet() }).unwrap();
    let half_used = |store: &Store| {
        let page = store.env.unnamed_database_stat().page_size as usize;
        (store.env.info().last_page_number + 1) * page * 2 > store.env.info().map_size
    };
    let mut n = 0u64;
    while !half_used(&store) {
        store.write(|txn, db| db.put(txn, &n.to_be_bytes(), &[1u8; 512]).map_err(Into::into), false).unwrap();
        n += 1;
    }
    let before = store.map_size();
    let held = store.begin().unwrap();
    held.abort();
    store.end_unit();
    assert!(store.map_size() > before, "the map grew from {before} before the unit began");
}

#[test]
fn room_is_made_by_growing_the_map_to_as_many_times_what_is_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(1 << 20), ..quiet() }).unwrap();
    let page = store.env.unnamed_database_stat().page_size as usize;
    let used = |store: &Store| (store.env.info().last_page_number + 1) * page;
    // With three times what is in use already, nothing grows
    let before = store.map_size();
    assert!(used(&store) * 3 <= before);
    store.make_room(3).unwrap();
    assert_eq!(store.map_size(), before);
    // Filled to more than a third and less than half, where no unit would grow it
    let mut n = 0u64;
    while used(&store) * 3 <= store.map_size() {
        store.write(|txn, db| db.put(txn, &n.to_be_bytes(), &[1u8; 512]).map_err(Into::into), false).unwrap();
        n += 1;
    }
    assert!(used(&store) * 2 <= store.map_size() && store.map_size() == before);
    store.make_room(3).unwrap();
    assert!(store.map_size() > before && used(&store) * 3 <= store.map_size());
    // And as many times as asked, however many growths that takes
    store.make_room(9).unwrap();
    assert!(used(&store) * 9 <= store.map_size());
    let grown = store.map_size();
    store.make_room(9).unwrap();
    assert_eq!(store.map_size(), grown);
    // With a unit open nothing grows, and the call returns: a growth would wait for the unit to end
    let held = store.begin().unwrap();
    store.make_room(1000).unwrap();
    held.abort();
    store.end_unit();
    assert_eq!(store.map_size(), grown);
    drop(store);
    // A growth refused for any other reason is the caller's to hear: a read-only store grows nothing, and says why
    let read_only = Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() }).unwrap();
    assert_eq!(read_only.make_room(1000).unwrap_err().code, ErrorCode::ReadOnly);
}

#[test]
fn a_store_closing_at_exit_reports_a_sync_that_failed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::default()).unwrap();
    put(&store, "a", b"1").unwrap();
    let _failing = crate::failpoints::fail_while_at(crate::failpoints::points::SYNC, store.path());
    let error = store.close_at_exit(Duration::ZERO).unwrap_err();
    assert_eq!(error.code, ErrorCode::Io);
}

/// What `grow_elsewhere` runs as a child of this test binary: writes well past its parent's map, in two commits, and
/// closes
#[test]
#[ignore = "run only by the tests that grow a database from another process, as their child process"]
fn child_grower() {
    let Ok(path) = std::env::var("ELMDB_GROW_DB") else { return };
    // In the parent's mode: a writer of another is refused while the parent has the database open
    let options = if std::env::var("ELMDB_GROW_GROUPED").is_ok() { StoreOptions::default() } else { quiet() };
    let store = Store::open(Path::new(&path), options).unwrap();
    let value = vec![9u8; 64 << 10];
    store
        .write(
            |txn, db| {
                for i in 0..64u32 {
                    db.put(txn, &i.to_be_bytes(), &value)?;
                }
                Ok(())
            },
            false,
        )
        .unwrap();
    put(&store, "grown", b"yes").unwrap();
    store.close().unwrap();
}

#[test]
fn a_read_adopts_the_map_another_process_grew_the_file_past() {
    let dir = tempfile::tempdir().unwrap();
    let page = 16 << 10;
    let store = Arc::new(Store::open(dir.path(), StoreOptions { map_size_bytes: Some(16 * page), ..quiet() }).unwrap());
    put(&store, "a", b"1").unwrap();
    grow_elsewhere(dir.path());
    let before = store.map_size();
    let (sent, read) = std::sync::mpsc::channel();
    {
        let store = Arc::clone(&store);
        std::thread::spawn(move || sent.send(get(&store, "grown")).unwrap());
    }
    let value = read.recv_timeout(Duration::from_secs(30)).expect("the read adopted the grown map");
    assert_eq!(value.as_deref(), Some(&b"yes"[..]));
    assert!(store.map_size() > before);
}

#[test]
fn a_unit_that_finds_the_map_grown_elsewhere_holds_no_turn_while_it_adopts_the_size() {
    use crate::failpoints::{self, points};
    use std::cell::Cell;
    use std::rc::Rc;
    let dir = tempfile::tempdir().unwrap();
    let small = 1 << 20;
    let store = Arc::new(Store::open(dir.path(), StoreOptions { map_size_bytes: Some(small), ..quiet() }).unwrap());
    put(&store, "a", b"1").unwrap();
    // The unit has reserved its place, with room in the map as it is, when another process writes past the map
    let path = dir.path().to_path_buf();
    failpoints::run_once_here(points::UNIT_RESERVED, move || {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "store::tests::child_grower", "--nocapture", "--test-threads=1"])
            .env("ELMDB_GROW_DB", &path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "the child's write failed");
    });
    // It finds the map grown as it takes the write transaction, and goes on to adopt the size, which takes the gate
    // for writing and so waits for this process's reads. It keeps no turn while it waits: on macOS the turn keeps
    // the file lock other processes' writers wait for
    let free = Rc::new(Cell::new(None));
    failpoints::run_once_here(points::UNIT_MAP_GROWN, {
        let (store, free) = (Arc::clone(&store), Rc::clone(&free));
        move || free.set(Some(store.writers.turn_is_free()))
    });
    let held = store.begin().unwrap();
    held.abort();
    store.end_unit();
    assert_eq!(free.get(), Some(true), "whether the unit found the map grown, and its turn free by then");
    assert!(store.map_size() > small, "the unit adopted the map the other process grew");
}

// The slot's lock is the models' own under Loom and Shuttle, which ask the slot's decisions and not this
#[cfg(not(any(loom, shuttle)))]
#[test]
fn a_reset_s_checkpoint_takes_its_turn_where_no_unit_can_reserve_its_place() {
    use crate::failpoints::{self, points};
    use std::cell::Cell;
    use std::rc::Rc;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), quiet()).unwrap());
    put(&store, "a", b"1").unwrap();
    // A unit that reserved its place between the checkpoint's look at the slot and its turn would keep the turn for
    // as long as it stayed open, with the checkpoint waiting for it: the turn is taken with the slot's lock held
    let claimed = Rc::new(Cell::new(None));
    failpoints::run_once_here(points::RESET_CLAIMING, {
        let (store, claimed) = (Arc::clone(&store), Rc::clone(&claimed));
        move || claimed.set(Some(store.unit.is_being_claimed()))
    });
    store.recycle_reset_pages().unwrap();
    assert_eq!(claimed.get(), Some(true), "whether the checkpoint took its turn, and under the slot's lock");
}

#[test]
fn a_reset_s_checkpoint_is_refused_while_a_unit_is_open_and_runs_once_it_has_ended() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    let held = store.begin().unwrap();
    // Refused, on the thread that holds the unit's turn: a checkpoint that went on to wait its turn would wait for good
    let refused = store.recycle_reset_pages().unwrap_err();
    assert_eq!(refused.code, ErrorCode::UnitOpen, "{refused}");
    held.abort();
    store.end_unit();
    store.recycle_reset_pages().unwrap();
    assert_eq!(get(&store, "a").as_deref(), Some(&b"1"[..]));
}

#[test]
fn a_commit_is_synced_on_its_own_once_writes_pause() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            sync_idle: Duration::from_millis(20),
            sync_interval: Duration::from_secs(60),
            ..Default::default()
        },
    )
    .unwrap();
    put(&store, "a", b"1").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while store.syncs() == 0 {
        assert!(std::time::Instant::now() < deadline, "no sync followed the idle time");
        std::thread::sleep(Duration::from_millis(5));
    }
    store.close().unwrap();
}

/// Saturates the reader table without exiting cleanly, so the parent can distinguish live and stale slots.
#[test]
#[ignore = "run only by the reader-recovery tests, as their child process"]
fn child_holds_every_reader_slot() {
    use std::io::Write;
    let path = std::env::var("ELMDB_READER_RECOVERY_DB").expect("reader-recovery parent supplies its database");
    let mut options = EnvOpenOptions::new().read_txn_without_tls();
    options.max_dbs(8);
    // SAFETY: this is a child process opening the parent's LMDB environment through LMDB's locks.
    let env = unsafe { options.open(path) }.unwrap();
    let mut held = Vec::new();
    loop {
        match env.read_txn() {
            Ok(txn) => held.push(txn),
            Err(heed::Error::Mdb(heed::MdbError::ReadersFull)) => break,
            Err(error) => panic!("reader saturation failed: {error}"),
        }
    }
    assert_eq!(held.len(), env.max_readers() as usize);
    println!("ELMDB_READERS_FULL_READY");
    std::io::stdout().flush().unwrap();
    // The parent's kill deliberately skips the read transactions' destructors. A failed/killed parent
    // must not leave its child behind indefinitely.
    std::thread::sleep(Duration::from_secs(20));
    panic!("reader-recovery parent did not terminate its child");
}

struct SaturatedReaders(Option<std::process::Child>);

impl SaturatedReaders {
    fn start(path: &Path) -> Self {
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "store::tests::child_holds_every_reader_slot", "--nocapture"])
            .env("ELMDB_READER_RECOVERY_DB", path)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = Self(Some(child));
        let stdout = BufReader::new(child.0.as_mut().unwrap().stdout.take().unwrap());
        assert!(
            stdout.lines().any(|line| line.unwrap().contains("ELMDB_READERS_FULL_READY")),
            "child must fill the reader table"
        );
        child
    }

    fn kill_and_wait(&mut self) {
        let mut child = self.0.take().unwrap();
        #[cfg(windows)]
        let pid = child.id();
        child.kill().unwrap();
        child.wait().unwrap();
        // Windows keeps a process object alive while a Child handle remains open. Close our handle
        // before asking LMDB to prove that the reader process no longer exists.
        drop(child);
        #[cfg(windows)]
        wait_until_reader_process_is_absent(pid);
    }
}

#[cfg(windows)]
fn wait_until_reader_process_is_absent(pid: u32) {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        // SAFETY: OpenProcess returns a handle owned here; all output pointers are valid for the call,
        // and every successful open is closed before retrying. No other process is modified.
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                assert_eq!(GetLastError(), ERROR_INVALID_PARAMETER, "only an absent PID establishes this precondition");
                break;
            }
            let mut exit_code = 0;
            let queried = GetExitCodeProcess(process, &mut exit_code);
            let query_error = if queried == 0 { Some(GetLastError()) } else { None };
            CloseHandle(process);
            assert!(query_error.is_none(), "reader exit-code query failed: {query_error:?}");
            assert_ne!(exit_code, STILL_ACTIVE as u32, "the killed reader must have exited");
            assert!(
                std::time::Instant::now() < deadline,
                "reader PID {pid} still exists after exit (code {exit_code})"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Drop for SaturatedReaders {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn a_read_refuses_live_reader_exhaustion_then_recovers_the_killed_readers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "key", b"value").unwrap();
    let mut readers = SaturatedReaders::start(dir.path());
    let error = store.read(|_, _| Ok(())).unwrap_err();
    assert_eq!(error.code, ErrorCode::ReadersFull, "live readers must not be cleared");
    readers.kill_and_wait();
    assert_eq!(get(&store, "key").as_deref(), Some(b"value".as_slice()));
}

#[test]
fn a_read_only_open_recovers_slots_left_by_a_killed_reader() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "key", b"value").unwrap();
    store.close().unwrap();
    let mut readers = SaturatedReaders::start(dir.path());
    // Keep the environment mapped until after the child dies: the first opener otherwise resets the table.
    let mut options = EnvOpenOptions::new().read_txn_without_tls();
    options.max_dbs(8);
    // SAFETY: the Store has closed; this is the only environment handle in this process.
    let env = unsafe { options.open(dir.path()) }.unwrap();
    readers.kill_and_wait();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::tests::child_opens_after_reader_death", "--nocapture"])
        .env("ELMDB_READER_RECOVERY_DB", dir.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ELMDB_READER_RECOVERY_OPENED"),
        "the child must really open and read"
    );
    drop(env);
}

#[test]
fn reader_recovery_retries_only_once_and_only_readers_full() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    let mut calls = 0;
    let result: heed::Result<()> = with_reader_recovery(&store.env, || {
        calls += 1;
        Err(heed::Error::Mdb(heed::MdbError::ReadersFull))
    });
    assert!(matches!(result, Err(heed::Error::Mdb(heed::MdbError::ReadersFull))));
    assert_eq!(calls, 2);
    let mut calls = 0;
    let result: heed::Result<()> = with_reader_recovery(&store.env, || {
        calls += 1;
        Err(heed::Error::Mdb(heed::MdbError::BadTxn))
    });
    assert!(matches!(result, Err(heed::Error::Mdb(heed::MdbError::BadTxn))));
    assert_eq!(calls, 1);
}

#[test]
#[ignore = "run only by the reader-recovery open test, as its child process"]
fn child_opens_after_reader_death() {
    let path = std::env::var("ELMDB_READER_RECOVERY_DB").expect("reader-recovery parent supplies its database");
    let store = Store::open(Path::new(&path), StoreOptions { read_only: true, ..quiet() }).unwrap();
    assert_eq!(get(&store, "key").as_deref(), Some(b"value".as_slice()));
    println!("ELMDB_READER_RECOVERY_OPENED");
}

#[test]
fn a_database_lets_go_of_the_read_it_keeps_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let db = crate::Database::open(dir.path(), crate::OpenOptions { syncs: false, ..Default::default() }).unwrap();
    let store = db.store().unwrap();
    store.read(|_, _| Ok(())).unwrap();
    assert_eq!(store.idle.len(), 1);
    db.release_idle_reads().unwrap();
    assert_eq!(store.idle.len(), 0);
}

#[test]
fn a_replacement_that_failed_before_committing_vouches_for_no_later_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    // A replacement that moves the epoch and then fails, as a reset whose index settling fails does
    let failed = store.write(
        |txn, db| {
            store.replacing_every_row(txn, db)?;
            Err::<(), _>(Error::new(ErrorCode::Io, "the replacement failed"))
        },
        false,
    );
    assert!(failed.is_err());
    // Another process replaces every row, moving the stored epoch to the same number
    let mut txn = store.env.write_txn().unwrap();
    let next = stored_epoch(&txn, store.db).unwrap() + 1;
    put_meta(&mut txn, store.db.main, meta::EPOCH, &next.to_be_bytes()).unwrap();
    txn.commit().unwrap();
    assert_eq!(
        put(&store, "a", b"2").unwrap_err().code,
        ErrorCode::DataReplaced,
        "refused, the replacement not its own"
    );
    let held = store.begin().unwrap();
    assert_eq!(held.commit(&store).unwrap_err().code, ErrorCode::DataReplaced, "a unit refused too");
    store.end_unit();
}

#[test]
fn a_unit_s_commit_lets_the_gate_go_before_what_runs_once_it_landed_and_keeps_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    let mut held = store.begin().unwrap();
    put_meta(&mut held.txn, store.db.main, "probe", b"1").unwrap();
    let (mut adoptable, mut writer_free) = (None, None);
    held.commit_then(&store, || {
        // What runs here may wait on a thread whose read waits for a growth to be taken, which takes the gate for
        // writing: it has to be free. The writer isn't, so no later writer of this process commits first
        std::thread::scope(|scope| {
            adoptable = Some(scope.spawn(|| super::gate_to_adopt(&store.gate).is_some()).join().unwrap());
            writer_free = Some(store.writers.turn_is_free());
        });
    })
    .unwrap();
    store.end_unit();
    assert_eq!(adoptable, Some(true), "the gate is free once the commit has landed");
    assert_eq!(writer_free, Some(false), "the writer is held until what runs once it landed has run");
}

#[test]
fn asking_whether_another_committed_adopts_the_map_another_process_grew_the_file_past() {
    let dir = tempfile::tempdir().unwrap();
    let page = 16 << 10;
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(16 * page), ..quiet() }).unwrap();
    put(&store, "a", b"1").unwrap();
    assert!(!store.committed_elsewhere().unwrap());
    grow_elsewhere(dir.path());
    let before = store.map_size();
    // The first this handle hears of the growth, as a handle that asks before it reads does
    assert!(store.committed_elsewhere().unwrap(), "the child committed, past the map this handle had");
    assert!(store.map_size() > before);
    assert_eq!(get(&store, "grown").as_deref(), Some(&b"yes"[..]));
}

#[test]
fn what_reads_through_the_map_waits_for_a_resize_of_it_to_end() {
    let dir = tempfile::tempdir().unwrap();
    // Syncs on, and none on the worker's own account, so the one below is the one asked for
    let patient = Duration::from_secs(600);
    let options = StoreOptions { sync_idle: patient, sync_interval: patient, ..Default::default() };
    let store = Arc::new(Store::open(dir.path(), options).unwrap());
    type Read = fn(&Store);
    let reads: [(&str, Read); 4] = [
        ("the map's size", |store| {
            store.map_size();
        }),
        ("whether the map is half used", |store| {
            store.map_half_used();
        }),
        ("whether another handle committed", |store| {
            store.committed_elsewhere().unwrap();
        }),
        ("a sync", |store| store.durable().expect("the store syncs").wait().unwrap()),
    ];
    for (what, read) in reads {
        put(&store, "a", what.as_bytes()).unwrap();
        // As `grow` and `adopt_map_size` hold it while LMDB unmaps the file and maps it again
        let resizing = store.gate.write();
        let (done, waited) = std::sync::mpsc::channel();
        let reader = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                read(&store);
                done.send(()).unwrap();
            })
        };
        assert!(waited.recv_timeout(Duration::from_millis(300)).is_err(), "{what} was read while the map was resized");
        drop(resizing);
        waited.recv_timeout(Duration::from_secs(30)).unwrap_or_else(|_| panic!("{what} is read once the resize ends"));
        reader.join().unwrap();
    }
    // A process that is exiting waits on no thread, one resizing the map among them: it syncs what it can read
    put(&store, "a", b"owed").unwrap();
    let resizing = store.gate.write();
    let deadline = || std::time::Instant::now() + Duration::from_secs(1);
    assert_eq!(store.sync_owed(deadline()).unwrap_err().code, ErrorCode::Io);
    drop(resizing);
    store.sync_owed(deadline()).unwrap();
}
/// A store of many rows, of which `destroy` removes them all in one commit, and its data file's path
fn filled(dir: &tempfile::TempDir, destroy: bool) -> std::path::PathBuf {
    {
        let store = Store::open(dir.path(), quiet()).unwrap();
        for n in 0..200u32 {
            put(&store, &format!("key-{n:03}"), &[7u8; 2000]).unwrap();
        }
        if destroy {
            store
                .write(
                    |txn, db| {
                        for n in 0..200u32 {
                            db.delete(txn, format!("key-{n:03}").as_bytes())?;
                        }
                        Ok(())
                    },
                    false,
                )
                .unwrap();
        }
    }
    dir.path().join("data.mdb")
}

/// The highest page the trees reach in a file as it is, and the last page of its meta page's pages
fn reached_in(dir: &tempfile::TempDir, data: &std::path::Path) -> (u64, u64) {
    let store = Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() }).unwrap();
    let last = store.env.info().last_page_number as u64;
    let snapshot = super::read_txn(&store.env).unwrap();
    let mut file = std::fs::File::open(data).unwrap();
    let page = super::page_size_of(&mut file, data).unwrap() as u64;
    let meta = super::reach::meta(&mut file, page as usize, &snapshot).unwrap();
    let pages = std::fs::metadata(data).unwrap().len() / page;
    let reached = super::reach::reach(&mut file, page as usize, pages, &meta, data).unwrap();
    assert!(reached.missing.is_none(), "the whole file holds every page the trees reach");
    (reached.highest, last)
}

#[test]
fn a_short_file_missing_a_page_the_trees_reach_is_refused_in_both_modes() {
    // Cut short of its last page, the file lacks a page the meta page names. The open refuses exactly when the trees reach it
    let dir = tempfile::tempdir().unwrap();
    let data = filled(&dir, false);
    let (highest, last) = reached_in(&dir, &data);
    let page = super::page_size_of(&mut std::fs::File::open(&data).unwrap(), &data).unwrap() as u64;
    let file = std::fs::OpenOptions::new().write(true).open(&data).unwrap();
    file.set_len(last * page).unwrap();
    drop(file);
    for options in [StoreOptions { read_only: true, ..quiet() }, quiet()] {
        let outcome = Store::open(dir.path(), options).err();
        assert_eq!(outcome.is_some(), highest == last, "reached the last page: {}", highest == last);
        if let Some(error) = outcome {
            assert_eq!(error.code, ErrorCode::Corrupted, "{error}");
            assert!(error.message.contains("past its end"), "{error}");
        }
    }
}

/// A store whose trees, emptied again and again, leave the file's last pages free: each round writes rows in one commit
/// and deletes them in another, and the rounds stop once the trees reach fewer pages than the file's meta page names
fn drained(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let data = dir.path().join("data.mdb");
    let store = Store::open(dir.path(), quiet()).unwrap();
    for round in 0..60u32 {
        store
            .write(
                |txn, db| {
                    for n in 0..200u32 {
                        db.put(txn, format!("key-{round}-{n:03}").as_bytes(), &[7u8; 2000])?;
                    }
                    Ok(())
                },
                false,
            )
            .unwrap();
        store
            .write(
                |txn, db| {
                    for n in 0..200u32 {
                        db.delete(txn, format!("key-{round}-{n:03}").as_bytes())?;
                    }
                    Ok(())
                },
                false,
            )
            .unwrap();
        let last = store.env.info().last_page_number as u64;
        let snapshot = super::read_txn(&store.env).unwrap();
        let mut file = std::fs::File::open(&data).unwrap();
        let page = super::page_size_of(&mut file, &data).unwrap();
        let meta = super::reach::meta(&mut file, page, &snapshot).unwrap();
        let reached = super::reach::reach(&mut file, page, last + 1, &meta, &data).unwrap();
        if reached.missing.is_none() && reached.highest < last {
            break;
        }
    }
    data
}

/// Counts on this thread each time an open syncs a commit of its own, from now on
fn count_open_syncs(count: std::rc::Rc<std::cell::Cell<u32>>) {
    crate::failpoints::run_once_here(crate::failpoints::points::OPEN_SYNCED, move || {
        count.set(count.get() + 1);
        count_open_syncs(std::rc::Rc::clone(&count));
    });
}

#[test]
fn an_open_syncs_each_commit_it_makes_itself() {
    let dir = tempfile::tempdir().unwrap();
    let synced = std::rc::Rc::new(std::cell::Cell::new(0));
    count_open_syncs(std::rc::Rc::clone(&synced));
    // A new environment's stamp, then the graphs' databases: two commits of the open's own, which group commit never
    // hears of, each synced before the open goes on. So a database made and closed has no commit left unsynced
    Store::open(dir.path(), quiet()).unwrap().close().unwrap();
    assert_eq!(synced.get(), 2);
    // Opened again the environment has both: the open commits nothing, and syncs nothing
    Store::open(dir.path(), quiet()).unwrap().close().unwrap();
    assert_eq!(synced.get(), 2);
}

#[test]
fn a_short_file_whose_tail_the_trees_do_not_reach_opens_and_a_writable_open_extends_it() {
    // Emptied, the trees reach far fewer pages than the file holds, and LMDB leaves the free tail out of the file. Such a
    // file is healthy: cut to the pages the trees reach, it opens read-only and a writable open extends it again
    let dir = tempfile::tempdir().unwrap();
    let data = drained(&dir);
    let (highest, last) = reached_in(&dir, &data);
    assert!(highest < last, "the drained trees leave a free tail: reached {highest}, last {last}");
    let page = super::page_size_of(&mut std::fs::File::open(&data).unwrap(), &data).unwrap() as u64;
    let file = std::fs::OpenOptions::new().write(true).open(&data).unwrap();
    file.set_len((highest + 1) * page).unwrap();
    drop(file);
    Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() })
        .expect("a read-only open of a healthy short file");
    assert_eq!(std::fs::metadata(&data).unwrap().len(), (highest + 1) * page, "a read-only open does not extend it");
    drop(Store::open(dir.path(), quiet()).expect("a writable open of a healthy short file"));
    assert_eq!(std::fs::metadata(&data).unwrap().len(), (last + 1) * page, "extended to the pages its meta page names");
}

/// A drained database's file cut to the pages its trees reach, short of the pages its meta page names and healthy: the
/// file, its page size, and the last of those pages
fn short_and_healthy(dir: &tempfile::TempDir) -> (std::path::PathBuf, u64, u64) {
    let data = drained(dir);
    let (highest, last) = reached_in(dir, &data);
    assert!(highest < last, "the drained trees leave a free tail: reached {highest}, last {last}");
    let page = super::page_size_of(&mut std::fs::File::open(&data).unwrap(), &data).unwrap() as u64;
    std::fs::OpenOptions::new().write(true).open(&data).unwrap().set_len((highest + 1) * page).unwrap();
    (data, page, last)
}

/// Another process opens the database in `dir`, writes some 4 MiB in two commits, and closes it
fn grow_elsewhere(dir: &Path) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::tests::child_grower", "--nocapture", "--test-threads=1"])
        .env("ELMDB_GROW_DB", dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the child's write failed");
}

#[test]
fn a_writable_open_cuts_nothing_another_process_wrote_after_the_file_was_found_short() {
    use crate::failpoints::{self, points};
    use std::cell::Cell;
    use std::rc::Rc;
    let dir = tempfile::tempdir().unwrap();
    let (data, page, last) = short_and_healthy(&dir);
    // The open has read the short file's trees, and has yet to extend it, when another process writes past the pages
    // its meta page named
    let written = Rc::new(Cell::new(0));
    failpoints::run_once_here(points::OPEN_WALKED, {
        let (dir, data, written) = (dir.path().to_path_buf(), data.clone(), Rc::clone(&written));
        move || {
            grow_elsewhere(&dir);
            written.set(std::fs::metadata(&data).unwrap().len());
        }
    });
    // The open's map is no larger than the file it found, so what the other process wrote is past it: the write
    // transaction the file is extended under is the first the open begins after, and takes the grown map
    let found_grown = Rc::new(Cell::new(false));
    failpoints::run_once_here(points::OPEN_MAP_GROWN, {
        let found_grown = Rc::clone(&found_grown);
        move || found_grown.set(true)
    });
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(1 << 20), ..quiet() })
        .expect("a writable open of a healthy short file");
    assert!(written.get() > (last + 1) * page, "the other process wrote past the pages the open had read of");
    assert!(found_grown.get(), "the open began its write transaction in the map the other process had outgrown");
    let held = std::fs::metadata(&data).unwrap().len();
    assert!(held >= written.get(), "the open cut the file to {held} bytes, of the {} written", written.get());
    assert_eq!(get(&store, "grown").as_deref(), Some(&b"yes"[..]));
}

#[test]
fn an_open_takes_the_map_another_process_grows_the_file_past_while_it_opens() {
    use crate::failpoints::{self, points};
    use std::cell::Cell;
    use std::rc::Rc;
    // Grown before the open's first transaction, which for a read-only open is the one its reader is registered by
    // and for a writable one the snapshot its walk reads at, and grown after the walk, before the databases are opened
    let cases = [points::OPEN_ENV, points::OPEN_WALKED].into_iter().flat_map(|point| [(point, false), (point, true)]);
    for (point, read_only) in cases {
        let dir = tempfile::tempdir().unwrap();
        let small = 1 << 20;
        let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(small), ..quiet() }).unwrap();
        put(&store, "a", b"1").unwrap();
        store.close().unwrap();
        // The open holds the file in a map of 1 MiB when another process writes some 4 MiB: each transaction it
        // begins from then on is refused in the map as it was opened, until it takes the new size
        failpoints::run_once_here(point, {
            let dir = dir.path().to_path_buf();
            move || grow_elsewhere(&dir)
        });
        let found_grown = Rc::new(Cell::new(false));
        failpoints::run_once_here(points::OPEN_MAP_GROWN, {
            let found_grown = Rc::clone(&found_grown);
            move || found_grown.set(true)
        });
        let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(small), read_only, ..quiet() })
            .unwrap_or_else(|e| panic!("an open while another process grows the file at {point}: {e}"));
        assert!(found_grown.get(), "the open began a transaction in the map the other process had outgrown");
        assert_eq!(get(&store, "grown").as_deref(), Some(&b"yes"[..]));
        assert!(store.map_size() > small);
    }
}

#[test]
fn an_open_reads_the_trees_at_a_later_commit_when_its_own_s_meta_page_has_been_written_over() {
    use crate::failpoints::{self, points};
    use std::cell::Cell;
    use std::rc::Rc;
    let dir = tempfile::tempdir().unwrap();
    short_and_healthy(&dir);
    // The open holds its snapshot, and has yet to read that commit's meta page from the file, when another process
    // makes two commits: each writes one of the two meta pages, so the snapshot's is gone. The open takes a snapshot
    // again, which the point armed for the second time says
    let again = Rc::new(Cell::new(false));
    failpoints::run_once_here(points::OPEN_SNAPSHOT, {
        let (dir, again) = (dir.path().to_path_buf(), Rc::clone(&again));
        move || {
            grow_elsewhere(&dir);
            failpoints::run_once_here(points::OPEN_SNAPSHOT, move || again.set(true));
        }
    });
    let store = Store::open(dir.path(), quiet()).expect("the open read the trees at a later commit");
    assert!(again.get(), "the open took a second snapshot");
    assert_eq!(get(&store, "grown").as_deref(), Some(&b"yes"[..]));
}

#[test]
fn an_open_refuses_a_file_that_lost_its_commit_s_meta_page_to_no_commit() {
    use crate::failpoints::{self, points};
    use std::io::{Seek, SeekFrom, Write};
    let dir = tempfile::tempdir().unwrap();
    let data = filled(&dir, false);
    let page = super::page_size_of(&mut std::fs::File::open(&data).unwrap(), &data).unwrap() as u64;
    // As the open holds its snapshot, both meta pages stop being LMDB's, with no commit made: nothing to read the
    // trees at, now or at a later snapshot
    failpoints::run_once_here(points::OPEN_SNAPSHOT, {
        let data = data.clone();
        move || {
            let mut file = std::fs::OpenOptions::new().write(true).open(&data).unwrap();
            for meta in [0, page] {
                file.seek(SeekFrom::Start(meta + 16)).unwrap();
                file.write_all(&[0; 4]).unwrap();
            }
        }
    });
    let error = Store::open(dir.path(), quiet()).err().expect("refused, and not asked again for ever");
    assert_eq!(error.code, ErrorCode::Corrupted, "{error}");
    let told = "holds no meta page of the commit LMDB opened it at, and no commit has landed since: something other \
                than a database handle changed the file as it was opened. Open it again once nothing else writes it";
    assert!(error.message.ends_with(told), "{error}");
}

#[test]
fn a_file_that_holds_every_page_its_meta_page_names_is_not_read_for_its_trees() {
    let dir = tempfile::tempdir().unwrap();
    let data = filled(&dir, false);
    // A writable open leaves the file whole, whatever tail LMDB left out of it
    drop(Store::open(dir.path(), quiet()).unwrap());
    let store = Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() }).unwrap();
    let mut file = std::fs::File::open(&data).unwrap();
    let page = super::page_size_of(&mut file, &data).unwrap();
    assert_eq!(std::fs::metadata(&data).unwrap().len(), (store.env.info().last_page_number as u64 + 1) * page as u64);
    let opening = super::Opening { env: &store.env, dir: store.path(), read_only: false, kept: None };
    assert!(!super::short_of_its_pages(&opening, &mut file, page, &data).unwrap(), "a whole file is not short");
}

#[test]
fn a_short_file_whose_last_page_is_an_overflow_page_the_trees_reach_is_refused() {
    // A value larger than a page lives on overflow pages, which the trees reach by the node that names them. When the
    // file's last page is one of them, cutting it short is a file the open refuses
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path(), quiet()).unwrap();
        put(&store, "big", &vec![9u8; 200_000]).unwrap();
    }
    let data = dir.path().join("data.mdb");
    let (highest, last) = reached_in(&dir, &data);
    assert_eq!(highest, last, "the big value's overflow pages are the file's last, and the trees reach them");
    let page = super::page_size_of(&mut std::fs::File::open(&data).unwrap(), &data).unwrap() as u64;
    let file = std::fs::OpenOptions::new().write(true).open(&data).unwrap();
    file.set_len(last * page).unwrap();
    drop(file);
    let error = Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() }).err().expect("refused");
    assert_eq!(error.code, ErrorCode::Corrupted, "{error}");
}

#[test]
fn a_short_file_whose_tree_root_is_torn_is_refused_whichever_tree_it_is() {
    // The meta page names each tree's root, which the trees reach by definition. Zeroed in a file cut short before its last
    // page, each root is refused, the free tree's and the main tree's alike: the expectation comes from the meta page read
    // here, not from the walk under test
    let dir = tempfile::tempdir().unwrap();
    let data = filled(&dir, false);
    let (_, last) = reached_in(&dir, &data);
    let image = std::fs::read(&data).unwrap();
    let page = super::page_size_of(&mut std::fs::File::open(&data).unwrap(), &data).unwrap();
    let txnid =
        Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() }).unwrap().env.info().last_txn_id as u64;
    let (free, main) = meta_roots(&image, page, txnid);
    for root in [free, main] {
        assert!(root != u64::MAX && root < last, "the root {root} is an inner page of the file, below its last {last}");
        let mut torn = image[..(last as usize) * page].to_vec();
        torn[(root as usize) * page..(root as usize + 1) * page].fill(0);
        std::fs::write(&data, &torn).unwrap();
        let error =
            Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() }).err().expect("a torn root is refused");
        assert_eq!(error.code, ErrorCode::Corrupted, "{error}");
        assert!(error.message.contains("carry its own number"), "root {root}: {error}");
    }
}

/// The free and main roots of the meta page written by `txnid`, read from the file's bytes at the offsets LMDB writes them
fn meta_roots(image: &[u8], psize: usize, txnid: u64) -> (u64, u64) {
    for at in [0, psize] {
        let field = |offset: usize| u64::from_ne_bytes(image[at + offset..at + offset + 8].try_into().unwrap());
        if field(144) == txnid {
            return (field(80), field(128));
        }
    }
    panic!("no meta page of transaction {txnid}");
}

#[test]
fn a_value_overwritten_in_place_by_a_shorter_one_leaves_a_longer_run_the_walk_accepts() {
    // Two values of one key in one commit: the second, of two pages, overwrites the first's run of three in place, and the
    // run keeps its count. The walk reads that run as its value's pages or more, never as a torn file
    let dir = tempfile::tempdir().unwrap();
    let data = {
        let store = Store::open(dir.path(), quiet()).unwrap();
        store
            .write(
                |txn, db| {
                    db.put(txn, b"big", &vec![1u8; 40_000])?;
                    db.put(txn, b"big", &vec![2u8; 20_000])?;
                    Ok(())
                },
                false,
            )
            .unwrap();
        dir.path().join("data.mdb")
    };
    let _ = reached_in(&dir, &data);
}

#[test]
fn a_reached_page_zeroed_in_a_short_file_is_refused_as_not_one_lmdb_wrote() {
    // Within the file, a page the trees reach that carries no page header is torn, and the walk refuses it rather than
    // letting a read see zeros
    let dir = tempfile::tempdir().unwrap();
    let data = filled(&dir, false);
    let (highest, _) = reached_in(&dir, &data);
    let page = super::page_size_of(&mut std::fs::File::open(&data).unwrap(), &data).unwrap() as u64;
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new().write(true).open(&data).unwrap();
        file.seek(SeekFrom::Start(highest * page)).unwrap();
        file.write_all(&vec![0u8; page as usize]).unwrap();
        file.set_len((highest + 1) * page).unwrap();
    }
    let error = Store::open(dir.path(), quiet()).err().expect("a torn reached page is refused");
    assert_eq!(error.code, ErrorCode::Corrupted, "{error}");
    assert!(error.message.contains("carry its own number"), "{error}");
}

#[test]
fn a_file_names_its_own_page_size_whatever_the_host_is() {
    // LMDB opens a file with the page size the file's meta page names, so a file of another host's page size is read at
    // its own. The host's size would be read here as 4 KiB or 16 KiB, never 8 KiB, and a size LMDB never writes is refused
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.mdb");
    let mut image = vec![0u8; 64];
    image[40..44].copy_from_slice(&8192u32.to_ne_bytes());
    std::fs::write(&path, &image).unwrap();
    assert_eq!(super::page_size_of(&mut std::fs::File::open(&path).unwrap(), &path).unwrap(), 8192);
    image[40..44].copy_from_slice(&3000u32.to_ne_bytes());
    std::fs::write(&path, &image).unwrap();
    let refused = super::page_size_of(&mut std::fs::File::open(&path).unwrap(), &path).expect_err("refused");
    assert_eq!(refused.code, ErrorCode::Corrupted, "{refused}");
}

#[test]
fn a_file_one_byte_short_of_a_page_its_trees_reach_is_refused_read_only_as_corrupt() {
    // Cut inside a page the trees reach, so the file is short of whole pages: refused as corrupt by the walk on every
    // platform, Windows' read-only view included, not as an I/O failure of the map
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path(), quiet()).unwrap();
        put(&store, "big", &vec![9u8; 200_000]).unwrap();
    }
    let data = dir.path().join("data.mdb");
    let (highest, last) = reached_in(&dir, &data);
    assert_eq!(highest, last, "the big value's overflow pages are the file's last, and the trees reach them");
    let page = super::page_size_of(&mut std::fs::File::open(&data).unwrap(), &data).unwrap() as u64;
    let file = std::fs::OpenOptions::new().write(true).open(&data).unwrap();
    file.set_len((last + 1) * page - 1).unwrap();
    drop(file);
    let refused = Store::open(dir.path(), StoreOptions { read_only: true, ..quiet() }).err().expect("refused");
    assert_eq!(refused.code, ErrorCode::Corrupted, "{refused}");
}

#[test]
fn the_walk_counts_the_main_tree_as_lmdb_counts_its_trees() {
    // The walk's count of the main tree, its sub-database and overflow runs must equal LMDB's own counts of the two trees
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    for n in 0..300u32 {
        put(&store, &format!("key-{n:04}"), &[7u8; 1500]).unwrap();
    }
    put(&store, "big", &vec![9u8; 200_000]).unwrap();
    let data = dir.path().join("data.mdb");
    let txn = super::read_txn(&store.env).unwrap();
    let main = store.env.unnamed_database_stat();
    let named = store.db.main.stat(&txn).unwrap();
    let lmdb = main.branch_pages
        + main.leaf_pages
        + main.overflow_pages
        + named.branch_pages
        + named.leaf_pages
        + named.overflow_pages;
    let info = store.env.info();
    let mut file = std::fs::File::open(&data).unwrap();
    let psize = super::page_size_of(&mut file, &data).unwrap();
    let walked = super::reach::main_tree_pages(
        &mut file,
        psize,
        info.last_page_number as u64 + 1,
        info.last_txn_id as u64,
        &data,
    )
    .unwrap();
    assert!(named.overflow_pages > 0, "the big value is on overflow pages, so the check covers them");
    assert_eq!(walked, lmdb, "the walk counts the main tree as LMDB does: main {main:?}, named {named:?}");
}

#[test]
fn the_page_size_read_from_a_real_file_is_the_one_lmdb_uses() {
    // The reader is checked against the page size LMDB reports for the open environment, not only a hand-built header
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "key", &[1u8; 64]).unwrap();
    let data = dir.path().join("data.mdb");
    let from_file = super::page_size_of(&mut std::fs::File::open(&data).unwrap(), &data).unwrap();
    assert_eq!(from_file, store.page_size());
}

#[test]
fn file_bytes_for_pages_and_map_size_arithmetic_reject_overflow() {
    let data = Path::new("data.mdb");
    assert_eq!(super::file_bytes_for_pages(10, 4096, data).unwrap(), 11 * 4096);
    assert_eq!(super::file_bytes_for_pages(u64::MAX, 4096, data).unwrap_err().code, ErrorCode::Corrupted);
    assert_eq!(super::file_bytes_for_pages(u64::MAX / 4096, 4096, data).unwrap_err().code, ErrorCode::Corrupted);

    assert_eq!(super::initial_map_size(u64::MAX), super::MAX_MAP_SIZE);
    assert!(
        super::MAX_MAP_SIZE > 0
            && super::MAX_MAP_SIZE.is_multiple_of(super::MAP_STEP)
            && super::MAX_MAP_SIZE > usize::MAX - super::MAP_STEP
    );
    assert_eq!(super::next_map_size(super::MAP_STEP), Some(2 * super::MAP_STEP));
    assert_eq!(super::next_map_size(usize::MAX), None);
    assert_eq!(super::next_map_size(usize::MAX / 2), None);
    assert_eq!(super::next_map_size(super::MAX_MAP_SIZE), None);
}

#[test]
fn sequence_at_u64_max_is_refused_as_corrupted() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    let mut txn = store.env.write_txn().unwrap();
    put_meta(&mut txn, store.db.main, meta::SEQUENCE, &u64::MAX.to_be_bytes()).unwrap();
    txn.commit().unwrap();
    let err = put(&store, "a", b"1").unwrap_err();
    assert_eq!(err.code, ErrorCode::Corrupted, "{err}");
    store.close().unwrap();
}

#[test]
fn panic_inside_write_aborts_txn_before_releasing_writer_and_gate() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "kept", b"1").unwrap();
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = store.write_committed::<()>(
            |txn, db| {
                db.put(txn, b"lost", b"2")?;
                panic!("simulated write panic");
            },
            false,
        );
    }));
    assert!(panicked.is_err());
    assert!(store.writers.turn_is_free(), "writer lock released on unwind");
    assert_eq!(get(&store, "lost"), None, "panicking write was aborted");
    store.grow().unwrap();
    put(&store, "after", b"3").unwrap();
    assert_eq!(get(&store, "after").as_deref(), Some(&b"3"[..]));
    store.close().unwrap();
}

#[test]
fn recycle_reset_pages_routes_through_group_commit_and_adopts_grown_map() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions {
            syncs: true,
            sync_idle: Duration::from_secs(600),
            sync_interval: Duration::from_secs(600),
            map_size_bytes: Some(256 << 10),
            ..Default::default()
        },
    )
    .unwrap();
    put(&store, "a", b"1").unwrap();
    let before_txnid = store.env.info().last_txn_id;
    let before_syncs = store.syncs();
    store.recycle_reset_pages().unwrap();
    assert!(store.env.info().last_txn_id > before_txnid, "recycle_reset_pages must commit a new LMDB transaction");
    assert!(store.syncs() > before_syncs, "recycle_reset_pages must account its commit and sync through GroupCommit");

    // Another process grows the map; recycle_reset_pages must adopt the grown map on MapResized and succeed
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::tests::child_grower", "--nocapture", "--test-threads=1"])
        .env("ELMDB_GROW_DB", dir.path())
        .env("ELMDB_GROW_GROUPED", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "child grower failed");
    let before = store.map_size();
    store.recycle_reset_pages().unwrap();
    assert!(store.map_size() > before, "recycle_reset_pages adopted the grown map");

    // A growth between ensure_pin and write_txn ends the pin; recycle_reset_pages must retry and re-pin before committing
    use crate::failpoints::{self, points};
    let store = std::sync::Arc::new(store);
    let grower = std::sync::Arc::clone(&store);
    let checker = std::sync::Arc::clone(&store);
    let re_pinned = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&re_pinned);
    failpoints::run_once_here(points::WRITE_PINNED, move || {
        grower.grow().unwrap();
        failpoints::run_once_here(points::RESET_CLAIMING, move || {
            let checker2 = std::sync::Arc::clone(&checker);
            let flag2 = std::sync::Arc::clone(&flag);
            failpoints::run_once_here(points::RESET_CLAIMING, move || {
                flag2.store(checker2.kept.as_ref().unwrap().pinned(), std::sync::atomic::Ordering::SeqCst);
            });
        });
    });
    store.recycle_reset_pages().unwrap();
    assert!(!failpoints::is_armed_here(points::WRITE_PINNED));
    assert!(
        re_pinned.load(std::sync::atomic::Ordering::SeqCst),
        "recycle_reset_pages must retry and re-pin after an intervening growth"
    );
    std::sync::Arc::try_unwrap(store).ok().expect("sole store owner").close().unwrap();
}

#[test]
fn store_drop_keeps_path_registered_until_env_closes_and_reopens_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "a", b"1").unwrap();
    assert_eq!(Store::open(dir.path(), quiet()).err().map(|e| e.code), Some(ErrorCode::AlreadyOpen));
    drop(store);
    let reopened = Store::open(dir.path(), quiet()).unwrap();
    assert_eq!(get(&reopened, "a").as_deref(), Some(&b"1"[..]));
    reopened.close().unwrap();
}

#[test]
fn sync_each_commit_writes_and_grows_cleanly_under_gate() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(
        dir.path(),
        StoreOptions { sync_each_commit: true, map_size_bytes: Some(256 << 10), ..Default::default() },
    )
    .unwrap();
    put(&store, "a", &vec![7u8; 400_000]).unwrap();
    assert_eq!(get(&store, "a").map(|v| v.len()), Some(400_000));
    store.close().unwrap();
}
