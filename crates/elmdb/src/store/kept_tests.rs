//! The grouped mode's last synced commit, kept (`kept.rs`, `restore.rs`): the record a sync writes, the restore an open
//! makes after a loss, a failed sync, and a process ended at each step of a sync and of a restore. A loss is made here
//! by copying the database's files while it is open, as a process that died leaves them, and writing its record as
//! of another boot, as a loss leaves it; the replay of every file a loss could leave is the engine's
//! (`engine/tests/power_loss.rs`).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::record::{Mode, RecordFile, State};
use super::*;
use crate::failpoints::{self, points};

/// The grouped mode, syncing only when a test asks
fn grouped() -> StoreOptions {
    StoreOptions { sync_idle: Duration::from_secs(600), sync_interval: Duration::from_secs(600), ..Default::default() }
}

fn put(store: &Store, key: &str, value: &[u8]) -> Result<()> {
    let key = crate::format::key::encode(&[key.into()]);
    store.write(|txn, db| db.put(txn, &key, value).map_err(Into::into), false)
}

fn get(store: &Store, key: &str) -> Option<Vec<u8>> {
    let key = crate::format::key::encode(&[key.into()]);
    store.read(|txn, db| Ok(db.get(txn, &key)?.map(<[u8]>::to_vec))).unwrap()
}

fn synced(store: &Store) {
    store.durable().unwrap().wait().unwrap();
}

/// The record beside the database in `dir`
fn record_in(dir: &Path) -> Option<record::Record> {
    RecordFile::open(dir, false).unwrap()?.read().unwrap()
}

/// LMDB's newest commit in the store
fn newest(store: &Store) -> u64 {
    store.env.info().last_txn_id as u64
}

/// The database's files copied into a new directory as they are now, the record written as of another boot when
/// `lost`: a process that died leaves the first, a loss of power the second, when what was not synced reached the disk
fn copied(from: &Path, lost: bool) -> tempfile::TempDir {
    let to = tempfile::tempdir().unwrap();
    std::fs::copy(from.join("data.mdb"), to.path().join("data.mdb")).unwrap();
    let held = std::fs::read(from.join(record::RECORD)).unwrap();
    let held = if lost { record::of_another_boot(&held) } else { held };
    std::fs::write(to.path().join(record::RECORD), held).unwrap();
    to
}

#[test]
fn a_writable_open_records_its_newest_commit_and_a_sync_moves_the_record_to_the_newest() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    let opened = record_in(dir.path()).expect("the open wrote its record");
    assert_eq!((opened.mode, opened.state, opened.commit), (Mode::Grouped, State::Kept, newest(&store)));
    assert!(opened.boot.same(&boot::this_boot()));
    assert!(!store.kept.as_ref().unwrap().pinned(), "a process that owes nothing holds no pin");
    put(&store, "a", b"1").unwrap();
    assert!(store.kept.as_ref().unwrap().pinned(), "a process with a commit unsynced holds its pin");
    assert_eq!(record_in(dir.path()).unwrap().commit, opened.commit, "a commit writes no record");
    synced(&store);
    let moved = record_in(dir.path()).unwrap();
    assert_eq!((moved.commit, moved.count), (newest(&store), opened.count + 1));
    assert_eq!(store.remembered(), Some(moved.commit));
    assert!(!store.kept.as_ref().unwrap().pinned(), "the sync ended the pin, nothing being owed");
    store.close().unwrap();
    // Opened again with nothing since, the open writes no record
    let store = Store::open(dir.path(), grouped()).unwrap();
    assert_eq!(record_in(dir.path()).unwrap().count, moved.count);
    store.close().unwrap();
}

#[test]
fn a_process_that_died_with_commits_unsynced_is_opened_at_its_newest_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    synced(&store);
    put(&store, "b", b"2").unwrap();
    // The OS holds what was written, and the record is of this boot
    let died = copied(dir.path(), false);
    let opened = Store::open(died.path(), grouped()).unwrap();
    assert_eq!(get(&opened, "b").as_deref(), Some(&b"2"[..]));
    assert_eq!(opened.restored(), None);
    // The open recorded the newest commit before its first write
    assert_eq!(record_in(died.path()).unwrap().commit, newest(&opened));
    opened.close().unwrap();
    store.close().unwrap();
}

#[test]
fn a_loss_after_commits_unsynced_restores_the_record_s_commit_and_the_database_takes_writes_over_its_numbers() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    synced(&store);
    let kept = record_in(dir.path()).unwrap().commit;
    for n in 0..5 {
        put(&store, &format!("lost-{n}"), &vec![n as u8; 20_000]).unwrap();
    }
    let lost = copied(dir.path(), true);
    let opened = Store::open(lost.path(), grouped()).unwrap();
    assert_eq!(newest(&opened), kept, "opened at the record's commit");
    assert_eq!(opened.restored(), Some(kept));
    assert_eq!(get(&opened, "a").as_deref(), Some(&b"1"[..]));
    assert_eq!(get(&opened, "lost-0"), None);
    let record = record_in(lost.path()).unwrap();
    assert!(record.boot.same(&boot::this_boot()) && record.restored == Some(kept), "{record:?}");
    assert!(lost.path().join(record::RESTORED).exists(), "the meta pages written over are saved");
    // Its two meta pages are of one commit, which a sync takes
    let restored = opened.kept.as_ref().unwrap();
    restored.sync(&|| restored.adopt()).unwrap();
    assert_eq!(record_in(lost.path()).unwrap().commit, kept);
    // Then commits over the numbers the lost ones used, and an open after them
    for n in 0..8 {
        put(&opened, &format!("again-{n}"), &vec![n as u8; 20_000]).unwrap();
    }
    synced(&opened);
    assert_eq!(record_in(lost.path()).unwrap().commit, newest(&opened));
    opened.close().unwrap();
    let again = Store::open(lost.path(), grouped()).unwrap();
    assert_eq!(get(&again, "again-7").as_deref(), Some(&vec![7u8; 20_000][..]));
    assert_eq!(get(&again, "lost-4"), None);
    assert_eq!(again.restored(), Some(kept), "the notice stands until a caller says it was read");
    again.close().unwrap();
    store.close().unwrap();
}

#[test]
fn a_loss_with_nothing_unsynced_opens_as_it_is_and_records_this_boot() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    synced(&store);
    let lost = copied(dir.path(), true);
    let opened = Store::open(lost.path(), grouped()).unwrap();
    assert_eq!((opened.restored(), get(&opened, "a").as_deref()), (None, Some(&b"1"[..])));
    assert!(record_in(lost.path()).unwrap().boot.same(&boot::this_boot()), "written again as of this boot");
    opened.close().unwrap();
    store.close().unwrap();
}

#[test]
fn a_record_another_file_bears_out_is_opened_as_it_is_and_recorded_again() {
    let (one, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // Two databases apart from their first write: made alike, the one's file would hold the other's commit whole
    for (dir, key, writes) in [(&one, "one", 6), (&other, "other", 3)] {
        let store = Store::open(dir.path(), grouped()).unwrap();
        for n in 0..writes {
            put(&store, &format!("{key}-{n}"), &vec![n as u8; 10_000 + 1000 * writes]).unwrap();
        }
        store.close().unwrap();
    }
    // The data file of one put where the other's was, its record beside it, after a loss
    std::fs::copy(one.path().join("data.mdb"), other.path().join("data.mdb")).unwrap();
    let held = std::fs::read(other.path().join(record::RECORD)).unwrap();
    std::fs::write(other.path().join(record::RECORD), record::of_another_boot(&held)).unwrap();
    let store = Store::open(other.path(), grouped()).unwrap();
    assert!(store.not_borne_out());
    assert_eq!((store.restored(), get(&store, "one-5").is_some()), (None, true));
    let record = record_in(other.path()).unwrap();
    assert_eq!((record.mode, record.commit), (Mode::Grouped, newest(&store)), "the file's own record now");
    store.close().unwrap();
}

#[test]
fn a_failed_sync_refuses_the_next_commit_and_the_next_open_restores_the_record_s_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    synced(&store);
    let kept = record_in(dir.path()).unwrap().commit;
    put(&store, "b", b"2").unwrap();
    let failing = failpoints::fail_while_at(points::SYNC, &store.path);
    assert!(store.durable().unwrap().wait().is_err());
    let record = record_in(dir.path()).unwrap();
    assert_eq!((record.state, record.commit), (State::Failed, kept));
    let refused = put(&store, "c", b"3").unwrap_err();
    assert!(refused.message.contains("An earlier sync failed"), "{refused}");
    drop(failing);
    assert!(put(&store, "c", b"3").is_err(), "refused until the database is opened again");
    let _ = store.close();
    // Opened again in the same boot: what was committed after the failed sync's commit may not be on disk
    let store = Store::open(dir.path(), grouped()).unwrap();
    assert_eq!((newest(&store), store.restored()), (kept, Some(kept)));
    assert_eq!((get(&store, "a").as_deref(), get(&store, "b")), (Some(&b"1"[..]), None));
    put(&store, "c", b"3").unwrap();
    synced(&store);
    store.close().unwrap();
}

#[test]
fn a_failed_record_the_file_does_not_bear_out_is_refused() {
    let (one, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let store = Store::open(one.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    synced(&store);
    put(&store, "b", b"2").unwrap();
    let failing = failpoints::fail_while_at(points::SYNC, &store.path);
    assert!(store.durable().unwrap().wait().is_err());
    drop(failing);
    let _ = store.close();
    // Apart from its first write, so the record's commit is nowhere in it
    let made = Store::open(other.path(), grouped()).unwrap();
    for n in 0..4 {
        put(&made, &format!("other-{n}"), &vec![n as u8; 10_000]).unwrap();
    }
    made.close().unwrap();
    std::fs::copy(other.path().join("data.mdb"), one.path().join("data.mdb")).unwrap();
    let refused = Store::open(one.path(), grouped()).err().expect("refused");
    assert_eq!(refused.code, ErrorCode::Corrupted, "{refused}");
}

#[test]
fn a_meta_page_read_beside_a_commit_is_not_taken() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    // A commit lands between the sync's beginning its read transaction and its reading that commit's meta page: the
    // page of the earlier commit is not taken, and the record names the later one
    let (env, db) = (store.env.clone(), store.db.main);
    failpoints::run_once_at(points::SYNC_PINNED, &store.path, move || {
        let mut txn = env.write_txn().unwrap();
        db.put(&mut txn, b"forced", b"between").unwrap();
        txn.commit().unwrap();
    });
    synced(&store);
    assert_eq!(record_in(dir.path()).unwrap().commit, newest(&store));
    store.close().unwrap();
}

#[test]
fn a_sync_after_a_writer_died_between_its_meta_page_and_the_lock_file_takes_the_commit_before() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    // What a writer of the next commit leaves when it is killed once its meta page is written and before LMDB's lock
    // file is told: the meta pages' newest is that commit for good, and a read transaction begins at the one before
    let commit = newest(&store);
    let data = dir.path().join("data.mdb");
    let page = page_size_of(&mut std::fs::File::open(&data).unwrap(), &data).unwrap();
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&data).unwrap();
    let mut meta = vec![0u8; page];
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(commit % 2 * page as u64)).unwrap();
    std::io::Read::read_exact(&mut file, &mut meta).unwrap();
    // The page's own number, then the commit's, at the end of its body
    meta[..8].copy_from_slice(&((commit + 1) % 2).to_ne_bytes());
    let txnid = reach::PAGE_HEADER + record::BODY - 8;
    meta[txnid..txnid + 8].copy_from_slice(&(commit + 1).to_ne_bytes());
    std::io::Seek::seek(&mut file, std::io::SeekFrom::Start((commit + 1) % 2 * page as u64)).unwrap();
    std::io::Write::write_all(&mut file, &meta).unwrap();
    assert_eq!(store.env.info().last_txn_id as u64, commit + 1, "the meta pages name the dead writer's commit");
    let (done, finished) = std::sync::mpsc::channel();
    let durable = store.durable().unwrap();
    std::thread::spawn(move || done.send(durable.wait()).unwrap());
    let Ok(synced) = finished.recv_timeout(Duration::from_secs(20)) else {
        // Dropped, the store would join a sync thread that never ends: the test fails here instead of hanging
        std::mem::forget(store);
        panic!("the sync never ended");
    };
    assert!(synced.is_ok(), "{synced:?}");
    assert_eq!(record_in(dir.path()).unwrap().commit, commit, "the lock file's newest, which a reader begins at");
}

#[test]
fn a_read_only_open_restores_where_it_can_write_the_file_and_is_refused_where_it_cannot() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    synced(&store);
    put(&store, "b", b"2").unwrap();
    let lost = copied(dir.path(), true);
    let read_only = StoreOptions { read_only: true, ..grouped() };
    // A process that may write any file can't be shown refused
    #[cfg(unix)]
    // SAFETY: geteuid has no preconditions and only reads the process's credentials
    if unsafe { libc::geteuid() } != 0 {
        use std::os::unix::fs::PermissionsExt;
        let data = lost.path().join("data.mdb");
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o444)).unwrap();
        let refused = Store::open(lost.path(), read_only.clone()).err().expect("refused");
        assert_eq!(refused.code, ErrorCode::RestorePending, "{refused}");
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let opened = Store::open(lost.path(), read_only).unwrap();
    assert_eq!((get(&opened, "a").is_some(), get(&opened, "b")), (true, None));
    opened.close().unwrap();
    store.close().unwrap();
}

/// What a child of this test binary does to the database `ELMDB_KEPT_DB` names, by `ELMDB_KEPT_DO`, printing what
/// came of it as its last line
#[test]
#[ignore = "run only by the tests below, as their child process"]
fn child_of_the_kept_tests() {
    let (Ok(path), Ok(what)) = (std::env::var("ELMDB_KEPT_DB"), std::env::var("ELMDB_KEPT_DO")) else { return };
    let path = PathBuf::from(path);
    match what.as_str() {
        // Opens read-only, says how that went, and waits to be killed
        "open-read-only" | "open-read-only-no-boot" => {
            if what.ends_with("no-boot") {
                boot::NO_BOOT.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            match Store::open(&path, StoreOptions { read_only: true, ..grouped() }) {
                Ok(store) => println!("opened b={:?}", get(&store, "b").is_some()),
                Err(error) => println!("refused {}", error.code.as_str()),
            }
        }
        // Writes, syncs, writes again, and is ended at the step of the next sync the point names
        point if point.starts_with("die-at-") => {
            let store = Store::open(&path, grouped()).unwrap();
            put(&store, "a", b"1").unwrap();
            synced(&store);
            put(&store, "b", b"2").unwrap();
            failpoints::run_once_at(named(&point["die-at-".len()..]), &store.path, || std::process::abort());
            println!("dying");
            let _ = store.durable().unwrap().wait();
            println!("lived");
        }
        // Opens a database to be restored and is ended at the step of the restore the point names
        point if point.starts_with("restore-dies-at-") => {
            let canonical = path.canonicalize().unwrap();
            failpoints::run_once_at(named(&point["restore-dies-at-".len()..]), &canonical, || std::process::abort());
            println!("dying");
            let _ = Store::open(&path, grouped());
            println!("lived");
        }
        // Holds the database open with a commit unsynced until its input ends; at each line it reads, commits and
        // syncs, and says how that went
        "hold" => {
            let store = Store::open(&path, grouped()).unwrap();
            put(&store, "b", b"2").unwrap();
            println!("holding");
            for _ in std::io::stdin().lines() {
                let synced = put(&store, "c", b"3").and_then(|()| store.durable().unwrap().wait());
                let after = put(&store, "d", b"4");
                println!("synced {} then {}", synced.is_ok(), after.is_ok());
            }
            drop(store);
        }
        other => panic!("no such child: {other}"),
    }
}

fn named(point: &str) -> &'static str {
    match point {
        "sync-pinned" => points::SYNC_PINNED,
        "sync-data" => points::SYNC_DATA,
        "record-written" => points::RECORD_WRITTEN,
        "sync-recorded" => points::SYNC_RECORDED,
        "restore-saved" => points::RESTORE_SAVED,
        "restore-half" => points::RESTORE_HALF,
        "restore-written" => points::RESTORE_WRITTEN,
        "restore-synced" => points::RESTORE_SYNCED,
        other => panic!("no such point: {other}"),
    }
}

/// Runs the child that does `what` to the database in `dir` to its end, and answers its last line
fn child(dir: &Path, what: &str) -> String {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::kept_tests::child_of_the_kept_tests", "--nocapture", "--test-threads=1"])
        .env("ELMDB_KEPT_DB", dir)
        .env("ELMDB_KEPT_DO", what)
        .stderr(Stdio::null())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&output.stdout);
    said.lines()
        .filter_map(|line| line.split_once(" ... ").map_or(Some(line), |(_, said)| Some(said)))
        .rfind(|line| ["opened", "refused", "dying", "lived"].iter().any(|word| line.starts_with(word)))
        .unwrap_or_default()
        .to_owned()
}

/// A child that holds the database in `dir` open with a commit unsynced, until it is dropped
struct Holder(std::process::Child);

impl Holder {
    /// Asks the holder to commit and sync, and answers what it said of it
    fn sync(&mut self, lines: &mut impl Iterator<Item = String>) -> String {
        use std::io::Write;
        writeln!(self.0.stdin.as_mut().unwrap(), "sync").unwrap();
        lines
            .find(|line| line.contains("synced "))
            .map(|line| line[line.find("synced ").unwrap()..].to_owned())
            .unwrap()
    }

    fn hold(dir: &Path) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "store::kept_tests::child_of_the_kept_tests", "--nocapture"])
            .env("ELMDB_KEPT_DB", dir)
            .env("ELMDB_KEPT_DO", "hold")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        assert!(lines.map_while(std::result::Result::ok).any(|line| line.contains("holding")), "the holder held");
        Self(child)
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        let _ = self.0.wait();
    }
}

#[test]
fn a_pin_another_process_holds_keeps_its_commit_s_pages_from_this_process_s_writes() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), grouped()).unwrap().close().unwrap();
    let holder = Holder::hold(dir.path());
    // The holder's write found the record's commit the newest, and pinned it
    let pinned = record_in(dir.path()).unwrap();
    let data = dir.path().join("data.mdb");
    let mut file = std::fs::File::open(&data).unwrap();
    let page = page_size_of(&mut file, &data).unwrap();
    let pages = reach::pages_of(&mut file, page, reach::roots_of(&pinned.body), &data).unwrap();
    let held = |file: &mut std::fs::File| -> Vec<Vec<u8>> {
        pages
            .iter()
            .map(|at| {
                let mut bytes = vec![0u8; page];
                std::io::Seek::seek(file, std::io::SeekFrom::Start(at * page as u64)).unwrap();
                std::io::Read::read_exact(file, &mut bytes).unwrap();
                bytes
            })
            .collect()
    };
    let before = held(&mut file);
    // Values on overflow pages, written over again and again, so the pages each commit frees are what the next wants
    let store = Store::open(dir.path(), grouped()).unwrap();
    let writes = |from: usize| {
        for n in from..from + 30 {
            put(&store, &format!("w-{}", n % 5), &vec![n as u8; 30_000]).unwrap();
            if n % 5 == 4 {
                synced(&store);
            }
        }
    };
    writes(0);
    assert!(before == held(&mut file), "a page the other process's pin keeps was written over");
    // Its close syncs, and it owes nothing: its pin ends
    drop(holder);
    writes(30);
    assert!(before != held(&mut file), "the pin's pages stayed as they were once it was ended");
    store.close().unwrap();
}

#[test]
fn another_process_s_sync_after_a_failed_one_fails_and_refuses_its_next_commit() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), grouped()).unwrap().close().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::kept_tests::child_of_the_kept_tests", "--nocapture"])
        .env("ELMDB_KEPT_DB", dir.path())
        .env("ELMDB_KEPT_DO", "hold")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines().map_while(std::result::Result::ok);
    assert!(lines.any(|line| line.contains("holding")));
    let mut holder = Holder(child);
    assert_eq!(holder.sync(&mut lines), "synced true then true", "the other process syncs while all is well");
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    let failing = failpoints::fail_while_at(points::SYNC, &store.path);
    assert!(store.durable().unwrap().wait().is_err());
    drop(failing);
    assert_eq!(record_in(dir.path()).unwrap().state, State::Failed);
    // The other process's next sync finds the record failed: what it builds on may lack pages
    assert_eq!(holder.sync(&mut lines), "synced false then false");
    assert_eq!(record_in(dir.path()).unwrap().state, State::Failed, "only a restore writes over it");
    let _ = store.close();
    drop(holder);
}

#[test]
fn what_the_pin_holds_back_makes_a_sync_due_whatever_the_time() {
    // Values written over again and again, which free as much as they take: with no sync the pin keeps every page
    // they free, and the file grows by all of it
    let grown = |bound: u64| {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), grouped()).unwrap();
        store.set_bound(bound);
        for n in 0..200 {
            put(&store, &format!("w-{}", n % 4), &vec![n as u8; 64 << 10]).unwrap();
        }
        let (syncs, grown) = (store.syncs(), store.file_size());
        store.close().unwrap();
        (syncs, grown)
    };
    let (unbounded_syncs, unbounded) = grown(u64::MAX / 8);
    let (bounded_syncs, bounded) = grown(1 << 20);
    assert_eq!(unbounded_syncs, 0, "no sync was due by the time");
    assert!(bounded_syncs > 0, "the bound made syncs due");
    // The file grows by the bound and by what is committed while a sync runs, which a slow sync makes the most of
    assert!(bounded < unbounded, "{bounded} bytes with a bound of 1 MiB, {unbounded} without");
}

#[test]
fn an_open_that_finds_a_restore_beside_another_process_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    let failing = failpoints::fail_while_at(points::SYNC, &store.path);
    assert!(store.durable().unwrap().wait().is_err());
    drop(failing);
    // This process has the database open, its record failed: another's open would undo what this one goes on from
    assert_eq!(child(dir.path(), "open-read-only"), "refused RESTORE_PENDING");
    let _ = store.close();
}

#[test]
fn a_read_only_open_beside_a_writer_with_commits_unsynced_opens_where_no_boot_can_be_read() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), grouped()).unwrap().close().unwrap();
    // The record is of no boot the reader can name, which is another boot than any: the writer's lock says the
    // database is open, and so in this boot
    let holder = Holder::hold(dir.path());
    assert_eq!(child(dir.path(), "open-read-only-no-boot"), "opened b=true");
    drop(holder);
}

#[test]
fn a_process_ended_at_each_step_of_a_sync_leaves_a_database_opened_where_the_model_says() {
    // Ended before its record is written, the record names the commit before; once it is written, the OS holds it.
    // A death opens at the newest commit whatever the step, and a loss after it at the record's
    for (point, recorded) in
        [("sync-pinned", false), ("sync-data", false), ("record-written", true), ("sync-recorded", true)]
    {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(child(dir.path(), &format!("die-at-{point}")), "dying", "{point}");
        let lost = copied(dir.path(), true);
        let store = Store::open(dir.path(), grouped()).unwrap();
        assert_eq!(get(&store, "b").as_deref(), Some(&b"2"[..]), "{point}: a death keeps the newest commit");
        store.close().unwrap();
        let store = Store::open(lost.path(), grouped()).unwrap();
        assert_eq!(get(&store, "a").as_deref(), Some(&b"1"[..]), "{point}");
        assert_eq!(get(&store, "b").is_some(), recorded, "{point}: a loss keeps the record's commit");
        store.close().unwrap();
    }
}

#[test]
fn a_process_ended_at_each_step_of_a_restore_leaves_it_to_the_next_open() {
    for point in ["restore-saved", "restore-half", "restore-written", "restore-synced"] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), grouped()).unwrap();
        put(&store, "a", b"1").unwrap();
        synced(&store);
        put(&store, "b", b"2").unwrap();
        let lost = copied(dir.path(), true);
        store.close().unwrap();
        assert_eq!(child(lost.path(), &format!("restore-dies-at-{point}")), "dying", "{point}");
        let store = Store::open(lost.path(), grouped()).unwrap();
        assert_eq!((get(&store, "a").is_some(), get(&store, "b")), (true, None), "{point}");
        assert!(store.restored().is_some(), "{point}");
        store.close().unwrap();
    }
}
