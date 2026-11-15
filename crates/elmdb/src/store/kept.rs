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
//! - After a sync fails the record says so, and the process refuses every commit.
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
use super::idle_reads::Clear;
use super::reach::{self, MetaPage};
use super::record::{self, Mode, Record, RecordFile, State};
use crate::error::{Error, ErrorCode, Result};
use crate::sync::PlainMutex;

/// How far the file's last page may move past the pin's before a sync is due, whatever the time: the greater of this
/// and a fraction of the file
const BOUND_BYTES: u64 = super::MAP_STEP as u64;
const BOUND_FRACTION: u64 = 4;

/// The pin, the record and what this process knows of both
pub(super) struct Kept {
    env: Env<WithoutTls>,
    /// The store's gate, held for reading wherever the map is read through: a transaction's beginning, a sync, and
    /// the environment's sizes
    gate: Arc<RecursiveRwLock<()>>,
    idle: Clear,
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
    /// The file's last page as the pin was taken, which `past_bound` measures from
    last_page: u64,
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

/// What every commit after a failed sync is refused with
fn refused(state: &Pinned) -> Option<Error> {
    state.failed.as_ref().map(|failed| {
        Error::new(
            failed.code,
            format!(
                "An earlier sync failed, so nothing is committed until the database is opened again, at the last \
                 commit known to be on disk: {}",
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
        })
    }

    /// The pin for a transaction: it, or nothing where a test turned the pin off
    fn pin(&self, txn: RoTxn<'static, WithoutTls>, last_page: u64) -> Pin {
        #[cfg(test)]
        if self.unpinned.load(std::sync::atomic::Ordering::Relaxed) {
            return Pin { _txn: None, last_page };
        }
        Pin { _txn: Some(txn), last_page }
    }

    /// A read transaction at the newest commit, with the gate held for it: held until the transaction is the pin, so
    /// no growth of the map runs while it is open and not yet a pin a growth can end
    fn begin(&self) -> heed::Result<(RoTxn<'static, WithoutTls>, RecursiveRwLockReadGuard<'_, ()>)> {
        let gate = self.gate.read();
        let txn = super::with_reader_recovery(&self.env, || self.env.clone().static_read_txn())?;
        Ok((txn, gate))
    }

    /// Takes the map size another process grew the file to, as the store does, the pin ended first: LMDB changes a
    /// map's size only while the process has no transaction
    pub(super) fn adopt(&self) -> Result<()> {
        let _gate = self.gate.write();
        self.idle.clear();
        self.end_pin();
        // SAFETY: the gate held for writing keeps every transaction of this process from beginning or running, the
        // idle ones are dropped and the pin, which holds no gate, is ended: no transaction is open, and nothing else
        // reads through the map, since a sync and each read of the environment's sizes hold the gate for reading
        unsafe { super::adopt_grown_map(&self.env, &self.dir, false) }
    }

    /// The meta page of the commit `txn` is at, taken only where it is whole: the commit two after it writes over it
    /// field by field, so it is taken only if LMDB's newest commit is still that one once it is read. The newest is
    /// the lock file's, which a read transaction begins at: the meta pages' newest is ahead of it for good where a
    /// writer died between writing its meta page and telling the lock file, and no sync would take a page again. The
    /// caller holds the gate
    fn take_meta(&self, txn: &RoTxn<'static, WithoutTls>) -> Result<Option<MetaPage>> {
        let commit = txn.id() as u64;
        let meta = reach::meta_page(&mut self.data.lock(), self.page, commit % 2);
        let Some(meta) = meta.filter(|meta| meta.txnid == commit) else { return Ok(None) };
        let newest = match super::with_reader_recovery(&self.env, || self.env.clone().static_read_txn()) {
            Ok(newest) => newest.id() as u64,
            // A map grown since: begun again, the caller meets it and takes it
            Err(heed::Error::Mdb(heed::MdbError::MapResized)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok((newest == commit).then_some(meta))
    }

    /// A sync: the pin moved to the newest commit, which is on disk and named by the record when this returns. `adopt`
    /// takes a map another process grew, as a transaction's beginning may find it
    pub(super) fn sync(&self, adopt: &dyn Fn() -> Result<()>) -> Result<()> {
        loop {
            let (txn, _gate) = match self.begin() {
                Ok(begun) => begun,
                Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                    adopt()?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            run_point!(SYNC_PINNED at &self.dir);
            // A commit landed as the page was read: the newer one is taken instead
            let Some(meta) = self.take_meta(&txn)? else { continue };
            return self.record_at(txn, &meta);
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
    fn record_at(&self, txn: RoTxn<'static, WithoutTls>, meta: &MetaPage) -> Result<()> {
        let commit = txn.id() as u64;
        let named = {
            let mut held = self.record.lock();
            let (file, synced) = &mut *held;
            let _locked = record::exclusive(&self.record_lock)?;
            let on_disk = file.read().map_err(|error| self.fail(error))?;
            // Relied on, and its other slot about to be written over, only once synced: its writer may have died
            // before syncing it, and a loss could otherwise leave neither of the two newest records on disk. One this
            // process wrote or synced is on disk already, and isn't flushed again, a full flush of the drive on Apple
            let count = on_disk.as_ref().map(|r| r.count);
            if count.is_some() && count != *synced {
                file.sync().map_err(|error| self.fail(error))?;
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
            let old = state.pin.replace(self.pin(txn, meta.last_page()));
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
    pub(super) fn sync_at_exit(&self) -> Result<()> {
        if self.record.try_lock().is_none() {
            self.sync_data()?;
            return Err(Error::new(
                ErrorCode::Io,
                "The record beside the data file was being written as the process exited, so it names an earlier \
                 commit than the last one synced",
            ));
        }
        self.sync(&|| Err(Error::new(ErrorCode::Io, "Another process grew the map as this one exited")))
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
            Some(record) if record.mode == Mode::Grouped => file.write(&record.next(Mode::Grouped, State::Failed)),
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
            // Read before the state is taken, which is never held while the gate is waited for
            let last_page = self.env.info().last_page_number as u64;
            {
                let mut state = self.state.lock();
                if state.pin.is_some() {
                    return Ok(writing);
                }
                // A pin at the record's commit or before, or at a file's first commit, which no commit is before
                if commit == 0 || state.remembered.is_some_and(|remembered| commit <= remembered) {
                    state.pin = Some(self.pin(txn, last_page));
                    return Ok(writing);
                }
            }
            // Another process committed since the record this one relies on: a sync and a record first, outside
            // LMDB's write lock, which leave the pin held for this write
            let Some(meta) = self.take_meta(&txn)? else { continue };
            self.record_at(txn, &meta)?;
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

    /// Whether the file's last page is past the pin's by more than the bound, which makes a sync due whatever the time
    pub(super) fn past_bound(&self) -> bool {
        let Some(pinned) = self.state.lock().pin.as_ref().map(|pin| pin.last_page) else { return false };
        let last = {
            let _gate = self.gate.read();
            self.env.info().last_page_number as u64
        };
        let page = self.page as u64;
        let fixed = self.bound.load(std::sync::atomic::Ordering::Relaxed);
        last.saturating_sub(pinned) * page > fixed.max((last + 1) * page / BOUND_FRACTION)
    }

    /// Sets the fixed part of the bound, for a test of what it does at a size a test reaches
    #[cfg(test)]
    pub(super) fn set_bound(&self, bytes: u64) {
        self.bound.store(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    /// The record's commit as this process relies on it, for a test
    #[cfg(test)]
    pub(super) fn remembered(&self) -> Option<u64> {
        self.state.lock().remembered
    }
}
