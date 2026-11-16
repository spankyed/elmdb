//! The grouped mode's last synced commit, kept (`docs/roadmap/synced-commit.md`): its pages by a read transaction, the
//! pin, and its meta page by the record beside the data file. A power loss then leaves that commit whole, whatever it
//! leaves of the commits after it, and the next open restores it.
//!
//! The rules this holds, each of which the model in `synced_model.rs` shows needed:
//!
//! - A write of this process begins only once it holds a pin at or before the record's commit (`ensure_pin`), taken
//!   before its transaction begins: a transaction too large for its memory writes pages before it commits.
//! - A sync moves the pin in this order: a read transaction begun, its commit's meta page taken while LMDB's newest
//!   commit is still that one, the record read, the data synced, the record written and synced, and then the old pin
//!   ended (`sync`).
//! - The record's commit never goes back, and a record read is relied on only once this process has synced it.
//! - A process that owes nothing holds no pin, in one step with any write of it that is beginning (`Writing`).
//!
//! And these, of a sync that fails, which the model leaves out:
//!
//! - After a sync fails as it reads or flushes the record, or flushes the data, the process refuses every commit, and
//!   where the data's flush or the record's write failed the record says so.
//! - No record is written once the process refuses its commits: a flush that succeeds after one that failed proves
//!   nothing of the pages the OS may have dropped.
//! - One that wrote nothing to rely on, as a full reader table or a meta page the file can't read fails it, refuses
//!   nothing for good: while the pin is past its bound, which it can't leave until a sync succeeds, a write is refused
//!   before it begins once a sync it asks for has failed so too (`SYNC_BEHIND`, `Store::hold_to_the_bound`), since it
//!   would grow the file past the bound.
//!
//! Locks are taken in one order: the gate, held for reading from a read transaction's beginning until it is the pin,
//! then the record's, then the data file's handle; the gate is read again within, which it lets a reader do whenever
//! it holds it. The pin's state is never held while the gate is waited for, since a growth holds the gate for writing
//! while it ends the pin.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use heed::{Env, RoTxn, WithoutTls};
use parking_lot::{RecursiveRwLock, RecursiveRwLockReadGuard};

use super::boot::Boot;
use super::call_reads::CallReads;
use super::idle_reads::Clear;
use super::reach::{self, MetaPage};
use super::record::{self, Mode, Record, RecordFile, State};
use crate::error::{Error, ErrorCode, Result};
use crate::sync::{PlainMutex, PlainMutexGuard};

/// The record's file and the count of the record in it this process last synced, locked
type RecordGuard<'a> = PlainMutexGuard<'a, (RecordFile, Option<u64>)>;

/// How much the pin may keep from reuse, in the pages commits after its own freed, before a sync is due whatever the
/// time and a write waits for it: the greater of this and a fraction of the file
const BOUND_BYTES: u64 = 16 << 20;
const BOUND_FRACTION: u64 = 4;
/// What the pin keeps is read from the free tree once in this many asks, which reads a few of its pages (a write asks
/// twice, before it begins and once it has committed), and whenever the file has grown by this fraction of the bound,
/// the whole of it, since it was last read, so a write of many pages is never let through on an answer from before it
const WALK_EVERY: u32 = 16;
const WALK_GROWTH: u64 = 8;

/// The pin, the record and what this process knows of both
pub(super) struct Kept {
    env: Env<WithoutTls>,
    /// The store's gate, held for reading wherever the map is read through: a transaction's beginning, a sync, and
    /// the environment's sizes
    gate: Arc<RecursiveRwLock<()>>,
    idle: Clear,
    /// The reads waiting on transactions held from their calls, which a change of the map's size runs first
    calls: Arc<CallReads<RoTxn<'static, WithoutTls>>>,
    dir: PathBuf,
    page: usize,
    boot: Boot,
    /// The data file, read for meta pages and root pages
    data: PlainMutex<File>,
    /// The record's file, its lock within this process, with the count of the record this process last knew synced;
    /// the lock between processes is `record_lock`, taken under it
    record: PlainMutex<(RecordFile, Option<u64>)>,
    record_lock: File,
    state: PlainMutex<Pinned>,
    /// A test's pin that holds nothing, to show what the replay finds without it
    #[cfg(test)]
    pub(super) unpinned: std::sync::atomic::AtomicBool,
    /// The fixed part of the bound, which a test makes small
    bound: std::sync::atomic::AtomicU64,
    /// Whether the pin was past the bound when the free tree was last read, the asks since, and the file's last page
    /// then
    past: std::sync::atomic::AtomicBool,
    asked: std::sync::atomic::AtomicU32,
    walked_at: std::sync::atomic::AtomicU64,
    /// The syncs of the record file this process's syncs have made, which a test counts
    #[cfg(test)]
    pub(super) record_syncs: std::sync::atomic::AtomicU64,
}

struct Pinned {
    pin: Option<Pin>,
    /// The record's commit as this process last synced it: never more than the record on disk
    remembered: Option<u64>,
    /// This process's last commit
    last: u64,
    /// Writes of this process that hold a pin and have not ended
    writing: usize,
    /// The sync that failed, after which every commit is refused
    failed: Option<Error>,
}

struct Pin {
    /// Held for the pages it keeps, never read through; none only where a test turned the pin off
    _txn: Option<RoTxn<'static, WithoutTls>>,
    /// The commit it is at: the pages commits from it on free are what it keeps from reuse
    commit: u64,
}

/// A write of this process that holds the pin: while one is, no sync ends it. Dropped when the write has ended, its
/// commit, if it made one, accounted for (`Kept::committed`)
pub(crate) struct Writing(Arc<Kept>);

impl Drop for Writing {
    fn drop(&mut self) {
        let mut state = self.0.state.lock();
        state.writing -= 1;
        // A write that made no commit leaves nothing owed: its pin goes with it rather than at a sync that never comes
        if state.writing == 0 && !owes(&state) {
            state.pin = None;
        }
    }
}

/// Whether a pin that keeps `kept` bytes from reuse in a file of `file` bytes keeps more than the bound: the greater
/// of `fixed` bytes and a fraction of the file
fn beyond(kept: u64, file: u64, fixed: u64) -> bool {
    kept > bound_in(file, fixed)
}

/// The bound in a file of `file` bytes: the greater of `fixed` bytes and a fraction of the file
fn bound_in(file: u64, fixed: u64) -> u64 {
    fixed.max(file / BOUND_FRACTION)
}

/// Whether the free tree is read again at the `asked`th ask, the file of `file` bytes having grown by `grown` since it
/// was last read: once in `WALK_EVERY` asks, and once the file has grown by a `WALK_GROWTH`th of the bound
fn walk_due(asked: u32, grown: u64, file: u64, fixed: u64) -> bool {
    asked.is_multiple_of(WALK_EVERY) || grown >= bound_in(file, fixed) / WALK_GROWTH
}

/// What every commit after a failed sync is refused with
fn refused(state: &Pinned) -> Option<Error> {
    state.failed.as_ref().map(|failed| {
        Error::new(
            failed.code,
            format!(
                "An earlier sync failed, so this process commits nothing more until the database is opened again: {}",
                failed.message
            ),
        )
    })
}

/// Whether the process has a commit after the record's
fn owes(state: &Pinned) -> bool {
    state.remembered.is_none_or(|remembered| state.last > remembered)
}

impl Kept {
    /// The grouped mode's state for an environment just opened, whose record an open has read and synced:
    /// `remembered` is its commit where it is a grouped record the file bears out
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        env: Env<WithoutTls>,
        gate: Arc<RecursiveRwLock<()>>,
        idle: Clear,
        calls: Arc<CallReads<RoTxn<'static, WithoutTls>>>,
        dir: &Path,
        page: usize,
        boot: Boot,
        record: RecordFile,
        remembered: Option<u64>,
    ) -> Result<Self> {
        Ok(Self {
            env,
            gate,
            idle,
            calls,
            dir: dir.to_owned(),
            page,
            boot,
            data: PlainMutex::new(File::open(dir.join("data.mdb"))?),
            record: PlainMutex::new((record, None)),
            record_lock: record::lock_file(dir, record::RECORD_LOCK)?,
            state: PlainMutex::new(Pinned { pin: None, remembered, last: 0, writing: 0, failed: None }),
            #[cfg(test)]
            unpinned: std::sync::atomic::AtomicBool::new(false),
            bound: std::sync::atomic::AtomicU64::new(BOUND_BYTES),
            past: std::sync::atomic::AtomicBool::new(false),
            asked: std::sync::atomic::AtomicU32::new(0),
            walked_at: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            record_syncs: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// The pin for a transaction: it, or nothing where a test turned the pin off
    fn pin(&self, txn: RoTxn<'static, WithoutTls>) -> Pin {
        let commit = txn.id() as u64;
        // A new pin keeps nothing yet: what an earlier one kept is asked of the free tree again
        self.past.store(false, std::sync::atomic::Ordering::Relaxed);
        self.asked.store(0, std::sync::atomic::Ordering::Relaxed);
        #[cfg(test)]
        if self.unpinned.load(std::sync::atomic::Ordering::Relaxed) {
            return Pin { _txn: None, commit };
        }
        Pin { _txn: Some(txn), commit }
    }

    /// A read transaction at the newest commit, with the gate held for it: held until the transaction is the pin, so
    /// no growth of the map runs while it is open and not yet a pin a growth can end
    fn begin(&self) -> heed::Result<(RoTxn<'static, WithoutTls>, RecursiveRwLockReadGuard<'_, ()>)> {
        let gate = self.gate.read();
        let txn = super::with_reader_recovery(&self.env, || self.env.clone().static_read_txn())?;
        Ok((txn, gate))
    }

    /// Takes the map size another process grew the file to, as the store does, the pin ended first: LMDB changes a
    /// map's size only while the process has no transaction. Gives way, taking nothing, where the gate isn't free
    /// soon (`super::gate_to_adopt`): the caller begins its transaction again
    pub(super) fn adopt(&self) -> Result<()> {
        let Some(_gate) = super::gate_to_adopt(&self.gate) else { return Ok(()) };
        self.calls.run_all();
        self.idle.clear();
        self.end_pin();
        // SAFETY: the gate held for writing keeps every transaction of this process from beginning or running, the
        // reads asked at calls have run and their transactions are ended, the idle ones are dropped and the pin, which
        // holds no gate, is ended: no transaction is open, and nothing else
        // reads through the map, since a sync and each read of the environment's sizes hold the gate for reading
        unsafe { super::adopt_grown_map(&self.env, &self.dir, false) }
    }

    /// The meta page of the commit `txn` is at, taken only where it is whole: the commit two after it writes over it
    /// field by field, so it is taken only if LMDB's newest commit is still that one once it is read. The newest is
    /// the lock file's, which a read transaction begins at: the meta pages' newest is ahead of it for good where a
    /// writer died between writing its meta page and telling the lock file, and no sync would take a page again. None
    /// where a commit landed as it was read, for the caller to begin again at the newer one. A read of the file that
    /// fails is an `IO` error and not that: begun again for it, the caller would read the page for as long as the read
    /// fails. The caller holds the gate
    fn take_meta(&self, txn: &RoTxn<'static, WithoutTls>) -> Result<Option<MetaPage>> {
        let commit = txn.id() as u64;
        let meta = self.read_meta(commit % 2).map_err(|error| {
            Error::new(
                ErrorCode::Io,
                format!(
                    "The meta page of commit {commit} couldn't be read from the data file of the database at {}: \
                     {error}. Nothing was synced or recorded, and nothing of a write that was to sync first was made: \
                     a durable wait or the write may be tried again, and goes on once the file reads",
                    self.dir.display()
                ),
            )
        })?;
        let Some(meta) = meta.filter(|meta| meta.txnid == commit) else { return Ok(None) };
        let newest = match super::with_reader_recovery(&self.env, || self.env.clone().static_read_txn()) {
            Ok(newest) => newest.id() as u64,
            // A map grown since: begun again, the caller meets it and takes it
            Err(heed::Error::Mdb(heed::MdbError::MapResized)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok((newest == commit).then_some(meta))
    }

    /// The data file's meta page in `slot`, read by a seek
    fn read_meta(&self, slot: u64) -> std::io::Result<Option<MetaPage>> {
        failure_point!(META_READ at &self.dir, std::io::Error::other("Injected meta page read failure"));
        reach::meta_page(&mut self.data.lock(), self.page, slot)
    }

    /// A sync: the pin moved to the newest commit, which is on disk and named by the record when this returns. `adopt`
    /// takes a map another process grew, as a transaction's beginning may find it
    pub(super) fn sync(&self, adopt: &dyn Fn() -> Result<()>) -> Result<()> {
        self.sync_holding(adopt, None)
    }

    /// `sync`, with the record's lock where the caller holds it already
    fn sync_holding(&self, adopt: &dyn Fn() -> Result<()>, held: Option<RecordGuard<'_>>) -> Result<()> {
        failure_point!(SYNC_BEGIN at &self.dir, Error::new(ErrorCode::ReadersFull, "Injected full reader table"));
        loop {
            let (txn, _gate) = match self.begin() {
                Ok(begun) => begun,
                Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                    run_point!(SYNC_MAP_GROWN at &self.dir);
                    adopt()?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            run_point!(SYNC_PINNED at &self.dir);
            // A commit landed as the page was read: the newer one is taken instead
            let Some(meta) = self.take_meta(&txn)? else { continue };
            run_point!(SYNC_TAKEN at &self.dir);
            return self.record_at(txn, &meta, held);
        }
    }

    /// What a writable open does once its own commits are made: the newest commit synced and recorded where the
    /// record it relies on names an earlier one, or none. Answers whether it synced
    pub(super) fn settle(&self, adopt: &dyn Fn() -> Result<()>) -> Result<bool> {
        let newest = {
            let _gate = self.gate.read();
            self.env.info().last_txn_id as u64
        };
        if self.state.lock().remembered.is_some_and(|remembered| newest <= remembered) {
            return Ok(false);
        }
        self.sync(adopt)?;
        Ok(true)
    }

    /// The data synced and the record written for the commit `txn` holds, whose meta page is `meta`, and then the pin
    /// moved to it
    fn record_at(&self, txn: RoTxn<'static, WithoutTls>, meta: &MetaPage, held: Option<RecordGuard<'_>>) -> Result<()> {
        let commit = txn.id() as u64;
        let named = {
            let mut held = held.unwrap_or_else(|| self.record.lock());
            // A failure refused the process's commits, which may be built on pages the OS dropped: no record may name
            // them, though a flush after it succeeds. Read under the record, which a sync that fails holds until then
            if let Some(refused) = self.refused() {
                return Err(refused);
            }
            let (file, synced) = &mut *held;
            let _locked = record::exclusive(&self.record_lock)?;
            let on_disk = file.read().map_err(|error| self.fail(error))?;
            // Relied on, and its other slot about to be written over, only once synced: its writer may have died
            // before syncing it, and a loss could otherwise leave neither of the two newest records on disk. One this
            // process wrote or synced is on disk already, and isn't flushed again, a full flush of the drive on Apple
            let count = on_disk.as_ref().map(|r| r.count);
            if count.is_some() && count != *synced {
                file.sync().map_err(|error| self.fail(error))?;
                self.synced_record();
                *synced = count;
            }
            if let Some(failed) = on_disk.as_ref().filter(|r| r.mode == Mode::Grouped && r.state == State::Failed) {
                return Err(self.fail(Error::new(
                    ErrorCode::Io,
                    format!(
                        "A sync of the database at {} failed in another process, so what it committed since commit \
                         {} may not be on disk. Close every process that has it open and open it again",
                        self.dir.display(),
                        failed.commit
                    ),
                )));
            }
            if let Err(error) = self.sync_data() {
                return Err(self.failed_sync(file, on_disk.as_ref(), error));
            }
            run_point!(SYNC_DATA at &self.dir);
            let later = on_disk.as_ref().is_none_or(|r| r.mode != Mode::Grouped || commit > r.commit);
            let named = if later {
                let roots = record::roots_checksum(&mut self.data.lock(), self.page, meta.roots())?;
                let written = Record {
                    count: on_disk.as_ref().map_or(1, |r| r.count + 1),
                    mode: Mode::Grouped,
                    state: State::Kept,
                    commit,
                    body: meta.body,
                    roots,
                    boot: self.boot,
                    restored: on_disk.as_ref().and_then(|r| r.restored),
                };
                if let Err(error) = file.write(&written) {
                    return Err(self.failed_sync(file, on_disk.as_ref(), error));
                }
                self.synced_record();
                *synced = Some(written.count);
                commit
            } else {
                // Another process's later record, synced above
                on_disk.as_ref().map_or(commit, |r| r.commit)
            };
            run_point!(SYNC_RECORDED at &self.dir);
            named
        };
        let ended = {
            let mut state = self.state.lock();
            state.remembered = Some(state.remembered.map_or(named, |remembered| remembered.max(named)));
            // The old pin goes only now, the record that names a later commit being on disk
            let old = state.pin.replace(self.pin(txn));
            if state.writing == 0 && !owes(&state) {
                state.pin = None;
            }
            old
        };
        drop(ended);
        Ok(())
    }

    /// `sync` in a process that is exiting, whose other threads may be gone: one that ended while it held the record
    /// leaves the record as it was, and the data is synced all the same
    pub(super) fn sync_at_exit(&self, deadline: std::time::Instant) -> Result<()> {
        // A thread that ended while it held the record, the data file's handle or the pin's state holds it for good,
        // where a live one holds it a moment, or the record until its sync ends: each is waited for until the one
        // deadline all three share, and past it the data is synced alone, as for a thread that ended. The record is
        // kept from here, so that no live thread takes it and holds it through a sync of its own; the other two a live
        // thread holds only while it reads a few pages or the pin, and one that ended can't take them again
        let record = self.record.try_lock_until(deadline);
        let free = record.is_some()
            && self.data.try_lock_until(deadline).is_some()
            && self.state.try_lock_until(deadline).is_some();
        if !free {
            drop(record);
            self.sync_data()?;
            return Err(Error::new(
                ErrorCode::Io,
                "The record beside the data file, its handle or the pin's state was in use as the process exited, so \
                 the record names an earlier commit than the last one synced",
            ));
        }
        self.sync_holding(&|| Err(Error::new(ErrorCode::Io, "Another process grew the map as this one exited")), record)
    }

    /// Counts a sync of the record file, for a test
    fn synced_record(&self) {
        #[cfg(test)]
        self.record_syncs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Syncs the data file, under the gate: LMDB reads the meta pages through the map to know what to flush
    fn sync_data(&self) -> Result<()> {
        failure_point!(SYNC at &self.dir, Error::new(ErrorCode::Io, "Injected sync failure"));
        let _gate = self.gate.read();
        self.env.force_sync().map_err(Into::into)
    }

    /// A failed sync, or a record that couldn't be written: the record written `failed` at its own commit, and every
    /// commit of this process refused from now on
    fn failed_sync(&self, file: &mut RecordFile, on_disk: Option<&Record>, error: Error) -> Error {
        let written = match on_disk {
            Some(record) if record.mode == Mode::Grouped => file.write(&record.next(State::Failed)),
            _ => Err(Error::new(ErrorCode::Io, "no record of the grouped mode to mark")),
        };
        let error = match written {
            Ok(()) => error,
            Err(unwritten) => Error::new(
                error.code,
                format!(
                    "{} The record beside the data file couldn't be marked failed either ({}), so an open of the \
                     database in this boot opens at its newest commit, whose pages the failed sync may not have \
                     written",
                    error.message, unwritten.message
                ),
            ),
        };
        self.fail(error)
    }

    /// Refuses every commit of this process from now on, keeping its pin until it ends
    fn fail(&self, error: Error) -> Error {
        self.state.lock().failed.get_or_insert_with(|| error.clone());
        error
    }

    /// Makes sure this process holds a pin at or before the record's commit, for a write about to begin its
    /// transaction: a pin it holds, or one at the newest commit, which when it is later than the record's is synced
    /// and recorded first. Refused once a sync has failed
    pub(super) fn ensure_pin(self: &Arc<Self>, adopt: &dyn Fn() -> Result<()>) -> Result<Writing> {
        {
            let mut state = self.state.lock();
            if let Some(refused) = refused(&state) {
                return Err(refused);
            }
            state.writing += 1;
        }
        let writing = Writing(Arc::clone(self));
        loop {
            if self.state.lock().pin.is_some() {
                return Ok(writing);
            }
            let (txn, _gate) = match self.begin() {
                Ok(begun) => begun,
                Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                    adopt()?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let commit = txn.id() as u64;
            {
                let mut state = self.state.lock();
                if state.pin.is_some() {
                    return Ok(writing);
                }
                // A pin at the record's commit or before, or at a file's first commit, which no commit is before
                if commit == 0 || state.remembered.is_some_and(|remembered| commit <= remembered) {
                    state.pin = Some(self.pin(txn));
                    return Ok(writing);
                }
            }
            // Another process committed since the record this one relies on: a sync and a record first, outside
            // LMDB's write lock, which leave the pin held for this write
            let Some(meta) = self.take_meta(&txn)? else { continue };
            run_point!(SYNC_TAKEN at &self.dir);
            self.record_at(txn, &meta, None)?;
        }
    }

    /// Whether this process holds a pin and may commit: what a write checks once its transaction has begun, since a
    /// growth may have ended the pin between its making sure of it and its beginning, and a sync may have failed
    pub(super) fn pinned(&self) -> bool {
        let state = self.state.lock();
        state.pin.is_some() && state.failed.is_none()
    }

    /// Why this process commits nothing, once a sync has failed
    pub(super) fn refused(&self) -> Option<Error> {
        refused(&self.state.lock())
    }

    /// The record's lock, which a test holds as a sync of another thread would
    #[cfg(test)]
    pub(super) fn record_guard(&self) -> RecordGuard<'_> {
        self.record.lock()
    }

    /// The failure that refused this process's commits, as it was met
    pub(super) fn failure(&self) -> Option<Error> {
        self.state.lock().failed.clone()
    }

    /// Ends the pin, for a growth of the map, which the caller holds the gate for writing for
    pub(super) fn end_pin(&self) {
        let ended = self.state.lock().pin.take();
        drop(ended);
    }

    /// This process committed transaction `txnid`
    pub(super) fn committed(&self, txnid: u64) {
        let mut state = self.state.lock();
        state.last = state.last.max(txnid);
    }

    /// Whether what the pin keeps from reuse is past the bound, which makes a sync due whatever the time and holds a
    /// write back until it lands. It is what commits from the pin's on freed, which no commit may take while the pin
    /// is there: where a commit rewrites what it changes, the file grows by all of it once the pages freed before the
    /// pin run out. Read from the free tree once in `WALK_EVERY` asks, and asked again of it as the pin moves; past
    /// where the newest commit's meta page, which names the tree, can't be read
    pub(super) fn past_bound(&self) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(pinned) = self.state.lock().pin.as_ref().map(|pin| pin.commit) else { return false };
        let last = {
            let _gate = self.gate.read();
            self.env.info().last_page_number as u64
        };
        let page = self.page as u64;
        let grown = last.saturating_sub(self.walked_at.load(Relaxed)).saturating_mul(page);
        let file = last.saturating_add(1).saturating_mul(page);
        if !walk_due(self.asked.fetch_add(1, Relaxed), grown, file, self.bound.load(Relaxed)) {
            return self.past.load(Relaxed);
        }
        let past = match self.kept_from_reuse(pinned) {
            Ok(Some((kept, file))) => beyond(kept, file, self.bound.load(Relaxed)),
            Ok(None) => return self.past.load(Relaxed),
            // The meta page couldn't be taken: it doesn't read, or the second read transaction `take_meta` begins
            // couldn't begin. A sync takes it the same way, beginning that transaction while it holds its pin's, so no
            // sync can move the pin either: taken as past the bound, so a write waits for a sync, and is refused if
            // that fails too, rather than growing the file for as long as this lasts. A sync that succeeds moves the
            // pin, and the new one asks again. Where the walk's own transaction can't begin, the last answer stands,
            // as it does for a full reader table, which refuses a sync too
            Err(_) => true,
        };
        // An answer for a pin that has moved since is no answer: the new pin asks again. Kept under the state, which
        // a new pin is made under, so none is made between the check and the stores
        let state = self.state.lock();
        let current = state.pin.as_ref().is_some_and(|pin| pin.commit == pinned);
        if current {
            self.past.store(past, Relaxed);
            self.walked_at.store(last, Relaxed);
        }
        drop(state);
        past && current
    }

    /// The bytes of the pages commits from `pinned` on freed, and of the file's pages, as the newest commit's free tree
    /// and meta page say: read under a read transaction at that commit, which keeps the tree's pages as they are. None
    /// where no read transaction could begin, or a commit landed as its meta page was read; an error where the meta
    /// page couldn't be taken otherwise, as where the file can't be read
    fn kept_from_reuse(&self, pinned: u64) -> Result<Option<(u64, u64)>> {
        let _gate = self.gate.read();
        let Ok(txn) = super::with_reader_recovery(&self.env, || self.env.clone().static_read_txn()) else {
            return Ok(None);
        };
        // Whole, as a sync takes it: the commit two after this one writes over its meta page field by field
        let Some(meta) = self.take_meta(&txn)? else { return Ok(None) };
        let page = self.page as u64;
        let kept = reach::freed_from(&mut self.data.lock(), self.page, meta.roots()[0], pinned).saturating_mul(page);
        Ok(Some((kept, meta.last_page().saturating_add(1).saturating_mul(page))))
    }

    /// Sets the fixed part of the bound, for a test of what it does at a size a test reaches
    #[cfg(test)]
    pub(super) fn set_bound(&self, bytes: u64) {
        self.bound.store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// The commit the pin is at, for a test
    #[cfg(test)]
    pub(super) fn pinned_at(&self) -> Option<u64> {
        self.state.lock().pin.as_ref().map(|pin| pin.commit)
    }

    /// The record's commit as this process relies on it, for a test
    #[cfg(test)]
    pub(super) fn remembered(&self) -> Option<u64> {
        self.state.lock().remembered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failpoints::{self, points};
    use crate::store::{Store, StoreOptions};

    /// The grouped mode, its timers too long to sync while a test runs
    fn grouped() -> StoreOptions {
        StoreOptions {
            sync_idle: std::time::Duration::from_secs(600),
            sync_interval: std::time::Duration::from_secs(600),
            ..Default::default()
        }
    }

    fn put(store: &Store, key: &str) {
        let key = crate::format::key::encode(&[key.into()]);
        store.write(|txn, db| db.put(txn, &key, b"1").map_err(Into::into), false).unwrap();
    }

    fn record_commit(dir: &std::path::Path) -> Option<u64> {
        RecordFile::open(dir, false).unwrap()?.read().unwrap().map(|record| record.commit)
    }

    #[test]
    fn a_sync_after_the_process_refused_its_commits_names_no_later_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), grouped()).unwrap();
        put(&store, "a");
        let recorded = record_commit(dir.path());
        // As a sync whose data failed leaves the process where the record couldn't be marked failed either: refused,
        // the record as it was. A flush after it that succeeds proves nothing of the pages the failed one dropped
        let kept = store.kept.as_ref().unwrap();
        kept.fail(Error::new(ErrorCode::Io, "Injected sync failure"));
        assert_eq!(kept.sync(&|| Ok(())).unwrap_err().message, kept.refused().unwrap().message);
        assert_eq!(record_commit(dir.path()), recorded, "the record names a commit built on what the sync dropped");
        assert!(store.close().is_err());
    }

    /// The pin taken as past the bound when the free tree was last read, and not read again before a walk is due
    fn past_the_bound(store: &Store) {
        use std::sync::atomic::Ordering::Relaxed;
        let kept = store.kept.as_ref().unwrap();
        kept.past.store(true, Relaxed);
        kept.asked.store(1, Relaxed);
        kept.walked_at.store(store.env.info().last_page_number as u64, Relaxed);
    }

    #[test]
    fn a_write_past_the_bound_whose_sync_failed_short_of_a_flush_is_refused_with_its_failure_and_the_next_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), grouped()).unwrap();
        put(&store, "a");
        past_the_bound(&store);
        // As a reader table full for a moment fails it: the write, which would grow the file past the bound, is
        // refused, and its caller may try it again
        let failing = failpoints::fail_while_at(points::SYNC_BEGIN, &store.path);
        let key = crate::format::key::encode(&["b".into()]);
        let refused = store.write(|txn, db| db.put(txn, &key, b"1").map_err(Into::into), false).unwrap_err();
        assert_eq!(refused.code, ErrorCode::SyncBehind);
        assert!(refused.message.contains("Injected full reader table"), "{refused}");
        assert!(store.kept.as_ref().unwrap().refused().is_none(), "the process refuses its commits");
        drop(failing);
        put(&store, "b");
        store.close().unwrap();
    }

    #[test]
    fn a_write_past_the_bound_whose_sync_failed_for_good_is_refused_as_every_commit_is() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), grouped()).unwrap();
        put(&store, "a");
        past_the_bound(&store);
        let failing = failpoints::fail_while_at(points::SYNC, &store.path);
        let key = crate::format::key::encode(&["b".into()]);
        let refused = store.write(|txn, db| db.put(txn, &key, b"1").map_err(Into::into), false).unwrap_err();
        assert_eq!(refused.message, store.kept.as_ref().unwrap().refused().unwrap().message);
        drop(failing);
        assert!(store.close().is_err());
    }

    #[test]
    fn a_write_while_the_meta_page_can_t_be_read_is_held_as_past_the_bound_and_refused_when_its_sync_fails() {
        use std::sync::atomic::Ordering::Relaxed;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), grouped()).unwrap();
        // Owed, so the pin stays, and not past the bound when last read, the next ask reading it again
        put(&store, "a");
        let kept = store.kept.as_ref().unwrap();
        assert!(kept.pinned_at().is_some() && !kept.past.load(Relaxed));
        kept.asked.store(0, Relaxed);
        // No sync can move the pin while the meta page can't be read: a write that went on would grow the file for as
        // long as the read fails
        let failing = failpoints::fail_while_at(points::META_READ, &store.path);
        let key = crate::format::key::encode(&["b".into()]);
        let refused = store.write(|txn, db| db.put(txn, &key, b"1").map_err(Into::into), false).unwrap_err();
        assert_eq!(refused.code, ErrorCode::SyncBehind, "{refused}");
        assert!(refused.message.contains("Injected meta page read failure"), "{refused}");
        drop(failing);
        // Once it reads, the write's sync moves the pin and the write goes on
        put(&store, "b");
        store.close().unwrap();
    }

    #[test]
    fn a_unit_s_owner_beginning_again_is_refused_before_it_would_wait_for_a_sync() {
        use std::sync::atomic::Ordering::Relaxed;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), grouped()).unwrap();
        put(&store, "a");
        let (txn, gate, writer) = store.begin().unwrap();
        // Past the bound, as last read: a write would wait for a sync, and one asked for would be held here
        let kept = store.kept.as_ref().unwrap();
        past_the_bound(&store);
        let (paused, held) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        failpoints::run_once_at(points::SYNC_DATA, &store.path, move || {
            let _ = paused.send(());
            let _ = released.recv_timeout(std::time::Duration::from_secs(5));
        });
        assert_eq!(store.begin().err().map(|error| error.code), Some(ErrorCode::UnitOpen));
        assert!(held.try_recv().is_err(), "the owner waited on a sync before it was refused");
        drop(release);
        txn.abort();
        drop(gate);
        writer.release();
        store.end_unit();
        kept.past.store(false, Relaxed);
        store.close().unwrap();
    }

    #[test]
    fn the_exit_s_sync_waits_for_a_lock_a_live_thread_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), grouped()).unwrap();
        put(&store, "a");
        let kept = Arc::clone(store.kept.as_ref().unwrap());
        let (taken, held) = std::sync::mpsc::channel();
        // The record held as a sync holds it, for a moment within the exit's wait
        let holder = std::thread::spawn(move || {
            let _record = kept.record.lock();
            taken.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(50));
        });
        held.recv().unwrap();
        let newest = store.env.info().last_txn_id as u64;
        store.close_at_exit(std::time::Duration::from_secs(10)).unwrap();
        holder.join().unwrap();
        assert_eq!(record_commit(dir.path()), Some(newest), "the exit synced and recorded the newest commit");
    }

    #[test]
    fn the_exit_s_sync_takes_no_lock_a_thread_holds_past_its_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), StoreOptions::default()).unwrap();
        let kept = Arc::clone(store.kept.as_ref().unwrap());
        // As a thread that ended holding it would: each of the locks the record takes, held in turn
        for which in 0..3 {
            // Each held until the end of its turn of the loop, after the exit has answered
            let (_record, _data, _state);
            match which {
                0 => _record = kept.record.lock(),
                1 => _data = kept.data.lock(),
                _ => _state = kept.state.lock(),
            }
            let exiting = Arc::clone(&kept);
            let (answered, answer) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
                let _ = answered.send(exiting.sync_at_exit(deadline));
            });
            let answer = answer.recv_timeout(std::time::Duration::from_secs(10));
            let refused = answer.unwrap_or_else(|_| panic!("the exit waited on lock {which} past its deadline"));
            assert_eq!(refused.unwrap_err().code, ErrorCode::Io, "lock {which}");
        }
        drop(kept);
        store.close().unwrap();
    }

    #[test]
    fn the_bound_is_the_greater_of_its_fixed_size_and_a_quarter_of_the_file() {
        // A file of 4 MiB: a quarter is 1 MiB, under a fixed 2 MiB
        assert!(!beyond(2 << 20, 4 << 20, 2 << 20), "at the bound");
        assert!(beyond((2 << 20) + 1, 4 << 20, 2 << 20), "a byte past it");
        // A file of 40 MiB: a quarter, 10 MiB, is over the fixed MiB
        assert!(!beyond(10 << 20, 40 << 20, 1 << 20), "at a quarter of the file");
        assert!(beyond((10 << 20) + 1, 40 << 20, 1 << 20), "a byte past it");
    }

    #[test]
    fn the_free_tree_is_read_again_once_in_16_asks_and_once_the_file_grows_by_an_eighth_of_the_bound() {
        let (file, fixed) = (40 << 20, 1 << 20);
        assert!(walk_due(0, 0, file, fixed), "the first ask");
        assert!(!walk_due(1, 0, file, fixed));
        assert!(walk_due(16, 0, file, fixed), "the 16th after it");
        // The bound is a quarter of the file, 10 MiB, not the fixed MiB: an eighth of it is 1.25 MiB
        assert!(walk_due(1, (10 << 20) / 8, file, fixed), "grown by an eighth of the bound");
        assert!(!walk_due(1, (10 << 20) / 8 - 1, file, fixed), "a byte short of it");
    }
}
