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
    assert_eq!(count, 32 + 3, "the unit's 32 entries, the stamp's two and the commit's sequence");
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
    let (txn, gate) = store.begin().unwrap();
    let ended = waiter(Arc::new(AtomicBool::new(false)));
    assert!(ended.recv_timeout(quiet_for).is_err(), "returned while the unit was open");
    txn.abort();
    drop(gate);
    store.end_unit();
    ended.recv_timeout(Duration::from_secs(5)).expect("returned once the unit ended");

    // Ended by its stop, the unit still open
    let (txn, gate) = store.begin().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let ended = waiter(Arc::clone(&stop));
    assert!(ended.recv_timeout(quiet_for).is_err(), "returned while the unit was open");
    stop.store(true, Ordering::SeqCst);
    store.wake_unit_waiters();
    ended.recv_timeout(Duration::from_secs(5)).expect("returned once stopped");
    txn.abort();
    drop(gate);
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

#[test]
fn a_read_runs_while_a_growth_waits_for_the_open_unit() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), quiet()).unwrap());
    put(&store, "a", b"1").unwrap();
    let (txn, gate) = store.begin().unwrap();
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
    let value = read.recv_timeout(Duration::from_secs(5)).expect("a read ran while a growth waited for the unit");
    assert_eq!(value.as_deref(), Some(&b"1"[..]));
    txn.abort();
    drop(gate);
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
    let files: Vec<_> = std::fs::read_dir(backup.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(files, ["data.mdb"]);
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
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions::default()).unwrap();
    put(&store, "a", b"1").unwrap();
    let durable = store.durable().expect("the store syncs");
    let (sent, closed) = std::sync::mpsc::channel();
    std::thread::spawn(move || sent.send(store.close()).unwrap());
    closed.recv_timeout(Duration::from_secs(5)).expect("the close returned").unwrap();
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
            continued.recv_timeout(Duration::from_secs(5)).expect("the test lets the publisher go on");
        });
        worker_store.copy_to(&destination)
    });
    ready.recv_timeout(Duration::from_secs(5)).expect("publisher reached ready point");
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
fn backup_creates_nested_destinations_and_leaves_only_the_complete_data_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("one/two");
    #[cfg(unix)]
    assert_eq!(backup::directories_to_sync(&target), [target.clone(), root.path().join("one"), root.path().to_owned()]);
    let store = Store::open(dir.path(), quiet()).unwrap();
    put(&store, "value", b"complete").unwrap();
    store.copy_to(&target).unwrap();
    let names: Vec<_> = std::fs::read_dir(&target).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(names, ["data.mdb"]);
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
    assert_eq!(store.idle.lock().unwrap().len(), 1, "the second read took the first's transaction");
    // Reads within reads each hold a transaction of their own
    nested(&store, MAX_IDLE + 1).unwrap();
    assert_eq!(store.idle.lock().unwrap().len(), MAX_IDLE);
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
    let (txn, gate) = store.begin().unwrap();
    txn.abort();
    drop(gate);
    store.end_unit();
    assert!(store.map_size() > before, "the map grew from {before} before the unit began");
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

/// The grower half of the next test, run as a child of this test binary: writes well past its parent's map, and closes
#[test]
#[ignore = "run only by the next test, as its child process"]
fn child_grower() {
    let Ok(path) = std::env::var("ELMDB_GROW_DB") else { return };
    let store = Store::open(Path::new(&path), quiet()).unwrap();
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
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::tests::child_grower", "--nocapture", "--test-threads=1"])
        .env("ELMDB_GROW_DB", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the child's write failed");
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
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
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
