//! A database's lifecycle around an open unit and a growing map: a close refuses while a unit holds the write
//! transaction, a unit that filled the map says so and goes on refusing, an aborted unit lets the map grow, and the
//! commit sequence follows what the database holds.

use std::sync::mpsc;
use std::time::Duration;

use serde_json::json;

use super::{batch, open, schema, write};
use crate::error::ErrorCode;
use crate::{Database, OpenOptions};

/// A database whose map is 1 MiB, so a few MiB of writes fill it
fn small(dir: &tempfile::TempDir) -> Database {
    Database::open(
        dir.path(),
        OpenOptions { schema: Some(schema()), syncs: false, map_size_bytes: Some(1 << 20), ..Default::default() },
    )
    .unwrap()
}

/// Runs `f` on a thread of its own, failing the test rather than hanging when it doesn't finish within ten seconds
fn within_ten_seconds<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done.send(f());
    });
    finished.recv_timeout(Duration::from_secs(10)).expect("finished within ten seconds")
}

/// A 4 MiB row in a field no index reads, so the write costs its bytes and nothing else
/// A task holding 4 MiB of text that doesn't compress, so it outgrows a 1 MiB map however its value is stored
fn large_task(id: &str) -> serde_json::Value {
    let mut state = 0x9e37_79b9_u32;
    let text: String = (0..4 << 20)
        .map(|_| {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            char::from(b'!' + (state >> 24) as u8 % 90)
        })
        .collect();
    json!({ "op": "create", "type": "Task", "id": id, "fields": { "c": text } })
}

#[test]
fn a_close_refuses_while_a_unit_is_open_and_closes_once_it_ends() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open(&dir));
    let mut unit = db.begin().unwrap();
    unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })])).unwrap();
    let closing = std::sync::Arc::clone(&db);
    let refused = within_ten_seconds(move || closing.close());
    assert_eq!(refused.unwrap_err().code, ErrorCode::UnitOpen);
    unit.abort();
    drop(unit);
    db.close().unwrap();
    assert!(!db.is_open());
}

#[test]
fn a_unit_that_fills_the_map_says_so_and_refuses_what_follows() {
    let dir = tempfile::tempdir().unwrap();
    let db = small(&dir);
    let mut unit = db.begin().unwrap();
    let full = unit.write(&batch(vec![large_task("Task-9")])).unwrap_err();
    assert_eq!(full.code, ErrorCode::MapFull, "{}", full.message);
    let after = unit.write(&batch(vec![json!({ "op": "create", "type": "Task", "id": "Task-1" })])).unwrap_err();
    assert_eq!(after.code, ErrorCode::MapFull, "{}", after.message);
    assert!(after.message.contains("must be aborted"), "{}", after.message);
}

#[test]
fn an_aborted_unit_lets_the_map_grow_while_it_is_still_held() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(small(&dir));
    let mut unit = db.begin().unwrap();
    unit.abort();
    let writing = std::sync::Arc::clone(&db);
    // The unit is aborted and not yet dropped: a write that has to grow the map must not wait for the drop
    within_ten_seconds(move || write(&writing, vec![large_task("Task-9")])).unwrap();
    drop(unit);
}

#[test]
fn the_sequence_follows_what_the_database_holds() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let store = db.store().unwrap();
    // What another process's commits leave: a stored sequence this handle never counted
    store
        .write(
            |txn, db| crate::store::put_meta(txn, db.main, crate::format::meta::SEQUENCE, &40u64.to_be_bytes()),
            false,
        )
        .unwrap();
    assert_eq!(db.sequence().unwrap(), 41, "the write that stored 40 committed as 41");
    let (_, changes) = db.write(&batch(vec![]), false).unwrap();
    assert_eq!(changes.sequence, 42);
}

/// What another process leaves: one commit, naming a task, and it has gone
#[test]
#[ignore = "run by commit_elsewhere, as a process of its own"]
fn child_commits_a_task() {
    let Ok(path) = std::env::var("ELMDB_ELSEWHERE_DB") else { return };
    let db = Database::open(std::path::Path::new(&path), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    write(&db, vec![large_task("Task-elsewhere")]).unwrap();
    db.close().unwrap();
}

/// Runs `child_commits_a_task` against the database in `dir`, and waits for it to end
fn commit_elsewhere(dir: &tempfile::TempDir) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "engine::tests::lifecycle::child_commits_a_task",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ELMDB_ELSEWHERE_DB", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the other process failed");
}

#[test]
fn a_handle_learns_that_another_process_committed() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![large_task("Task-mine")]).unwrap();
    assert!(!db.committed_elsewhere().unwrap(), "nothing yet");
    commit_elsewhere(&dir);

    assert!(db.committed_elsewhere().unwrap(), "another process committed");
    assert!(!db.committed_elsewhere().unwrap(), "and nothing since this asked");
    write(&db, vec![large_task("Task-after")]).unwrap();
    assert!(!db.committed_elsewhere().unwrap(), "a commit here is never another's");
}

#[test]
fn a_handles_own_commits_of_every_kind_are_never_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![large_task("Task-1"), large_task("Task-2")]).unwrap();
    write(&db, vec![json!({ "op": "link", "source": "Task-1", "kind": "refs", "target": "Task-2" })]).unwrap();
    let unit = db.begin().unwrap();
    unit.commit(false).unwrap();
    db.store().unwrap().write(|_, _| Ok(()), false).unwrap();
    assert!(!db.committed_elsewhere().unwrap());
}

/// A commit here that finds another's commit before it, whose wake this handle never heard, tells its listener
#[test]
fn a_commit_here_after_an_unheard_one_elsewhere_wakes_this_handles_listener() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    commit_elsewhere(&dir);
    // Listening only now: the other process's wake went to nobody
    let (told, heard) = std::sync::mpsc::channel();
    db.hear_commits_elsewhere(std::sync::Arc::new(move || told.send(()).unwrap())).unwrap();
    write(&db, vec![large_task("Task-here")]).unwrap();
    heard.recv_timeout(Duration::from_secs(10)).expect("woken by its own commit");
    assert!(db.committed_elsewhere().unwrap());
}

#[test]
fn a_unit_sees_its_own_changes_before_it_commits() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let mut unit = db.begin().unwrap();
    unit.write(&batch(vec![large_task("Task-pending")])).unwrap();
    assert!(unit.pending_changes().unwrap().entities.contains_key("Task-pending"));
    let decoded: crate::ChangeSet = rmp_serde::from_slice(&unit.pending_changes_bytes().unwrap()).unwrap();
    assert_eq!(decoded, unit.pending_changes().unwrap(), "what the binding reads, in msgpack");
    unit.commit(false).unwrap();
}

/// Replaces every row of the database `ELMDB_RESET_DB` names, from a process of its own
#[test]
#[ignore = "a helper the tests below run as another process"]
fn child_resets_the_database() {
    let Ok(path) = std::env::var("ELMDB_RESET_DB") else { return };
    let db = Database::open(std::path::Path::new(&path), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    db.reset().unwrap();
    db.close().unwrap();
}

fn reset_elsewhere(dir: &tempfile::TempDir) {
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "engine::tests::lifecycle::child_resets_the_database",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ELMDB_RESET_DB", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the resetting process failed");
}

const A_TASK: fn(&str) -> serde_json::Value =
    |id| json!({ "op": "create", "type": "Task", "id": id, "fields": { "title": "t" } });

/// Replaces every row of the database `ELMDB_REPLACE_DB` names with the database `ELMDB_REPLACE_FROM` names
#[test]
#[ignore = "a helper the tests below run as another process"]
fn child_replaces_the_database() {
    let Ok(path) = std::env::var("ELMDB_REPLACE_DB") else { return };
    let from = std::env::var("ELMDB_REPLACE_FROM").unwrap();
    let db = Database::open(std::path::Path::new(&path), OpenOptions { syncs: false, ..Default::default() }).unwrap();
    db.replace_with(from).unwrap();
    db.close().unwrap();
}

#[test]
fn a_replace_with_in_another_process_refuses_commits_here_as_a_reset_does() {
    let (dir, source) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    {
        let db = open(&source);
        write(&db, vec![A_TASK("Task-copied")]).unwrap();
        db.close().unwrap();
    }
    let db = open(&dir);
    write(&db, vec![A_TASK("Task-1")]).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "engine::tests::lifecycle::child_replaces_the_database",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ELMDB_REPLACE_DB", dir.path())
        .env("ELMDB_REPLACE_FROM", source.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "the replacing process failed");
    let epoch = db.replaced_elsewhere().unwrap().expect("replaced");
    assert_eq!(db.write(&batch(vec![A_TASK("Task-2")]), false).unwrap_err().code, ErrorCode::DataReplaced);
    assert!(db.adopt_replacement(epoch).unwrap());
    write(&db, vec![A_TASK("Task-3")]).unwrap();
}

#[test]
fn adopting_a_replacement_another_has_followed_adopts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    reset_elsewhere(&dir);
    let first = db.replaced_elsewhere().unwrap().expect("replaced");
    reset_elsewhere(&dir);
    assert!(!db.adopt_replacement(first).unwrap(), "the second reset is news the caller hasn't acted on");
    assert_eq!(db.write(&batch(vec![A_TASK("Task-1")]), false).unwrap_err().code, ErrorCode::DataReplaced);
    let second = db.replaced_elsewhere().unwrap().expect("still replaced");
    assert!(second > first);
    assert!(db.adopt_replacement(second).unwrap());
    write(&db, vec![A_TASK("Task-2")]).unwrap();
}

#[test]
fn a_reset_here_that_failed_vouches_for_no_reset_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    crate::failpoints::fail_once_here(crate::failpoints::points::COMMIT);
    assert!(db.reset().is_err(), "the commit failed as a full disk would");
    write(&db, vec![A_TASK("Task-1")]).unwrap();
    reset_elsewhere(&dir);
    assert_eq!(
        db.write(&batch(vec![A_TASK("Task-2")]), false).unwrap_err().code,
        ErrorCode::DataReplaced,
        "the failed reset's epoch is the one the other process stored"
    );
}

#[test]
fn a_write_refused_for_a_replacement_leaves_nothing_for_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    reset_elsewhere(&dir);
    assert_eq!(db.write(&batch(vec![A_TASK("Task-refused")]), false).unwrap_err().code, ErrorCode::DataReplaced);
    let epoch = db.replaced_elsewhere().unwrap().unwrap();
    assert!(db.adopt_replacement(epoch).unwrap());
    write(&db, vec![A_TASK("Task-next")]).unwrap();
    assert!(super::row(&db, "Task-refused").is_none());
    assert!(super::row(&db, "Task-next").is_some());
}

#[test]
fn a_reset_in_another_process_refuses_commits_here_until_it_is_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![A_TASK("Task-1")]).unwrap();
    assert_eq!(db.replaced_elsewhere().unwrap(), None);

    reset_elsewhere(&dir);
    let epoch = db.replaced_elsewhere().unwrap().expect("replaced");
    let refused = db.write(&batch(vec![A_TASK("Task-2")]), false).unwrap_err();
    assert_eq!(refused.code, ErrorCode::DataReplaced);
    let mut unit = db.begin().unwrap();
    unit.write(&batch(vec![A_TASK("Task-3")])).unwrap();
    assert_eq!(unit.commit(false).unwrap_err().code, ErrorCode::DataReplaced, "a unit opened after it too");
    assert!(
        db.read(&serde_json::from_value(json!({"reads":[{"read":"query","plan":{"start":{"from":"types","types":["Task"]},"terminal":{"answer":"ids"}}}]})).unwrap()).is_ok(),
        "reads carry on, over what's there now"
    );

    assert!(db.adopt_replacement(epoch).unwrap());
    assert_eq!(db.replaced_elsewhere().unwrap(), None);
    write(&db, vec![A_TASK("Task-4")]).unwrap();
}

#[test]
fn a_reset_here_goes_on_taking_commits_and_says_nothing_of_itself() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![A_TASK("Task-1")]).unwrap();
    db.reset().unwrap();
    assert_eq!(db.replaced_elsewhere().unwrap(), None);
    write(&db, vec![A_TASK("Task-2")]).unwrap();
    db.reset().unwrap();
    write(&db, vec![A_TASK("Task-3")]).unwrap();
}

#[test]
fn a_replacement_counts_for_a_process_that_opened_after_it() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(&dir);
        db.reset().unwrap();
        db.close().unwrap();
    }
    let db = open(&dir);
    assert_eq!(db.replaced_elsewhere().unwrap(), None);
    write(&db, vec![A_TASK("Task-1")]).unwrap();
}

#[test]
fn a_failed_reset_leaves_the_handle_refusing_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    let mut unit = db.begin().unwrap();
    assert!(db.reset().is_err(), "a unit holds the write transaction");
    unit.abort();
    write(&db, vec![A_TASK("Task-1")]).unwrap();
}

/// Holds a unit open on the database `ELMDB_UNIT_DB` names, having written in it, until it is killed; says so by
/// creating `holding` beside it
#[test]
#[ignore = "a helper the tests below run as another process"]
fn child_holds_a_unit_until_killed() {
    let Ok(path) = std::env::var("ELMDB_UNIT_DB") else { return };
    let path = std::path::PathBuf::from(path);
    let db = Database::open(&path, OpenOptions { syncs: false, ..Default::default() }).unwrap();
    let mut unit = db.begin().unwrap();
    unit.write(&batch(vec![A_TASK("Task-held")])).unwrap();
    std::fs::write(path.join("holding"), b"").unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Another process holding a unit open on `dir`, ready once it has written in it
fn a_writer_holding_a_unit(dir: &tempfile::TempDir) -> std::process::Child {
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "engine::tests::lifecycle::child_holds_a_unit_until_killed",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ELMDB_UNIT_DB", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let holding = dir.path().join("holding");
    let started = std::time::Instant::now();
    while !holding.exists() {
        assert!(started.elapsed() < Duration::from_secs(30), "the other process never held its unit");
        std::thread::sleep(Duration::from_millis(10));
    }
    child
}

#[test]
fn a_writer_killed_inside_a_unit_leaves_the_write_lock_to_the_next_while_another_holds_the_database_open() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open(&dir));
    let mut child = a_writer_holding_a_unit(&dir);
    child.kill().unwrap();
    child.wait().unwrap();
    let writing = std::sync::Arc::clone(&db);
    within_ten_seconds(move || write(&writing, vec![A_TASK("Task-after")]).unwrap());
    assert!(super::row(&db, "Task-held").is_none(), "the killed unit committed nothing");
}

#[test]
fn a_writer_waiting_when_the_holder_is_killed_is_let_go() {
    let dir = tempfile::tempdir().unwrap();
    let db = std::sync::Arc::new(open(&dir));
    let mut child = a_writer_holding_a_unit(&dir);
    let (done, finished) = mpsc::channel();
    let writing = std::sync::Arc::clone(&db);
    std::thread::spawn(move || {
        let _ = done.send(write(&writing, vec![A_TASK("Task-waiting")]).map(|_| ()));
    });
    assert!(finished.recv_timeout(Duration::from_millis(300)).is_err(), "it waits while the holder is alive");
    child.kill().unwrap();
    child.wait().unwrap();
    finished.recv_timeout(Duration::from_secs(10)).expect("let go once the holder died").unwrap();
}

/// Asks which process holds the record lock LMDB keeps on the first byte of the lock file `ELMDB_LOCK_FILE` names, by
/// which an opener knows it isn't the first, printing its process id or 0. Readers' locks, one byte at each reader's
/// process id, are left out of the question
#[cfg(unix)]
#[test]
#[ignore = "a helper the test below runs as another process"]
fn child_asks_who_holds_the_lock_file() {
    let Ok(path) = std::env::var("ELMDB_LOCK_FILE") else { return };
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new().read(true).write(true).open(path).unwrap();
    // SAFETY: a zeroed `flock` is a valid query for the whole file once its type and whence are set
    let mut query: libc::flock = unsafe { std::mem::zeroed() };
    query.l_type = libc::F_WRLCK as libc::c_short;
    query.l_whence = libc::SEEK_SET as libc::c_short;
    query.l_start = 0;
    query.l_len = 1;
    // SAFETY: the descriptor is open, and F_GETLK writes only into `query`
    assert_eq!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETLK, &mut query) }, 0);
    let unlocked = query.l_type == libc::F_UNLCK as libc::c_short;
    println!("holder={}", if unlocked { 0 } else { query.l_pid });
}

#[cfg(unix)]
#[test]
fn a_writing_handle_keeps_the_lock_lmdb_holds_on_its_lock_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(&dir);
    write(&db, vec![A_TASK("Task-1")]).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "engine::tests::lifecycle::child_asks_who_holds_the_lock_file",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ELMDB_LOCK_FILE", dir.path().join("lock.mdb"))
        .output()
        .unwrap();
    let printed = String::from_utf8_lossy(&output.stdout);
    // The test harness prints its own words on the same line
    let holder = printed.split("holder=").nth(1).and_then(|rest| rest.split_whitespace().next()).unwrap_or_else(|| {
        panic!("the other process didn't answer: {printed} {}", String::from_utf8_lossy(&output.stderr))
    });
    assert_eq!(holder, std::process::id().to_string(), "the next opener would otherwise take itself for the first");
}

#[test]
fn a_panic_that_unwinds_through_a_write_or_a_unit_leaves_the_database_to_be_written_and_closed() {
    use crate::failpoints::{self, points};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let task = |id: &str| vec![json!({ "op": "create", "type": "Task", "id": id })];
    for point in [points::WRITE_HELD, points::UNIT_RESERVED, points::UNIT_HELD, points::UNIT_COMMITTING] {
        let dir = tempfile::tempdir().unwrap();
        let db = open(&dir);
        write(&db, task("Task-before")).unwrap();
        failpoints::run_once_here(point, || panic!("a panic the test raised"));
        let unwound = catch_unwind(AssertUnwindSafe(|| {
            if point == points::WRITE_HELD {
                write(&db, task("Task-lost")).unwrap();
            } else {
                let mut unit = db.begin().unwrap();
                unit.write(&batch(task("Task-lost"))).unwrap();
                unit.commit(false).unwrap();
            }
        }));
        assert!(unwound.is_err() && !failpoints::is_armed_here(point), "{point}: the panic was raised there");
        // The write transaction, the unit's place and the gate were let go as the panic unwound: a unit begins, a
        // write lands, what the panic interrupted is not there, and the database closes and opens again
        let mut unit = db.begin().unwrap_or_else(|error| panic!("{point}: {error}"));
        unit.write(&batch(task("Task-unit"))).unwrap();
        unit.commit(false).unwrap();
        write(&db, task("Task-after")).unwrap();
        assert!(super::row(&db, "Task-lost").is_none(), "{point}");
        assert!(db.check().unwrap().is_clean(), "{point}");
        db.close().unwrap_or_else(|error| panic!("{point}: {error}"));
        let again = open(&dir);
        assert!(super::row(&again, "Task-after").is_some() && super::row(&again, "Task-before").is_some(), "{point}");
    }
}

#[test]
fn a_data_file_cut_short_is_extended_by_a_writable_open_and_what_its_missing_pages_hold_is_refused_not_faulted() {
    // A power loss can leave the tree's pages unwritten at the end of the file. The writable open extends the file with
    // zeros, so a read of one of them is an error from the engine, which the open or a check reports, and not a fault that
    // ends the process
    let dir = tempfile::tempdir().unwrap();
    {
        let db = open(&dir);
        for n in 0..200 {
            let task = json!({ "op": "create", "type": "Task", "id": format!("Task-{n}"), "fields": { "c": "x".repeat(2000) } });
            write(&db, vec![task]).unwrap();
        }
        db.close().unwrap();
    }
    let data = dir.path().join("data.mdb");
    let whole = std::fs::metadata(&data).unwrap().len();
    std::fs::OpenOptions::new().write(true).open(&data).unwrap().set_len(whole / 2).unwrap();
    let options = || OpenOptions { schema: Some(schema()), syncs: false, ..Default::default() };
    match Database::open(dir.path(), options()) {
        Err(error) => assert_eq!(error.code, ErrorCode::Corrupted, "{error}"),
        Ok(db) => {
            let report = db.check();
            assert!(report.map(|r| !r.is_clean()).unwrap_or(true), "a page the file lost is reported");
            db.close().unwrap();
        }
    }
    assert_eq!(std::fs::metadata(&data).unwrap().len(), whole, "extended to the pages its meta page names");
}
