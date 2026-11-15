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
    // Opened again with nothing since, the open writes no record, and syncs nothing: it relies on the record's commit,
    // the file's newest
    let store = Store::open(dir.path(), grouped()).unwrap();
    assert_eq!(record_in(dir.path()).unwrap().count, moved.count);
    assert_eq!(store.remembered(), Some(moved.commit));
    assert_eq!(store.record_syncs(), 0, "the open synced what it could rely on");
    store.close().unwrap();
}

#[test]
fn an_open_of_another_mode_than_the_record_s_writes_its_own_and_one_of_the_same_writes_none() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), grouped()).unwrap().close().unwrap();
    let ordered = StoreOptions { sync_each_commit: true, ..grouped() };
    let store = Store::open(dir.path(), ordered.clone()).unwrap();
    let written = record_in(dir.path()).unwrap();
    assert_eq!(written.mode, Mode::Ordered, "the record of the grouped mode was kept");
    store.close().unwrap();
    let store = Store::open(dir.path(), ordered).unwrap();
    assert_eq!(record_in(dir.path()).unwrap().count, written.count, "the record of its own mode was written again");
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
    // Final: a wait after it fails without another sync being tried
    let syncs = store.syncs();
    let later = store.durable().unwrap().wait().unwrap_err();
    assert_eq!(store.syncs(), syncs, "a sync was tried again after the data's failed");
    // Naming the failure once, as it was met
    assert_eq!(later.message.matches("An earlier sync failed").count(), 1, "{later}");
    assert!(later.message.ends_with("Injected sync failure"), "{later}");
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
fn a_sync_that_fails_before_its_data_fails_its_own_waits_and_the_next_sync_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    // As a reader table that stays full fails it, before anything is written: no page may have been dropped
    failpoints::fail_once_at(points::SYNC_BEGIN, &store.path);
    assert!(store.durable().unwrap().wait().is_err());
    assert!(kept(&store).refused().is_none(), "a failure before the data refused the process's commits");
    // The next sync goes on, and a durable write after it is durable
    store.durable().unwrap().wait().unwrap();
    store
        .write(|txn, db| db.put(txn, &crate::format::key::encode(&["b".into()]), b"2").map_err(Into::into), true)
        .unwrap();
    assert_eq!(record_in(dir.path()).unwrap().commit, newest(&store));
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
        // Holds the database open; at each line it reads commits (and with `sync`, waits for its sync), and at a line
        // that begins `pause `, first pauses the step `ELMDB_KEPT_AT` names of its next sync, whichever thread reaches
        // it, until a line more
        "pause" => {
            let store = Store::open(&path, grouped()).unwrap();
            println!("holding");
            let point = named(&std::env::var("ELMDB_KEPT_AT").unwrap());
            // A line at a time, the input's lock let go between them, so the paused step can read its own
            let line = || {
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).ok().filter(|read| *read > 0).map(|_| line.trim().to_owned())
            };
            while let Some(asked) = line() {
                let (pause, asked) =
                    asked.strip_prefix("pause ").map_or((false, asked.as_str()), |asked| (true, asked));
                if pause {
                    failpoints::run_once_at(point, &store.path, move || {
                        println!("paused");
                        let _ = line();
                    });
                }
                let done = match asked {
                    "sync" => store.durable().unwrap().wait(),
                    _ => put(&store, asked, b"written"),
                };
                println!("synced {}", done.is_ok());
            }
            drop(store);
        }
        other => panic!("no such child: {other}"),
    }
}

fn named(point: &str) -> &'static str {
    match point {
        "sync-pinned" => points::SYNC_PINNED,
        "sync-taken" => points::SYNC_TAKEN,
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

/// A child that pauses at a step of its process's syncs whenever it is asked to
struct Pauser {
    child: std::process::Child,
    lines: Box<dyn Iterator<Item = String>>,
}

impl Pauser {
    fn open(dir: &Path, at: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "store::kept_tests::child_of_the_kept_tests", "--nocapture"])
            .env("ELMDB_KEPT_DB", dir)
            .env("ELMDB_KEPT_DO", "pause")
            .env("ELMDB_KEPT_AT", at)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines().map_while(std::result::Result::ok);
        let mut pauser = Self { child, lines: Box::new(lines) };
        pauser.until("holding");
        pauser
    }

    fn until(&mut self, said: &str) {
        assert!(self.lines.any(|line| line.contains(said)), "the child said no {said:?}");
    }

    fn say(&mut self, line: &str) {
        use std::io::Write;
        writeln!(self.child.stdin.as_mut().unwrap(), "{line}").unwrap();
    }

    /// Asks for `what`, a sync or a key to write, and waits until the child is paused in it
    fn pause_in(&mut self, what: &str) {
        self.say(&format!("pause {what}"));
        self.until("paused");
    }

    /// Lets the paused step go on, and answers whether what was asked then succeeded
    fn go_on(&mut self) -> bool {
        self.say("go on");
        let said = self.lines.find(|line| line.contains("synced ")).expect("the child answered");
        said.contains("synced true")
    }
}

impl Drop for Pauser {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.wait();
    }
}

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
fn an_open_in_another_mode_than_a_live_writer_s_is_refused_and_one_after_it_has_gone_is_not() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), grouped()).unwrap().close().unwrap();
    let holder = Holder::hold(dir.path());
    let before = record_in(dir.path()).unwrap();
    // Each other mode refused, naming both, and having read or written nothing
    for (other, named) in [
        (StoreOptions { sync_each_commit: true, ..grouped() }, "`syncEachCommit` mode"),
        (StoreOptions { syncs: false, ..grouped() }, "`syncs: false` mode"),
    ] {
        let Err(refused) = Store::open(dir.path(), other) else { panic!("an open of the {named} beside a writer") };
        assert_eq!(refused.code, ErrorCode::ModeMismatch, "{refused}");
        assert!(refused.message.contains("grouped mode") && refused.message.contains(named), "{refused}");
    }
    assert_eq!(record_in(dir.path()).unwrap(), before, "a refused open wrote the record");
    // Beside it, a writer of its own mode, and a reader of any
    Store::open(dir.path(), grouped()).unwrap().close().unwrap();
    Store::open(dir.path(), StoreOptions { read_only: true, ..grouped() }).unwrap().close().unwrap();
    // Once it has gone, the other mode opens, and writes its record
    drop(holder);
    let store = Store::open(dir.path(), StoreOptions { sync_each_commit: true, ..grouped() }).unwrap();
    assert_eq!(record_in(dir.path()).unwrap().mode, Mode::Ordered);
    store.close().unwrap();
}

#[test]
fn a_first_open_of_a_mode_that_keeps_no_commit_writes_its_record() {
    for (options, mode) in [
        (StoreOptions { sync_each_commit: true, ..grouped() }, Mode::Ordered),
        (StoreOptions { syncs: false, ..grouped() }, Mode::Unsynced),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), options).unwrap();
        let written = record_in(dir.path()).expect("the open wrote no record");
        assert_eq!((written.mode, written.commit), (mode, 0));
        store.close().unwrap();
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
    // they free. How many syncs run by the time the writes end turns on how soon the sync thread wakes: no sync is due
    // by the time for ten minutes, so one that runs at all is the bound's. How far the file grows with the bound is
    // held apart (`a_write_past_the_bound_waits_for_its_sync_...`)
    let synced_within = |bound: u64, wait: Duration| {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), grouped()).unwrap();
        store.set_bound(bound);
        for n in 0..200 {
            put(&store, &format!("w-{}", n % 4), &vec![n as u8; 64 << 10]).unwrap();
        }
        let waited = std::time::Instant::now();
        while store.syncs() == 0 && waited.elapsed() < wait {
            std::thread::sleep(Duration::from_millis(10));
        }
        let synced = store.syncs() > 0;
        store.close().unwrap();
        synced
    };
    assert!(!synced_within(u64::MAX / 8, Duration::from_secs(1)), "a sync was due by the time");
    assert!(synced_within(1 << 20, Duration::from_secs(20)), "the bound made no sync due");
}

#[test]
fn a_durable_write_beside_another_process_s_slower_sync_is_kept_by_the_next_loss() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), grouped()).unwrap().close().unwrap();
    // The other process has a commit unsynced, and its sync has taken that commit's meta page and stopped before the
    // record's lock
    let mut slower = Pauser::open(dir.path(), "sync-taken");
    slower.say("b");
    slower.until("synced true");
    slower.pause_in("sync");
    // This one writes past it, durable: the record names this commit once the write returns
    let store = Store::open(dir.path(), grouped()).unwrap();
    store
        .write(|txn, db| db.put(txn, &crate::format::key::encode(&["x".into()]), b"durable").map_err(Into::into), true)
        .unwrap();
    let durable = newest(&store);
    assert_eq!(record_in(dir.path()).unwrap().commit, durable);
    // The slower sync goes on with its earlier commit, and writes no record back
    assert!(slower.go_on());
    assert_eq!(record_in(dir.path()).unwrap().commit, durable, "the record went back");
    let lost = copied(dir.path(), true);
    store.close().unwrap();
    drop(slower);
    let opened = Store::open(lost.path(), grouped()).unwrap();
    assert_eq!(get(&opened, "x").as_deref(), Some(&b"durable"[..]), "a loss undid a durable write");
    opened.close().unwrap();
}

#[test]
fn an_open_beside_another_process_s_sync_waits_for_its_record() {
    let dir = tempfile::tempdir().unwrap();
    Store::open(dir.path(), grouped()).unwrap().close().unwrap();
    // A process open since before another's commit, unsynced, whose next write finds it and syncs before it begins:
    // stopped with the data synced and the record not yet written, its lock held
    let mut writer = Pauser::open(dir.path(), "sync-data");
    let holder = Holder::hold(dir.path());
    writer.pause_in("c");
    // Read-only, so the record's lock it waits for is the one it decides under, and not the one a writable open's own
    // record would take after it
    let path = dir.path().to_owned();
    let (opened, open) = std::sync::mpsc::channel();
    let opening = std::thread::spawn(move || {
        let store = Store::open(&path, StoreOptions { read_only: true, ..grouped() }).unwrap();
        opened.send(()).unwrap();
        store
    });
    assert!(open.recv_timeout(Duration::from_millis(500)).is_err(), "the open decided beside a record being written");
    assert!(writer.go_on(), "the write after its sync");
    open.recv_timeout(Duration::from_secs(20)).expect("the open went on once the record was written");
    let store = opening.join().unwrap();
    // Beside other processes it opens as the file is, which a record names no part of: what it waited for was the
    // record's lock, and it reads what is there
    assert_eq!(get(&store, "b").as_deref(), Some(&b"2"[..]));
    store.close().unwrap();
    drop(writer);
    drop(holder);
}

#[test]
fn a_write_run_again_after_its_map_grew_syncs_before_it_and_a_loss_keeps_the_record_s_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(1 << 20), ..grouped() }).unwrap();
    put(&store, "a", &vec![1u8; 20_000]).unwrap();
    synced(&store);
    // Two commits after the record's, which free its pages for a third to take once no pin keeps them
    put(&store, "a", &vec![2u8; 20_000]).unwrap();
    put(&store, "b", &vec![3u8; 20_000]).unwrap();
    let before = newest(&store);
    // Too large for the map: aborted, the map grown with the pin ended, and run again, its pin made sure of first,
    // which syncs and records the newest commit since it is later than the record's
    put(&store, "big", &vec![4u8; 4 << 20]).unwrap();
    assert!(store.map_size() > 1 << 20, "the map grew");
    assert_eq!(record_in(dir.path()).unwrap().commit, before, "the record named the commit before the write");
    let lost = copied(dir.path(), true);
    store.close().unwrap();
    let opened = Store::open(lost.path(), grouped()).unwrap();
    assert_eq!(newest(&opened), before);
    assert_eq!(get(&opened, "a").as_deref(), Some(&vec![2u8; 20_000][..]));
    assert_eq!(get(&opened, "b").as_deref(), Some(&vec![3u8; 20_000][..]));
    assert_eq!(get(&opened, "big"), None);
    opened.close().unwrap();
}

#[test]
fn a_write_past_the_bound_waits_for_its_sync_and_the_file_grows_by_the_bound_alone() {
    // Values written over again and again, which free as much as they take, as fast as one process commits. The sync
    // the bound makes due is held once its data is synced: a write past the bound waits for it, so the writer is held
    // with it, where without the wait it would go on writing fresh pages as long as the sync took
    let dir = tempfile::tempdir().unwrap();
    let store = shared(dir.path());
    store.set_bound(1 << 20);
    let (paused, held) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel::<()>();
    failpoints::run_once_at(points::SYNC_DATA, &store.path, move || {
        // Not unwrapped: a test that failed before it heard this has dropped the receiver, and the sync its store's
        // close makes comes here again, where a panic would leave the close waiting on a sync that never ends
        let _ = paused.send(());
        let _ = released.recv_timeout(Duration::from_secs(30));
    });
    let writing = std::sync::Arc::clone(&store);
    let writer = std::thread::spawn(move || {
        for n in 0..200 {
            put(&writing, &format!("w-{}", n % 4), &vec![n as u8; 64 << 10]).unwrap();
        }
    });
    held.recv_timeout(Duration::from_secs(30)).expect("the bound made a sync due");
    std::thread::sleep(Duration::from_millis(500));
    assert!(!writer.is_finished(), "a write went on past the bound while its sync was held");
    release.send(()).unwrap();
    writer.join().unwrap();
    // What the four values hold, the bound past it, and a commit's own pages past that
    let grown = store.file_size();
    assert!(grown < 6 << 20, "{grown} bytes, for four values of 64 KiB and a bound of 1 MiB");
}

#[test]
fn what_the_pin_keeps_from_reuse_is_what_commits_from_its_own_freed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", &vec![1u8; 100_000]).unwrap();
    let pinned = kept(&store).pinned_at().expect("a process with a commit unsynced holds its pin");
    // Each rewrite frees the value's overflow run before it: the pages LMDB's OVPAGES gives a value of its size
    let page = store.page_size() as u64;
    let run = (16 - 1 + 100_000) / page + 1;
    for n in 0..10 {
        put(&store, "a", &vec![n as u8; 100_000]).unwrap();
    }
    let data = dir.path().join("data.mdb");
    let mut file = std::fs::File::open(&data).unwrap();
    let meta = reach::newer_meta_page(&mut file, page as usize).unwrap();
    let root = meta.roots()[0];
    let kept_pages = reach::freed_from(&mut file, page as usize, root, pinned);
    assert!(kept_pages >= 10 * run, "{kept_pages} pages kept, where ten rewrites freed {} pages of runs", 10 * run);
    assert_eq!(reach::freed_from(&mut file, page as usize, root, meta.txnid + 1), 0, "a commit after the newest");
    let newest = reach::freed_from(&mut file, page as usize, root, meta.txnid);
    assert!(newest >= run && newest < kept_pages, "the newest commit's alone, which freed a run: {newest}");
    store.close().unwrap();
}

#[test]
fn what_the_pin_keeps_is_read_from_a_list_of_its_own_and_from_a_tree_of_branches() {
    let dir = tempfile::tempdir().unwrap();
    // A map the writes fit in: a growth ends the pin, and the write after it pins a later commit
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(512 << 20), ..grouped() }).unwrap();
    store.set_bound(u64::MAX / 8);
    let page = store.page_size() as u64;
    // A value so large that the pages one commit frees of it are listed on a page of their own, LMDB's for a list
    // longer than a node holds: the commit after the pin's writes it, and the one after that frees its run
    let big = vec![7u8; 20 << 20];
    put(&store, "big", &big).unwrap();
    let pinned = kept(&store).pinned_at().unwrap();
    put(&store, "big", &big).unwrap();
    // And commits enough that the free tree's records fill more than a leaf
    for n in 0..1_000u32 {
        put(&store, "small", &n.to_be_bytes()).unwrap();
    }
    assert_eq!(kept(&store).pinned_at(), Some(pinned), "the pin stayed where the test reads from");
    let data = dir.path().join("data.mdb");
    let mut file = std::fs::File::open(&data).unwrap();
    let meta = reach::newer_meta_page(&mut file, page as usize).unwrap();
    let root = meta.roots()[0];
    let run = (16 - 1 + big.len() as u64) / page + 1;
    let kept_pages = reach::freed_from(&mut file, page as usize, root, pinned);
    assert!(kept_pages >= run + 1_000, "{kept_pages} pages: the large value's run of {run} and a page a commit after");
    // The record of the commit that freed the run alone: the run, read from its list's own page, and the few pages
    // of the tree the commit wrote again
    let record = reach::freed_from(&mut file, page as usize, root, pinned + 2)
        - reach::freed_from(&mut file, page as usize, root, pinned + 3);
    assert!(record >= run && record <= run + 16, "{record} pages freed by the commit that freed a run of {run}");
    store.close().unwrap();
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
        // Two commits after the record's, so that neither meta page is its: the restore writes over each
        put(&store, "b", b"2").unwrap();
        put(&store, "c", b"3").unwrap();
        let lost = copied(dir.path(), true);
        store.close().unwrap();
        let (data, saved) = (lost.path().join("data.mdb"), lost.path().join(record::RESTORED));
        let page = page_size_of(&mut std::fs::File::open(&data).unwrap(), &data).unwrap();
        let metas = |file: &Path| std::fs::read(file).unwrap()[..2 * page].to_vec();
        let (before, body) = (metas(&data), record_in(lost.path()).unwrap().body);
        // A save another death cut short, one byte shy of whole: made again, and not taken for the file's
        std::fs::write(&saved, &before[1..]).unwrap();
        assert_eq!(child(lost.path(), &format!("restore-dies-at-{point}")), "dying", "{point}");
        assert_eq!(std::fs::read(&saved).unwrap(), before, "{point}: the meta pages as they were, saved whole");
        if point == "restore-half" {
            // The first meta page written, from the map's size on, and the second as it was
            let from = reach::PAGE_HEADER + record::WRITTEN_FROM;
            let now = metas(&data);
            assert_eq!(now[from..reach::PAGE_HEADER + body.len()], body[record::WRITTEN_FROM..], "the first");
            assert_eq!(now[page..], before[page..], "the second was written before the restore was half done");
        }
        let store = Store::open(lost.path(), grouped()).unwrap();
        assert_eq!((get(&store, "a").is_some(), get(&store, "b")), (true, None), "{point}");
        assert!(store.restored().is_some(), "{point}");
        // The restore made again kept the save of the meta pages as they were before any restore
        assert_eq!(std::fs::read(&saved).unwrap(), before, "{point}: saved again over the first");
        store.close().unwrap();
    }
}

/// The store, shared with what a run point runs on another thread
fn shared(dir: &Path) -> std::sync::Arc<Store> {
    std::sync::Arc::new(Store::open(dir, grouped()).unwrap())
}

fn kept(store: &Store) -> &Kept {
    store.kept.as_ref().expect("a store of the grouped mode")
}

/// A commit another process would make, made through the environment and not the store: the store hears of nothing
fn committed_elsewhere(store: &Store, key: &[u8]) {
    let mut txn = store.env.write_txn().unwrap();
    store.db.main.put(&mut txn, key, b"elsewhere").unwrap();
    txn.commit().unwrap();
}

#[test]
fn a_write_that_commits_nothing_leaves_no_pin() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    synced(&store);
    let refused = store.write(|_, _| -> Result<()> { Err(Error::new(ErrorCode::Io, "refused by the test")) }, false);
    assert!(refused.is_err());
    assert!(!kept(&store).pinned(), "a write that made no commit left its pin, which no sync would end");
    store.close().unwrap();
}

#[test]
fn a_growth_ends_the_pin_and_the_next_write_makes_sure_of_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    assert!(kept(&store).pinned());
    store.grow().unwrap();
    assert!(!kept(&store).pinned(), "the map was resized with the pin's transaction open");
    put(&store, "b", b"2").unwrap();
    assert!(kept(&store).pinned());
    store.close().unwrap();
}

#[test]
fn a_write_and_a_unit_whose_pin_a_growth_ended_before_their_transaction_make_sure_of_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let store = shared(dir.path());
    put(&store, "a", b"1").unwrap();
    // The map grown between a write's making sure of its pin and its transaction's beginning, which ends the pin
    let grower = std::sync::Arc::clone(&store);
    failpoints::run_once_here(points::WRITE_PINNED, move || grower.grow().unwrap());
    put(&store, "b", b"2").unwrap();
    assert!(!failpoints::is_armed_here(points::WRITE_PINNED), "the write reached the point");
    let grower = std::sync::Arc::clone(&store);
    failpoints::run_once_here(points::UNIT_PINNED, move || grower.grow().unwrap());
    let (mut txn, gate, writer) = store.begin().unwrap();
    assert!(!failpoints::is_armed_here(points::UNIT_PINNED), "the unit reached the point");
    store.db.main.put(&mut txn, b"c", b"3").unwrap();
    let committed = store.commit_txn(txn);
    drop((gate, writer));
    store.end_unit();
    committed.unwrap();
    assert_eq!(get(&store, "b").as_deref(), Some(&b"2"[..]));
    let unit = store.read(|txn, db| Ok(db.main.get(txn, b"c")?.map(<[u8]>::to_vec))).unwrap();
    assert_eq!(unit.as_deref(), Some(&b"3"[..]), "the unit's commit");
}

#[test]
fn a_unit_open_across_a_failed_sync_is_refused_at_its_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    let (mut txn, gate, writer) = store.begin().unwrap();
    store.db.main.put(&mut txn, b"b", b"2").unwrap();
    let failing = failpoints::fail_while_at(points::SYNC, &store.path);
    assert!(store.durable().unwrap().wait().is_err());
    drop(failing);
    let refused = store.commit_txn(txn).unwrap_err();
    drop((gate, writer));
    store.end_unit();
    assert!(refused.message.contains("An earlier sync failed"), "{refused}");
    let _ = store.close();
}

#[test]
fn another_process_s_later_record_is_not_written_back_and_a_sync_of_nothing_new_writes_none() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    synced(&store);
    // A sync with nothing committed since the record: no record written
    let count = record_in(dir.path()).unwrap().count;
    kept(&store).sync(&|| kept(&store).adopt()).unwrap();
    assert_eq!(record_in(dir.path()).unwrap().count, count, "a record of the commit the record names already");
    // Another process's record of a later commit than this one has: kept, whatever this one syncs
    let mut file = RecordFile::open(dir.path(), true).unwrap().unwrap();
    let later = record::Record { count: count + 1, commit: newest(&store) + 100, ..record_in(dir.path()).unwrap() };
    file.write(&later).unwrap();
    put(&store, "b", b"2").unwrap();
    synced(&store);
    assert_eq!(record_in(dir.path()).unwrap().commit, later.commit, "the record went back");
    store.close().unwrap();
}

#[test]
fn a_record_this_process_synced_is_not_synced_again_and_another_s_is() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    let syncs = |run: &dyn Fn()| {
        let before = store.record_syncs();
        run();
        store.record_syncs() - before
    };
    // The open wrote and synced the record: a sync writes the next, and syncs that alone
    assert_eq!(
        syncs(&|| {
            put(&store, "a", b"1").unwrap();
            synced(&store)
        }),
        1
    );
    // A record another process wrote, which it may have died before syncing: synced before its other slot is written
    let on_disk = record_in(dir.path()).unwrap();
    RecordFile::open(dir.path(), true).unwrap().unwrap().write(&on_disk.next(State::Kept)).unwrap();
    assert_eq!(
        syncs(&|| {
            put(&store, "b", b"2").unwrap();
            synced(&store)
        }),
        2
    );
    store.close().unwrap();
}

#[test]
fn a_sync_beside_which_a_commit_lands_keeps_its_pin_for_that_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = shared(dir.path());
    put(&store, "a", b"1").unwrap();
    // A commit of this process once the sync has its commit's meta page and has synced the data: owed after the sync
    let writer = std::sync::Arc::clone(&store);
    failpoints::run_once_at(points::SYNC_DATA, &store.path, move || put(&writer, "b", b"2").unwrap());
    synced(&store);
    assert!(kept(&store).pinned(), "the sync ended the pin of a process with a commit after the record's");
    synced(&store);
    assert!(!kept(&store).pinned());
}

#[test]
fn the_exit_s_sync_records_the_newest_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    let last = newest(&store);
    store.close_at_exit(Duration::from_secs(5)).unwrap();
    assert_eq!(record_in(dir.path()).unwrap().commit, last);
}

#[test]
fn a_failed_sync_marks_no_record_of_another_mode() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    // A writer of another mode's record, which names no commit to restore
    let on_disk = record_in(dir.path()).unwrap();
    let other = record::of_mode(Some(&on_disk), Mode::Unsynced, boot::this_boot());
    RecordFile::open(dir.path(), true).unwrap().unwrap().write(&other).unwrap();
    let failing = failpoints::fail_while_at(points::SYNC, &store.path);
    assert!(store.durable().unwrap().wait().is_err());
    drop(failing);
    let record = record_in(dir.path()).unwrap();
    assert_eq!((record.mode, record.state), (Mode::Unsynced, State::Kept), "a record naming commit 0 marked failed");
    let _ = store.close();
}

#[test]
fn a_write_after_another_process_committed_syncs_and_records_before_it_begins() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    synced(&store);
    committed_elsewhere(&store, b"elsewhere");
    let theirs = newest(&store);
    put(&store, "b", b"2").unwrap();
    assert_eq!(record_in(dir.path()).unwrap().commit, theirs, "the write pinned a commit later than the record's");
    store.close().unwrap();
}

#[test]
fn adopting_a_grown_map_drops_the_idle_reads_and_the_pin() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    put(&store, "a", b"1").unwrap();
    assert!(get(&store, "a").is_some());
    assert!(store.idle.len() > 0, "a read keeps its transaction for the next");
    store.adopt_map_size().unwrap();
    assert_eq!(store.idle.len(), 0, "the map was taken at another size with a read transaction open");
    assert!(!kept(&store).pinned());
    store.close().unwrap();
}

#[test]
fn a_write_takes_the_map_another_process_grew_the_file_past() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(256 << 10), ..grouped() }).unwrap();
    put(&store, "a", b"1").unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "store::tests::child_grower", "--nocapture", "--test-threads=1"])
        .env("ELMDB_GROW_DB", dir.path())
        .env("ELMDB_GROW_GROUPED", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the child's write failed");
    put(&store, "b", b"2").unwrap();
    assert_eq!(get(&store, "grown").as_deref(), Some(&b"yes"[..]));
    store.close().unwrap();
}

#[test]
fn an_open_s_own_commit_is_made_under_the_pin() {
    let dir = tempfile::tempdir().unwrap();
    // A database with elmdb's own and no vector graphs, as one an earlier elmdb made: the open creates them, its own
    // commit, which syncs and records the commit before it first
    {
        // SAFETY: this test's own directory, which no other handle has open
        let env = unsafe { EnvOpenOptions::new().read_txn_without_tls().max_dbs(8).open(dir.path()) }.unwrap();
        let txn = env.write_txn().unwrap();
        env.create_databases_and_commit(txn, |opener, txn| -> Result<_> {
            let db = opener.create_database::<Bytes, Bytes>(txn, Some(format::DB_NAME))?;
            put_meta(txn, db, meta::KIND, format::KIND.as_bytes())?;
            put_meta(txn, db, meta::FORMAT, &format::FORMAT_VERSION.to_be_bytes())?;
            Ok(CommitOrAbort::<_, ()>::Commit(db))
        })
        .unwrap();
    }
    let store = Store::open(dir.path(), grouped()).unwrap();
    assert!(store.db.cosine.is_some(), "the open made the graphs' databases");
    assert_eq!(record_in(dir.path()).unwrap().count, 2, "the commit before the open's, then the open's own");
    store.close().unwrap();
}

#[test]
fn a_meta_page_names_the_last_page_lmdb_does() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), grouped()).unwrap();
    for n in 0..20 {
        put(&store, &format!("k-{n}"), &vec![n as u8; 30_000]).unwrap();
    }
    let data = dir.path().join("data.mdb");
    let mut file = std::fs::File::open(&data).unwrap();
    let page = page_size_of(&mut file, &data).unwrap();
    let meta = reach::newer_meta_page(&mut file, page).unwrap();
    assert_eq!(meta.txnid, newest(&store));
    assert_eq!(meta.last_page(), store.env.info().last_page_number as u64);
    store.close().unwrap();
}

/// The dirty pages a write transaction keeps in memory before LMDB writes some of them to the file, which its commit
/// then writes over: `MDB_IDL_UM_MAX` (`midl.h`), 2^17 less one
const DIRTY_ROOM: usize = (1 << 17) - 1;

#[test]
#[ignore = "a transaction past LMDB's dirty pages, some 2 GiB at 16 KiB pages and 512 MiB at 4 KiB: by hand, \
            cargo nextest run -p elmdb --run-ignored only -E 'test(/spills_its_pages/)'"]
fn a_write_that_spills_its_pages_before_its_commit_leaves_a_loss_the_record_s_commit() {
    let dir = tempfile::tempdir().unwrap();
    let page = {
        let store = Store::open(dir.path(), grouped()).unwrap();
        let page = store.page_size();
        store.close().unwrap();
        page
    };
    // A map the write fits in, so that it isn't run again: that a write run again after its map grew syncs and
    // records before it begins is held apart (`a_write_run_again_after_its_map_grew_...`)
    let map = (4 * DIRTY_ROOM * page).div_ceil(MAP_STEP) * MAP_STEP;
    let store = Store::open(dir.path(), StoreOptions { map_size_bytes: Some(map), ..grouped() }).unwrap();
    // A value of one overflow page each: every put is one dirty page of its own
    let value = |byte: u8| vec![byte; page - 64];
    let key = |name: &str, i: usize| crate::format::key::encode(&[format!("{name}-{i:08}").into()]);
    let loaded = DIRTY_ROOM + DIRTY_ROOM / 5;
    // The record's commit holds more pages than the write below keeps in memory
    for from in (0..loaded).step_by(10_000) {
        store
            .write(
                |txn, db| {
                    for i in from..(from + 10_000).min(loaded) {
                        db.put(txn, &key("load", i), &value(5))?;
                    }
                    Ok(())
                },
                false,
            )
            .unwrap();
    }
    put(&store, "a", &vec![1u8; 20_000]).unwrap();
    synced(&store);
    let recorded = newest(&store);
    // Two commits after the record's: the first frees all its pages, which a third may take where no pin keeps them.
    // LMDB gives a write the lowest free pages first and writes a write's highest dirty pages to the file once it
    // keeps too many: given the record's pages, it writes over them before it commits
    store
        .write(
            |txn, db| {
                for i in 0..loaded {
                    db.delete(txn, &key("load", i))?;
                }
                Ok(())
            },
            false,
        )
        .unwrap();
    put(&store, "a", &vec![3u8; 20_000]).unwrap();
    let before = std::fs::read(dir.path().join("data.mdb")).unwrap();
    let mut lost = None;
    let written = store.write(
        |txn, db| {
            for i in 0..DIRTY_ROOM + DIRTY_ROOM / 4 {
                db.put(txn, &key("big", i), &value(9))?;
            }
            lost = Some(copied(dir.path(), true));
            Err::<(), _>(Error::new(ErrorCode::Io, "the write is given up once its loss is taken"))
        },
        false,
    );
    let given_up = written.unwrap_err();
    assert!(given_up.message.contains("given up"), "{given_up}");
    let lost = lost.expect("the write took its loss");
    let image = std::fs::read(lost.path().join("data.mdb")).unwrap();
    assert!(image != before, "nothing was written to the file before the commit");
    assert_eq!(record_in(lost.path()).unwrap().commit, recorded);
    store.close().unwrap();
    let opened = Store::open(lost.path(), grouped()).unwrap();
    assert_eq!(newest(&opened), recorded, "the loss opened at the record's commit");
    assert_eq!(get(&opened, "a").as_deref(), Some(&vec![1u8; 20_000][..]));
    let held = opened
        .read(|txn, db| {
            let mut held = 0;
            for i in 0..loaded {
                held += usize::from(db.get(txn, &key("load", i))? == Some(&value(5)[..]));
            }
            Ok(held)
        })
        .unwrap();
    assert_eq!(held, loaded, "the record's commit lost values to pages written before a later commit");
    assert_eq!(get(&opened, "big-00000000"), None);
    opened.close().unwrap();
}
