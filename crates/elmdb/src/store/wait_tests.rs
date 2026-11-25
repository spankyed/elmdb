//! What a unit's owner may wait on while its unit is open answers while it stays open, whatever another process did to
//! the file meanwhile. A unit holds the gate for reading and the database's one write transaction until it ends, so any
//! work the owner waits on must need neither. Each test here holds a thread at the point where it met a map another
//! process grew, begins a unit while it is held, lets it go on, and waits a bounded time for its answer with the unit
//! open: a wait that never ended would be a cycle through the unit (`docs/architecture/engine.md`, What waits on what).
//!
//! The work waited on, each in the grouped mode and with syncs off, which take a grown map's size by different paths:
//! a sync (the grouped mode's only, which `durable()` waits on), a read on another thread, and a backup.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use super::*;
use crate::failpoints::{self, points};

/// How long a wait for work beside an open unit may take before it is taken as one that never ends
const ANSWER: Duration = Duration::from_secs(10);

/// The grouped mode, syncing only when a test asks, or syncs off
fn options(grouped: bool) -> StoreOptions {
    let map_size_bytes = Some(256 << 10);
    if grouped {
        StoreOptions {
            map_size_bytes,
            sync_idle: Duration::from_secs(600),
            sync_interval: Duration::from_secs(600),
            ..Default::default()
        }
    } else {
        StoreOptions { map_size_bytes, syncs: false, ..Default::default() }
    }
}

fn put(store: &Store, key: &str, value: &[u8]) -> Result<()> {
    let key = crate::format::key::encode(&[key.into()]);
    store.write(|txn, db| db.put(txn, &key, value).map_err(Into::into), false)
}

/// Another process, in the same mode, grows the file well past this one's map
fn grow_elsewhere(dir: &Path, grouped: bool) {
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .args(["--ignored", "--exact", "store::tests::child_grower", "--nocapture", "--test-threads=1"])
        .env("ELMDB_GROW_DB", dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if grouped {
        child.env("ELMDB_GROW_GROUPED", "1");
    }
    assert!(child.status().unwrap().success(), "the child's write failed");
}

/// Opens a store, has another process grow its file, and runs `work` on a thread of its own, held at `point` once it
/// has met the growth. A unit begins while it is held, taking the size itself, and `work` is let go on: its answer
/// must come within `ANSWER` while the unit is still open. The unit is ended either way before the answer is judged, so
/// a failing test lets the thread go and ends
fn answers_beside_a_unit_begun_as_it_met_a_growth(
    grouped: bool,
    point: &'static str,
    work: impl FnOnce(&Store) -> Result<()> + Send + 'static,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), options(grouped)).unwrap());
    put(&store, "a", b"1").unwrap();
    grow_elsewhere(dir.path(), grouped);
    let (paused, held) = mpsc::channel();
    let (opened, unit_open) = mpsc::channel::<()>();
    failpoints::run_once_at(point, &store.path, move || {
        paused.send(()).unwrap();
        let _ = unit_open.recv_timeout(ANSWER);
    });
    let (answered, answer) = mpsc::channel();
    let worker = {
        let store = Arc::clone(&store);
        std::thread::spawn(move || {
            let _ = answered.send(work(&store));
        })
    };
    held.recv_timeout(ANSWER).expect("the work met the grown map");
    let held_unit = store.begin().unwrap();
    opened.send(()).unwrap();
    let answer = answer.recv_timeout(ANSWER);
    held_unit.abort();
    store.end_unit();
    match answer {
        Ok(answered) => answered.unwrap(),
        // The work panicked: its own failure, not a wait
        Err(mpsc::RecvTimeoutError::Disconnected) => std::panic::resume_unwind(worker.join().unwrap_err()),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("the work waited behind the unit, which its owner held as it waited")
        }
    }
    worker.join().unwrap();
    Arc::into_inner(store).expect("the worker let the store go").close().unwrap();
}

/// The grouped mode's sync, which a durable wait waits on
fn synced(store: &Store) -> Result<()> {
    store.durable().expect("the grouped mode syncs").wait()
}

fn read(store: &Store) -> Result<()> {
    let key = crate::format::key::encode(&["a".into()]);
    let held = store.read(|txn, db| Ok(db.get(txn, &key)?.map(<[u8]>::to_vec)))?;
    assert_eq!(held.as_deref(), Some(&b"1"[..]));
    Ok(())
}

fn backed_up(store: &Store) -> Result<()> {
    let target = tempfile::tempdir().unwrap();
    super::backup::copy(store, &target.path().join("copy"))
}

#[test]
fn a_sync_asked_on_another_thread_answers_beside_a_unit_begun_as_it_met_a_grown_map() {
    answers_beside_a_unit_begun_as_it_met_a_growth(true, points::SYNC_MAP_GROWN, synced);
}

#[test]
fn a_read_on_another_thread_answers_beside_a_unit_begun_as_it_met_a_grown_map_in_the_grouped_mode() {
    answers_beside_a_unit_begun_as_it_met_a_growth(true, points::READ_MAP_GROWN, read);
}

#[test]
fn a_read_on_another_thread_answers_beside_a_unit_begun_as_it_met_a_grown_map_with_syncs_off() {
    answers_beside_a_unit_begun_as_it_met_a_growth(false, points::READ_MAP_GROWN, read);
}

#[test]
fn a_backup_answers_beside_a_unit_begun_as_it_met_a_grown_map_in_the_grouped_mode() {
    answers_beside_a_unit_begun_as_it_met_a_growth(true, points::BACKUP_MAP_GROWN, backed_up);
}

#[test]
fn a_backup_answers_beside_a_unit_begun_as_it_met_a_grown_map_with_syncs_off() {
    answers_beside_a_unit_begun_as_it_met_a_growth(false, points::BACKUP_MAP_GROWN, backed_up);
}
