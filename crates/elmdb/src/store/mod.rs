//! The store: one LMDB environment, its one database (`format::DB_NAME`), the format stamp, reads, and writes with
//! group commit and map growth (`docs/architecture/engine.md`, Durability; `docs/architecture/format.md`).
//!
//! Every transaction holds `gate` for reading, and growing the map holds it for writing, since LMDB grows a map only
//! while its process has no transaction open. So a read and a write never call each other: a write reads through its
//! own transaction. The gate lets a reader in whenever another holds it, so an open unit, which holds it across calls,
//! never waits on a growth that waits on the unit.
//!
//! A transaction isn't all that reads through the map. LMDB's meta pages are in it, and a sync, the environment's
//! sizes and its statistics each read them through a pointer into it, which a growth leaves pointing at nothing between
//! unmapping the file and mapping it again. Each of those holds the gate for reading too.

mod backup;
mod boot;
pub(crate) mod call_reads;
mod commits;
mod idle_reads;
mod kept;
mod reach;
mod record;
pub use record::{is_backup_lock, left_by_a_backup, written_in};
mod restore;
#[cfg(unix)]
pub(crate) use backup::directories_to_sync as publication_directories;
pub(crate) use backup::publish_new;
pub mod sync;
mod unit_slot;
pub(crate) mod wake;
mod writers;
#[cfg(test)]
pub(crate) use record::{RECORD as RECORD_FILE, of_another_boot as record_of_another_boot};

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use heed::types::Bytes;
use heed::{CommitOrAbort, Env, EnvFlags, EnvOpenOptions, RoTxn, RwTxn, WithoutTls};
use parking_lot::lock_api::ArcRwLockReadGuard;
use parking_lot::{RawRwLockRecursive, RecursiveRwLock, RecursiveRwLockReadGuard, RecursiveRwLockWriteGuard};

use crate::diagnostics::{Op, names};
use crate::engine::mirror::{EdgeDelta, EdgeMirror};
use crate::error::{Error, ErrorCode, Result};
use crate::format::{self, Prefix, meta};
use crate::sync::PlainMutex;
use call_reads::{AtCommit, CallReads};
use idle_reads::IdleReads;
use kept::Kept;
use record::Mode;
use sync::{Durable, GroupCommit, Policy, Syncer, SystemClock};
use unit_slot::UnitSlot;

/// A vector index's graph database, by its distance
pub type CosineGraphs = hannoy::Database<'static, hannoy::distances::Cosine>;
pub type EuclideanGraphs = hannoy::Database<'static, hannoy::distances::Euclidean>;

/// The store's databases: the one of keys (`main`, which `Db` dereferences to), and hannoy's graph databases, one per
/// distance, absent from a read-only environment that predates them
#[derive(Clone, Copy)]
pub struct Db {
    pub main: heed::Database<'static, Bytes, Bytes>,
    pub cosine: Option<CosineGraphs>,
    pub euclidean: Option<EuclideanGraphs>,
}

impl std::ops::Deref for Db {
    type Target = heed::Database<'static, Bytes, Bytes>;
    fn deref(&self) -> &Self::Target {
        &self.main
    }
}

/// How the store opens
#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub read_only: bool,
    /// Group commit's syncs; off, nothing is synced and `durable()` resolves at once
    pub syncs: bool,
    /// Each commit synced by LMDB as it is made, its pages before the meta page that names them: a commit that
    /// returned is on disk, and no group commit runs
    pub sync_each_commit: bool,
    pub sync_idle: Duration,
    pub sync_interval: Duration,
    /// The map's size at open; by default `initial_map_size` of the file
    pub map_size_bytes: Option<usize>,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            read_only: false,
            syncs: true,
            sync_each_commit: false,
            sync_idle: Duration::from_millis(100),
            sync_interval: Duration::from_millis(1000),
            map_size_bytes: None,
        }
    }
}

// The offset `page_size_of` reads is for a 64-bit host; every target this crate ships for is one.
const _: () = assert!(std::mem::size_of::<usize>() == 8, "the meta page offset in page_size_of is for 64-bit hosts");

/// The page size LMDB wrote a file with, which it reads from the file and not from the host: a file written on a host with
/// 4 KiB pages opens with 4 KiB pages on one with 16 KiB. It is `mm_psize`, in the first meta page: the page header (16
/// bytes), then `mm_magic`, `mm_version`, `mm_address` and `mm_mapsize` (24 bytes on a 64-bit host), then the free tree's
/// `md_pad`. It is read by seeking, so the file is never mapped to find it
fn page_size_of(file: &mut std::fs::File, path: &std::path::Path) -> Result<usize> {
    const AT: u64 = 16 + 24;
    file.seek(SeekFrom::Start(AT))?;
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes).map_err(|_| {
        Error::new(ErrorCode::Corrupted, format!("{} is too short to hold a meta page", path.display()))
    })?;
    let page = u32::from_ne_bytes(bytes) as usize;
    if !page.is_power_of_two() || !(512..=32768).contains(&page) {
        return Err(Error::new(
            ErrorCode::Corrupted,
            format!("{}'s meta page names a page size of {page}, which LMDB does not write", path.display()),
        ));
    }
    Ok(page)
}

/// The environment as an open holds it, before a store is made of it: this process's only handle to the files
/// (`open_paths`), with no transaction open but the one the open is in, each ended before the next is begun
#[derive(Clone, Copy)]
struct Opening<'a> {
    env: &'a Env<WithoutTls>,
    /// The database's directory
    dir: &'a Path,
    read_only: bool,
    /// The grouped mode's pin and record, once the open has them: its own commits hold the pin as every write does
    kept: Option<&'a Arc<Kept>>,
}

impl<'a> Opening<'a> {
    /// A transaction begun, the map taken at the size another process grew the file to when LMDB refuses to begin one
    /// in the map as it was opened (`MDB_MAP_RESIZED`): other processes write while this one opens
    fn begin<T>(&self, begin: impl Fn() -> heed::Result<T>) -> Result<T> {
        loop {
            match begin() {
                Err(heed::Error::Mdb(heed::MdbError::MapResized)) => self.adopt()?,
                begun => return Ok(begun?),
            }
        }
    }

    /// Takes the map size another process grew the file to, the pin ended first
    fn adopt(&self) -> Result<()> {
        run_point!(OPEN_MAP_GROWN);
        match self.kept {
            Some(kept) => kept.adopt(),
            // SAFETY: the transaction refused was the open's one, so none is open in this process, which LMDB
            // requires, and no other thread holds the environment yet: the open begins its last transaction before
            // it starts group commit's thread
            None => unsafe { adopt_grown_map(self.env, self.dir, self.read_only) },
        }
    }

    fn read(&self) -> Result<RoTxn<'a, WithoutTls>> {
        let env = self.env;
        self.begin(|| read_txn(env))
    }

    /// A write transaction, begun once the pin is made sure of where the open keeps one
    fn write(&self) -> Result<(heed::RwTxn<'a>, Option<kept::Writing>)> {
        loop {
            let writing = self.kept.map(|kept| kept.ensure_pin(&|| self.adopt())).transpose()?;
            match self.env.write_txn() {
                Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                    drop(writing);
                    self.adopt()?;
                }
                // mutant-skip: replace match guard self.kept.is_some_and\(\|kept\| !kept.pinned\(\)\) with false in Opening<'a>::write
                // A pin ended between its making sure of it and here, which at an open nothing does: no other thread
                // holds the environment yet, and the open takes a grown map only once this write is refused, before
                // it makes sure of its pin again. Held as every write is, and `commits` refuses a commit without it
                Ok(txn) if self.kept.is_some_and(|kept| !kept.pinned()) => txn.abort(),
                begun => return Ok((begun?, writing)),
            }
        }
    }
}

/// How long taking a grown map's size waits for the gate before it gives way
const ADOPT_WAIT: Duration = Duration::from_millis(10);

/// The gate held for writing, to take the size of a map another process grew, or None where it isn't free within
/// `ADOPT_WAIT`, the caller then beginning its transaction again rather than waiting on. A unit holds the gate for
/// reading until it ends, and its owner may be waiting on the caller: on group commit's sync, or on a read asked of
/// another thread. A transaction refused for a growth from before the unit began would otherwise wait behind the unit:
/// once the unit's own beginning has taken the size, the gate lets the unit's read in ahead of a writer that waits.
/// While a unit is open the size is taken already, its transaction having begun, and no other process can commit to
/// grow the file again until it ends, so the transaction begun again isn't refused. The caller holds no part of the
/// gate as it asks: one it held would make every ask give way
fn gate_to_adopt(gate: &RecursiveRwLock<()>) -> Option<RecursiveRwLockWriteGuard<'_, ()>> {
    gate.try_write_for(ADOPT_WAIT)
}

/// Takes the map size another process grew the file to.
///
/// # Safety
///
/// No transaction is open in this process, and nothing else reads through the map LMDB unmaps.
unsafe fn adopt_grown_map(env: &Env<WithoutTls>, dir: &Path, read_only: bool) -> Result<()> {
    #[cfg(windows)]
    let size = if read_only { read_only_map_size(std::fs::metadata(dir.join("data.mdb"))?.len(), None) } else { 0 };
    #[cfg(not(windows))]
    let size = {
        let _ = (dir, read_only);
        0
    };
    // SAFETY: the caller's; a size of 0 adopts the recorded capacity. Windows read-only mappings instead use physical
    // file length, so the next external extension is detected before any page outside that view is read.
    unsafe { env.resize(size) }?;
    Ok(())
}

/// Bytes a file needs to hold `last_page + 1` pages of `page` bytes, refused as `Corrupted` on overflow
fn file_bytes_for_pages(last_page: u64, page: usize, data: &Path) -> Result<u64> {
    last_page
        .checked_add(1)
        .and_then(|pages| pages.checked_mul(page as u64))
        .ok_or_else(|| reach::refused(data, "names more pages than a file can hold"))
}

/// Whether the data file is shorter than the pages its last commit names, having read the trees to see that they reach
/// none of the pages it lacks; a file whose trees do is refused as torn.
///
/// The trees are read as one commit left them, which a read transaction holds for the walk: other processes may be
/// committing, and LMDB gives no page a held snapshot reaches to a later commit. The commit's meta page is read from the
/// file, where the commit after the next writes over it: when it has gone by the time it is read, two commits landed
/// since the snapshot was taken, and the walk starts again from a later one
fn short_of_its_pages(opening: &Opening<'_>, file: &mut std::fs::File, page: usize, data: &Path) -> Result<bool> {
    let env = opening.env;
    loop {
        let snapshot = opening.read()?;
        run_point!(OPEN_SNAPSHOT);
        let Some(meta) = reach::meta(file, page, &snapshot) else {
            if env.info().last_txn_id > snapshot.id() {
                continue;
            }
            // LMDB took the snapshot from a meta page of this file, and no commit has written over it
            return Err(reach::refused(
                data,
                "holds no meta page of the commit LMDB opened it at, and no commit has landed since: something other \
                 than a database handle changed the file as it was opened. Open it again once nothing else writes it",
            ));
        };
        // Read once the snapshot is held: a commit writes its pages before its meta page, and a file only grows under
        // other processes' commits, so every page this commit wrote to the file is in the length read here
        let held = file.metadata()?.len();
        if held >= file_bytes_for_pages(meta.last_page, page, data)? {
            return Ok(false);
        }
        let reached = reach::reach(file, page, held / page as u64, &meta, data)?;
        if let Some(missing) = reached.missing {
            return Err(Error::new(
                ErrorCode::Corrupted,
                format!(
                    "{} is {held} bytes, and page {missing} of its trees is past its end: a power loss may have left it \
                     unwritten. Restore the database from a backup, or from a copy of the file that holds it",
                    data.display()
                ),
            ));
        }
        debug_assert!(reached.highest < held / page as u64, "a clean walk reaches only pages in the file");
        return Ok(true);
    }
}

/// Extends the data file with zeros to the pages its last commit names. Under the environment's write lock, which every
/// commit of every process lands under: the file's length and the last commit's pages are read where neither changes,
/// so the file is only ever made longer, and never cut where another process has written since it was found short
fn extend_to_its_pages(
    opening: &Opening<'_>,
    writers: &writers::Writers,
    file: &std::fs::File,
    page: usize,
) -> Result<()> {
    let env = opening.env;
    let _writer = writers.hold()?;
    // No commit: the file made longer with zeros, before any pin is kept
    let (txn, _) = opening.write()?;
    let needed = file_bytes_for_pages(env.info().last_page_number as u64, page, &opening.dir.join("data.mdb"))?;
    // mutant-skip: replace < with <= in extend_to_its_pages
    // A file of exactly those pages would be set to the length it has
    if file.metadata()?.len() < needed {
        file.set_len(needed)?;
    }
    txn.abort();
    Ok(())
}

/// The map grows in steps of this, and starts at it at least. LMDB needs a multiple of the OS page size, which this is
pub const MAP_STEP: usize = 64 << 20;

/// Largest multiple of `MAP_STEP` that fits in `usize`
const MAX_MAP_SIZE: usize = (usize::MAX / MAP_STEP) * MAP_STEP;

/// Rounds `bytes` up to a multiple of `MAP_STEP`, or `None` if that would overflow `usize`
fn round_up_map_step(bytes: usize) -> Option<usize> {
    bytes.div_ceil(MAP_STEP).checked_mul(MAP_STEP)
}

/// The next map size after `current`, doubling and rounding up to `MAP_STEP`, or `None` if it cannot grow further
fn next_map_size(current: usize) -> Option<usize> {
    let doubled = current.checked_mul(2)?;
    round_up_map_step(doubled.max(MAP_STEP))
}

/// The largest map a unit test's database grows to. No test needs near it, so a change that keeps a database growing
/// fails the test at it, as `MAP_FULL`, rather than filling the disk
#[cfg(test)]
const TEST_MAP_LIMIT: usize = 1 << 30;

/// A unit's place, reserved while the unit is begun: let go when dropped, which a unit that began forgets it for
struct Reserved<'a>(&'a Store);

impl Drop for Reserved<'_> {
    fn drop(&mut self) {
        self.0.end_unit();
    }
}

/// An open write transaction held together with its writer lock and map-growth gate.
///
/// Field order is drop order: `txn` drops before `_writer` and `_gate`, so on normal drop or panic unwind LMDB's
/// transaction ends before the writer lock and map read lock are released.
pub(crate) struct WriteTxn<'a, G = ArcRwLockReadGuard<RawRwLockRecursive, ()>> {
    // SAFETY: `txn` MUST be declared first so Rust drops it before `_writer` (which releases the writer turn and
    // macOS `writer.lock`, preventing double-writer UB) and before `_gate` (which releases the map read lock,
    // preventing `env.resize` from unmapping under a live transaction).
    pub(crate) txn: RwTxn<'a>,
    _writer: writers::WriterLock,
    _gate: G,
}

impl<'a, G> WriteTxn<'a, G> {
    fn new(txn: RwTxn<'a>, writer: writers::WriterLock, gate: G) -> Self {
        Self { txn, _writer: writer, _gate: gate }
    }

    /// Commits the transaction through `store.commit_txn`, keeping `_writer` and `_gate` alive across commit
    /// accounting, then letting `_gate` go and `_writer` after it.
    pub(crate) fn commit(self, store: &Store) -> Result<u64> {
        self.commit_then(store, || ())
    }

    /// `commit`, running `landed` once the commit has landed and before the writer is let go, so every later writer's
    /// commit sees what it did. The gate is let go first: the transaction has ended, so `landed` reads nothing through
    /// the map, and it may wait on a lock a thread holds while it reads, whose read can wait on a growth another process
    /// made that the gate would keep this one from taking (the module's rule)
    pub(crate) fn commit_then(self, store: &Store, landed: impl FnOnce()) -> Result<u64> {
        let Self { _gate, _writer, txn } = self;
        let result = store.commit_txn(txn);
        drop(_gate);
        if result.is_ok() {
            landed();
        }
        drop(_writer);
        result
    }

    /// Commits the raw LMDB transaction through `commits::commit` without bumping sequence, keeping `_writer` and
    /// `_gate` alive across the commit and dropping them afterwards.
    fn commit_raw(self, kept: Option<&Kept>) -> Result<()> {
        let Self { _gate, _writer, txn } = self;
        let result = commits::commit(txn, kept);
        drop(_writer);
        drop(_gate);
        result
    }

    /// Aborts the transaction, dropping `txn` first, then `_writer`, then `_gate`.
    #[cfg(test)]
    pub(crate) fn abort(self) {
        drop(self);
    }

    /// Widens the transaction's lifetime to `'static` for `Unit`, preserving the drop-order guard.
    ///
    /// # Safety
    ///
    /// The caller must ensure the `Store` (and its `Env`) that `txn` borrows outlives the returned `WriteTxn`.
    pub(crate) unsafe fn into_static(self) -> WriteTxn<'static, G> {
        let Self { txn, _writer, _gate } = self;
        // SAFETY: upheld by the caller's contract that the `Store`/`Env` outlives the returned `WriteTxn`.
        let txn = unsafe { std::mem::transmute::<RwTxn<'a>, RwTxn<'static>>(txn) };
        WriteTxn { txn, _writer, _gate }
    }
}

/// What a write's claim of the write transaction came to
enum Claimed<'a> {
    Held(WriteTxn<'a, RecursiveRwLockReadGuard<'a, ()>>),
    /// Another process grew the file past this process's map
    MapResized,
    /// The pin the write made sure of was ended, by a growth, before the transaction began
    Unpinned,
}

/// How long a growth beside units tries for the gate before it looks again whether a unit has opened
const GROWTH_GATE_TRY: Duration = Duration::from_millis(5);

/// Takes every open of this process from now on for one of another boot than any record's, as after a restart: for a
/// test whose writer was another process of this boot
#[cfg(test)]
pub(crate) fn forget_the_boot() {
    boot::NO_BOOT.store(true, Ordering::Relaxed);
}

/// The record held by another thread for a test (`Store::hold_record`), let go as this drops
#[cfg(test)]
pub(crate) struct HeldRecord {
    release: Option<std::sync::mpsc::Sender<()>>,
    holder: Option<std::thread::JoinHandle<()>>,
}

#[cfg(test)]
impl Drop for HeldRecord {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(holder) = self.holder.take() {
            let _ = holder.join();
        }
    }
}

/// How many times one write grows the map before giving up with `MAP_FULL`
const MAX_GROWTHS: usize = 16;

/// The map's size for a data file of `file_bytes`: twice the file, so a database can double before its first growth,
/// and a step at least. This is writable mapping capacity, independent of the file's current physical length.
pub fn initial_map_size(file_bytes: u64) -> usize {
    let wanted = usize::try_from(file_bytes).unwrap_or(usize::MAX).saturating_mul(2).max(MAP_STEP);
    round_up_map_step(wanted).unwrap_or(MAX_MAP_SIZE)
}

// Windows read-only sections map only the current file, not a writer's reserved capacity. The logical bound
// must not exceed that view: transaction renewal then reports MAP_RESIZED before accessing newly appended pages. The view
// ends at the file's last whole page: heed refuses a map that isn't whole pages, and a torn tail page past it is one the
// open's walk of the trees judges (`short_of_its_pages`), as it does on every other platform
#[cfg(any(windows, test))]
pub(crate) fn read_only_map_size(file_bytes: u64, requested: Option<usize>) -> usize {
    /// The page every Windows target has, which heed requires the map to be a multiple of
    const WINDOWS_PAGE: usize = 4096;
    let physical = usize::try_from(file_bytes).unwrap_or(usize::MAX);
    let whole = physical - physical % WINDOWS_PAGE;
    requested.unwrap_or(whole).min(whole)
}

/// A transaction that committed, with its sync outcome kept apart until in-memory state and observers are finalized.
pub(crate) struct Commit {
    pub sequence: u64,
    pub synced: Result<()>,
}

impl Commit {
    pub fn finish(self) -> Result<()> {
        self.synced.map_err(|error| {
            Error::new(
                ErrorCode::NotDurable,
                format!(
                    "The operation committed (sequence {}), but its sync failed: {}. Do not retry the operation",
                    self.sequence, error.message
                ),
            )
        })
    }
}

/// What became of the drains computed outside the write lock, for a test
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RankPlans {
    pub(crate) applied: u64,
    pub(crate) redone: u64,
    pub(crate) retried: u64,
    pub(crate) one_at_a_time: u64,
    pub(crate) every_node: u64,
}

pub struct Store {
    path: PathBuf,
    read_only: bool,
    /// Each commit is synced as it is made, with no group commit (`StoreOptions::sync_each_commit`)
    sync_each_commit: bool,
    env: Env<WithoutTls>,
    db: Db,
    gate: Arc<RecursiveRwLock<()>>,
    commit: Option<GroupCommit>,
    /// Whether a unit is open, holding the environment's one write transaction
    unit: UnitSlot,
    /// Read transactions a read finished with, for the next read to take while it is still current
    idle: IdleReads,
    /// Read transactions held from a call until a read asked at it runs on another thread (`call_reads`)
    calls: Arc<CallReads<RoTxn<'static, WithoutTls>>>,
    /// The last commit's sequence (`meta::SEQUENCE`), which every commit raises by one
    sequence: AtomicU64,
    /// The stored epoch this handle has seen. A commit under any other is refused, since what it read has been replaced
    epoch: AtomicU64,
    /// The epoch the transaction about to commit moves to, when it replaces every row: taken by that commit, whatever
    /// comes of it, so a replacement that failed can't vouch for a later one
    replacing: AtomicU64,
    /// Tells other processes of commits here, and this one of theirs
    pub(crate) wake: wake::Wake,
    /// What a writer takes before LMDB's write lock: its turn among this process's writers, kept until its commit is
    /// accounted for, and on macOS a file lock, so a write lock a dead writer left held is let go
    writers: writers::Writers,
    /// What rank drains read of the graph, kept between them by the commits that change it (`engine::mirror`)
    mirror: Arc<PlainMutex<EdgeMirror>>,
    /// Run once on the builder's thread between computing a drain and writing it, for a test's commit there
    #[cfg(test)]
    rank_planned: PlainMutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run once on the builder's thread between a drain's check for staleness and its taking the write lock
    #[cfg(test)]
    rank_checked: PlainMutex<Option<Box<dyn FnOnce() + Send>>>,
    /// What became of the drains computed outside the write lock
    #[cfg(test)]
    pub(crate) rank_plans: PlainMutex<RankPlans>,
    /// The edge entries the write being committed filed and unfiled, handed by the engine before it commits; none from a
    /// write outside the engine's units, whose commit then empties the mirror
    deposited: PlainMutex<Option<Vec<EdgeDelta>>>,
    /// The entities the write about to commit wrote, which its commit stamps with its sequence
    touched: PlainMutex<Option<crate::engine::changed::Touched>>,
    /// How far this handle has accounted for every commit, and whether one since it last answered was another's
    accounted: PlainMutex<Accounted>,
    /// The grouped mode's pin and record (`kept`), in a writable store of that mode
    kept: Option<Arc<Kept>>,
    /// The notice of a restore made at this open or an earlier one, which no caller has said it read
    /// (`docs/roadmap/synced-commit.md`, The design)
    restored: PlainMutex<Option<record::Restored>>,
    /// The record beside the data file named another file than this one, which was opened as it is; reported with
    /// the notice
    not_borne_out: bool,
    /// The locks that say this process has the database open, and in which mode a writable one writes it, held shared
    /// until the environment has closed: an open that finds the first unheld may restore the file, and one of another
    /// mode is refused while the second is held.
    presence: Option<restore::Presence>,
    /// This process's `open_paths()` registration. Declared LAST so Rust drops it only after `env`, `kept`, and
    /// `presence` have all dropped, preventing a same-process LMDB double-open race.
    _registration: Option<Registration>,
}

/// A read `Store::ask` keeps waiting, for `Store::run_asked` on any thread
pub(crate) struct CallRead(call_reads::Ticket<RoTxn<'static, WithoutTls>>);

impl AtCommit for RoTxn<'static, WithoutTls> {
    fn read_at(&self) -> u64 {
        self.id() as u64
    }
}

/// Every commit up to `through` is one this handle made or has noticed; `elsewhere` says another's was among them since
/// `committed_elsewhere` last answered. Every commit raises the sequence by one and every commit here passes through
/// `commit_txn`, so a sequence between `through` and the next commit here is another handle's
#[derive(Default)]
struct Accounted {
    through: u64,
    elsewhere: bool,
}

/// The most idle read transactions kept: one for each thread that reads at once, the caller's and the read pool's
const MAX_IDLE: usize = 8;

/// The environments open in this process, by canonical path. LMDB's locks are per process, so an environment opened
/// twice in one process shares no locks with itself and can corrupt its own file.
fn open_paths() -> &'static Mutex<HashSet<PathBuf>> {
    static OPEN: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    OPEN.get_or_init(Default::default)
}

/// Releases a path's registration unless the open that took it succeeded
pub(crate) struct Registration(Option<PathBuf>);

/// Reserves a canonical path in this process until the registration is dropped, refused with `refused`'s message when
/// an open, a publisher or a reader holds it
fn reserve(canonical: &Path, refused: impl FnOnce() -> String) -> Result<Registration> {
    if !open_paths().lock().expect("the open-paths lock is never poisoned").insert(canonical.to_owned()) {
        return Err(Error::new(ErrorCode::AlreadyOpen, refused()));
    }
    Ok(Registration(Some(canonical.to_owned())))
}

/// Reserve an existing canonical destination against opens and publishers in this process.
pub(crate) fn reserve_destination(target: &Path) -> Result<Registration> {
    reserve(target, || {
        format!("The database at {} is open or being published; use another directory", target.display())
    })
}

/// Reserves the database in `source`, which a reader opens itself, against every other environment of it in this
/// process while it is read: refused when it is open here, the reading database's own directory included
pub(crate) fn reserve_source(source: &Path) -> Result<Registration> {
    reserve(&source.canonicalize()?, || {
        format!(
            "The database at {} is open in this process, so it can't be read as a source; back it up and use the backup",
            source.display()
        )
    })
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            open_paths().lock().expect("the open-paths lock is never poisoned").remove(&path);
        }
    }
}

/// Retry reader acquisition once after LMDB removes slots whose owning processes are dead.
/// Live readers are never evicted; all other errors, including a second READERS_FULL, remain refusals.
fn with_reader_recovery<T>(env: &Env<WithoutTls>, mut begin: impl FnMut() -> heed::Result<T>) -> heed::Result<T> {
    match begin() {
        Err(heed::Error::Mdb(heed::MdbError::ReadersFull)) => {
            env.clear_stale_readers()?;
            begin()
        }
        result => result,
    }
}

pub(crate) fn read_txn(env: &Env<WithoutTls>) -> heed::Result<RoTxn<'_, WithoutTls>> {
    with_reader_recovery(env, || env.read_txn())
}

/// What an open of the database in `dir` decides before it reads it, for a reader that opens it itself: a restore made
/// where the record asks for one, and the lock that says it is open, held until what this answers is dropped
pub(crate) fn decide_for_reading(dir: &Path) -> Result<impl Sized> {
    let decided = restore::decide(&dir.canonicalize()?, None, boot::this_boot())?;
    Ok(decided.presence)
}

// LMDB also silently omits its lock table after EROFS, even without NO_LOCK. Beginning a reader first makes
// mti_numreaders nonzero for every registered environment; a missing lock table always reports zero.
pub(crate) fn require_registered_reader(env: &Env<WithoutTls>, path: &Path) -> Result<()> {
    let opening = Opening { env, dir: path, read_only: true, kept: None };
    registered(env, path, &opening.read()?)
}

/// An opened read-only database handle and snapshot transaction for `replace_with`
pub(crate) struct SourceSnapshot {
    pub(crate) db: heed::Database<'static, Bytes, Bytes>,
    pub(crate) txn: RoTxn<'static, WithoutTls>,
}

/// Whether the lock table registers `_reader`, a read transaction of `env` held while it is asked
fn registered(env: &Env<WithoutTls>, path: &Path, _reader: &RoTxn<'_, WithoutTls>) -> Result<()> {
    if env.info().number_of_readers == 0 {
        return Err(Error::new(
            ErrorCode::Io,
            format!(
                "Cannot open {} with LMDB reader registration: no lock table is mapped. Grant write access to lock.mdb on a writable filesystem, or use a writable copy",
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Opens a read-only source LMDB environment for `replace_with`.
///
/// The caller must hold `reserve_source(source)` and `decide_for_reading(source)` for the lifetime of the returned
/// environment and verify reader registration with `require_registered_reader` before reading from it.
pub(crate) fn open_source_env(source: &Path) -> Result<Env<WithoutTls>> {
    let canonical = source.canonicalize()?;
    let file_bytes = std::fs::metadata(canonical.join("data.mdb"))?.len();
    let mut options = EnvOpenOptions::new().read_txn_without_tls();
    options.max_dbs(8);
    #[cfg(windows)]
    options.map_size(read_only_map_size(file_bytes, None));
    #[cfg(not(windows))]
    options.map_size(initial_map_size(file_bytes));
    // SAFETY: `READ_ONLY` opens the environment without modifying it.
    unsafe { options.flags(EnvFlags::READ_ONLY) };
    // SAFETY: the caller holds `reserve_source(source)`, guaranteeing no other environment in this process has
    // these files open.
    Ok(unsafe { options.open(&canonical) }?)
}

/// Verifies torn-file integrity (`short_of_its_pages`), adopts any concurrent map growth (`MapResized`), validates the
/// format version, and returns an immutable snapshot transaction on an opened source environment.
pub(crate) fn open_source_snapshot(env: &Env<WithoutTls>, source: &Path) -> Result<SourceSnapshot> {
    let canonical = source.canonicalize()?;
    let data = canonical.join("data.mdb");
    let mut file = std::fs::File::open(&data)?;
    let page = page_size_of(&mut file, &data)?;
    let opening = Opening { env, dir: &canonical, read_only: true, kept: None };
    short_of_its_pages(&opening, &mut file, page, &data)?;
    let rtxn = opening.read()?;
    let from = env
        .open_databases_and_commit(rtxn, |opener, rtxn| -> heed::Result<_> {
            Ok(CommitOrAbort::<_, ()>::Commit(opener.open_database::<Bytes, Bytes>(rtxn, Some(format::DB_NAME))?))
        })?
        .unwrap_commit()
        .ok_or_else(|| Error::new(ErrorCode::NotADatabase, format!("{} holds no elmdb database", source.display())))?;
    let rtxn = opening.begin(|| with_reader_recovery(env, || env.clone().static_read_txn()))?;
    let stamped = from.get(&rtxn, &meta_key(meta::FORMAT))?.map(<[u8]>::to_vec);
    if stamped.as_deref() != Some(&format::FORMAT_VERSION.to_be_bytes()[..]) {
        return Err(Error::new(
            ErrorCode::WrongFormat,
            format!("{} isn't a database in format {}", source.display(), format::FORMAT_VERSION),
        ));
    }
    Ok(SourceSnapshot { db: from, txn: rtxn })
}

/// Syncs what an open committed, before the open goes on: a new environment's stamp, and the graphs' databases
fn opened_synced(env: &Env<WithoutTls>) -> Result<()> {
    env.force_sync()?;
    run_point!(OPEN_SYNCED);
    Ok(())
}

impl Store {
    pub fn open(path: &Path, options: StoreOptions) -> Result<Self> {
        let data = path.join("data.mdb");
        if options.read_only {
            if !data.is_file() {
                return Err(Error::new(ErrorCode::NoDatabase, format!("No database at {}", path.display())));
            }
        } else {
            std::fs::create_dir_all(path)?;
        }
        let canonical = path.canonicalize()?;
        if !open_paths().lock().expect("the open-paths lock is never poisoned").insert(canonical.clone()) {
            return Err(Error::new(
                ErrorCode::AlreadyOpen,
                format!("The database at {} is already open in this process; use the open one", path.display()),
            ));
        }
        let registration = Registration(Some(canonical.clone()));

        // Before LMDB opens the file: the record says whether it opens as it is or at the last synced commit, which a
        // restore takes it to (`restore.rs`)
        let mode = (!options.read_only).then_some(match (options.sync_each_commit, options.syncs) {
            (true, _) => Mode::Ordered,
            (false, true) => Mode::Grouped,
            (false, false) => Mode::Unsynced,
        });
        let boot = boot::this_boot();
        let restore::Decided { presence, record, remembered, restored, not_borne_out } =
            restore::decide(&canonical, mode, boot)?;

        let file_bytes = std::fs::metadata(&data).map(|m| m.len()).unwrap_or(0);
        let open = |flags: EnvFlags| {
            let mut builder = EnvOpenOptions::new().read_txn_without_tls();
            let map_size = options.map_size_bytes.unwrap_or_else(|| initial_map_size(file_bytes));
            #[cfg(windows)]
            let map_size =
                if options.read_only { read_only_map_size(file_bytes, options.map_size_bytes) } else { map_size };
            builder.max_dbs(8).map_size(map_size);
            // SAFETY: the flags say when LMDB syncs, and nothing else about its files: with NO_SYNC and NO_META_SYNC off,
            // LMDB syncs a commit's pages before writing the meta page that names them, which is what the sync-each-commit
            // mode relies on, and with NO_SYNC on, group commit's syncs are what flush a commit (`committed`). A read-only
            // environment is never written, so its flags are none of these
            unsafe {
                builder.flags(flags);
            }
            // SAFETY: `open_paths` guarantees this process holds no other handle to the environment, and its files are
            // LMDB's, written only through LMDB
            unsafe { builder.open(&canonical) }
        };
        let env =
            open(match (options.read_only, options.sync_each_commit) {
                (true, _) => EnvFlags::READ_ONLY,
                // LMDB's own order: a commit's pages, a sync, then the meta page that names them, synced as written
                (false, true) => EnvFlags::empty(),
                (false, false) => EnvFlags::NO_SYNC,
            })
            .map_err(|e| match e {
                heed::Error::Io(ref io) if options.read_only && io.kind() == std::io::ErrorKind::PermissionDenied => {
                    Error::new(
                        ErrorCode::Io,
                        format!(
                            "Cannot open {} with LMDB reader registration: {e}. Grant read access to data.mdb and write access to lock.mdb, or use a writable copy",
                            canonical.display()
                        ),
                    )
                }
                heed::Error::Mdb(heed::MdbError::Invalid | heed::MdbError::VersionMismatch) => Error::new(
                    ErrorCode::NotADatabase,
                    format!(
                        "{} isn't a file this LMDB reads: {e}. Files lmdb-js wrote are in its own LMDB data version, \
                     which elmdb doesn't read; re-create the database with elmdb",
                        data.display()
                    ),
                ),
                e => e.into(),
            })?;

        // A read of a page past the end of the file is a SIGBUS (on Windows, an access violation) that ends the process, so
        // a file shorter than its meta page's pages is read first, by seeks, to see whether the trees reach any of them. LMDB
        // may leave its last free pages out of the file, and then the file is healthy: a writable open extends it with zeros,
        // as LMDB's write map would, and a read-only open goes on, since it touches no page the trees don't reach. A page the
        // trees reach past the end, or that fails the walk's checks of a page, is a torn file, refused whichever way it is opened
        let mut file = std::fs::OpenOptions::new().read(true).write(!options.read_only).open(&data)?;
        let page = page_size_of(&mut file, &data)?;
        run_point!(OPEN_ENV);
        let opening = Opening { env: &env, dir: &canonical, read_only: options.read_only, kept: None };
        if options.read_only {
            // Before the walk, which holds a snapshot by a read transaction: one no lock table registers holds none
            registered(&env, &canonical, &opening.read()?)?;
        }
        let short = short_of_its_pages(&opening, &mut file, page, &data)?;
        run_point!(OPEN_WALKED);
        let writers = writers::Writers::open(&canonical, options.read_only)?;
        if short && !options.read_only {
            extend_to_its_pages(&opening, &writers, &file, page)?;
        }
        let gate = Arc::new(RecursiveRwLock::new(()));
        let idle = IdleReads::new()?;
        let calls = Arc::new(CallReads::default());
        let kept = match (mode, record) {
            (Some(Mode::Grouped), Some(record)) => Some(Arc::new(Kept::new(
                env.clone(),
                Arc::clone(&gate),
                idle.clearer(),
                Arc::clone(&calls),
                &canonical,
                page,
                boot,
                record,
                remembered,
            )?)),
            _ => None,
        };
        let opening = Opening { kept: kept.as_ref(), ..opening };
        let db = Self::open_db(&opening, &writers)?;
        // The open's record: its newest commit synced and named before the first write of a unit
        if let Some(kept) = &kept
            && kept.settle(&|| opening.adopt())?
        {
            run_point!(OPEN_SYNCED);
        }
        // Read before group commit's thread is started, the last of the open's transactions: none is begun, and no
        // map taken at a new size, once another thread holds the environment
        let sequence = {
            let txn = opening.read()?;
            let held = db.get(&txn, &meta_key(meta::SEQUENCE))?;
            held.and_then(|bytes| <[u8; 8]>::try_from(bytes).ok()).map_or(0, u64::from_be_bytes)
        };
        let epoch = {
            let txn = opening.read()?;
            stored_epoch(&txn, db)?
        };
        let commit = kept
            .as_ref()
            .map(|kept| {
                GroupCommit::start(
                    Policy::new(options.sync_idle, options.sync_interval),
                    Box::new(EnvSyncer { kept: Arc::clone(kept) }),
                    Box::new(SystemClock),
                )
            })
            .transpose()?;
        let wake = wake::Wake::new(&canonical);
        let accounted = PlainMutex::<Accounted>::default();
        accounted.lock().through = sequence;
        let store = Self {
            path: canonical,
            read_only: options.read_only,
            // A read-only store makes no commit, so the mode is none of its concern
            sync_each_commit: options.sync_each_commit,
            env,
            db,
            gate,
            commit,
            unit: UnitSlot::new(),
            idle,
            calls,
            sequence: AtomicU64::new(sequence),
            epoch: AtomicU64::new(epoch),
            replacing: AtomicU64::new(0),
            wake,
            writers,
            mirror: Arc::default(),
            #[cfg(test)]
            rank_planned: PlainMutex::default(),
            #[cfg(test)]
            rank_checked: PlainMutex::default(),
            #[cfg(test)]
            rank_plans: PlainMutex::default(),
            deposited: PlainMutex::default(),
            touched: PlainMutex::default(),
            accounted,
            kept,
            restored: PlainMutex::new(restored),
            not_borne_out,
            presence: Some(presence),
            _registration: Some(registration),
        };
        store.note_the_restore()?;
        Ok(store)
    }

    /// A restore's notice that names the LMDB commit it took the file to, that commit being the file's newest still:
    /// made to name the sequence the database opened at, before any write. A read-only open on a filesystem it can't
    /// write leaves that to the next open
    fn note_the_restore(&self) -> Result<()> {
        let Some(record::Restored::Commit(commit)) = *self.restored.lock() else { return Ok(()) };
        run_point!(RESTORE_NOTING at &self.path);
        if self.env.info().last_txn_id as u64 != commit {
            // Another process opened the database since, and may have noted the sequence before it wrote: the
            // record's notice is the one to report
            if let Ok(noticed) = restore::notice(&self.path) {
                *self.restored.lock() = noticed;
            }
            return Ok(());
        }
        match restore::note(&self.path, commit, self.sequence()) {
            Ok(noted) => *self.restored.lock() = noted,
            Err(_) if self.read_only => {}
            Err(error) => return Err(error),
        }
        Ok(())
    }

    /// The notice of a restore made at this open or an earlier one, until a caller says it read it: the sequence the
    /// database opened at, where the open that made it could read it
    pub fn restored(&self) -> Option<crate::database::Restore> {
        self.restored.lock().map(|restored| crate::database::Restore {
            sequence: match restored {
                record::Restored::Sequence(sequence) => Some(sequence),
                record::Restored::Commit(_) => None,
            },
        })
    }

    /// Says the notice of a restore was read: the record is written again without it, and no open reports it after
    pub fn acknowledge_restore(&self) -> Result<()> {
        self.check_writable()?;
        if self.restored.lock().is_some() {
            restore::acknowledge(&self.path)?;
            *self.restored.lock() = None;
        }
        Ok(())
    }

    /// The environment's elmdb database, checked to be in this version's format; a new environment's is created and
    /// stamped first
    fn open_db(opening: &Opening<'_>, writers: &writers::Writers) -> Result<Db> {
        let Opening { env, dir: path, read_only, .. } = *opening;
        let rtxn = opening.read()?;
        let (db, main) = env
            .open_databases_and_commit(rtxn, |opener, rtxn| -> heed::Result<_> {
                let elmdb = opener.open_database::<Bytes, Bytes>(rtxn, Some(format::DB_NAME))?;
                let main = opener.open_database::<Bytes, Bytes>(rtxn, None)?;
                Ok(CommitOrAbort::<_, ()>::Commit((elmdb, main)))
            })?
            .unwrap_commit();
        // The unnamed database lists the named ones: anything in it but elmdb's own is another program's data
        let foreign = match main {
            Some(main) => {
                let txn = opening.read()?;
                main.iter(&txn)?.any(|e| {
                    e.is_ok_and(|(k, _)| {
                        ![format::DB_NAME, format::COSINE_GRAPHS, format::EUCLIDEAN_GRAPHS]
                            .iter()
                            .any(|n| k == n.as_bytes())
                    })
                })
            }
            None => false,
        };
        if let Some(db) = db {
            check_stamp(opening, db)?;
            return Self::with_graphs(opening, db, writers);
        }
        if read_only || foreign {
            return Err(Error::new(
                ErrorCode::NotADatabase,
                format!(
                    "{} holds no elmdb database{}",
                    path.display(),
                    if foreign { ", and data of another program" } else { "" }
                ),
            ));
        }
        // A new environment: its database and stamp in one unit, synced, before anything else is written
        let writer = writers.hold()?;
        let (wtxn, writing) = opening.write()?;
        let kept = opening.kept.map(|kept| &**kept);
        let db = commits::create_databases(env, wtxn, kept, |opener, wtxn| -> Result<_> {
            let db = opener.create_database::<Bytes, Bytes>(wtxn, Some(format::DB_NAME))?;
            // A test's stand-in for a disk that fills while a new environment is stamped
            failure_point!(STAMP at path, Error::new(ErrorCode::Io, "No space left on device"));
            put_meta(wtxn, db, meta::KIND, format::KIND.as_bytes())?;
            put_meta(wtxn, db, meta::FORMAT, &format::FORMAT_VERSION.to_be_bytes())?;
            Ok(CommitOrAbort::<_, ()>::Commit(db))
        })
        .map_err(|e: Error| {
            Error::new(e.code, format!("Couldn't record the storage format of {}: {}", path.display(), e.message))
        })?
        .unwrap_commit();
        // Let go before the graphs' transaction, which takes it again
        drop(writing);
        writer.release();
        opened_synced(env)?;
        Self::with_graphs(opening, db, writers)
    }

    /// The databases around `main`: hannoy's graph databases, created in a writable environment that lacks them, and
    /// opened, when there, in a read-only one
    fn with_graphs(
        opening: &Opening<'_>,
        main: heed::Database<'static, Bytes, Bytes>,
        writers: &writers::Writers,
    ) -> Result<Db> {
        let Opening { env, read_only, .. } = *opening;
        let rtxn = opening.read()?;
        let (cosine, euclidean) = env
            .open_databases_and_commit(rtxn, |opener, rtxn| -> heed::Result<_> {
                let cosine = opener.open_database(rtxn, Some(format::COSINE_GRAPHS))?;
                let euclidean = opener.open_database(rtxn, Some(format::EUCLIDEAN_GRAPHS))?;
                Ok(CommitOrAbort::<_, ()>::Commit((cosine, euclidean)))
            })?
            .unwrap_commit();
        if read_only || (cosine.is_some() && euclidean.is_some()) {
            return Ok(Db { main, cosine, euclidean });
        }
        let writer = writers.hold()?;
        let (wtxn, writing) = opening.write()?;
        let kept = opening.kept.map(|kept| &**kept);
        let (cosine, euclidean) = commits::create_databases(env, wtxn, kept, |opener, wtxn| -> Result<_> {
            let cosine = opener.create_database(wtxn, Some(format::COSINE_GRAPHS))?;
            let euclidean = opener.create_database(wtxn, Some(format::EUCLIDEAN_GRAPHS))?;
            Ok(CommitOrAbort::<_, ()>::Commit((cosine, euclidean)))
        })?
        .unwrap_commit();
        // The open's last commit, and synced as its first is: group commit, which syncs a unit's, isn't started yet
        // and hears of neither, so left unsynced it would stay so until some unit's commit brought a sync. The grouped
        // mode's open syncs it as it writes its record (`Kept::settle`)
        drop(writing);
        writer.release();
        if opening.kept.is_none() {
            opened_synced(env)?;
        }
        Ok(Db { main, cosine: Some(cosine), euclidean: Some(euclidean) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The gate held for a transaction
    fn gate(&self) -> RecursiveRwLockReadGuard<'_, ()> {
        self.gate.read()
    }

    /// Runs `f` in a read transaction, a snapshot of the last commit. An idle one is taken while it still is that
    /// snapshot — its id the environment's last committed one, which a commit in any process moves — and opening one
    /// is what a read saves. Completed snapshots expire after 100 ms idle even without further database calls.
    pub fn read<T>(&self, f: impl FnOnce(&RoTxn<'_, WithoutTls>, Db) -> Result<T>) -> Result<T> {
        self.read_on(|txn| f(txn, self.db))?
    }

    /// `read`'s snapshot, handed to `f` as the transaction it is
    fn read_on<R>(&self, f: impl FnOnce(&RoTxn<'static, WithoutTls>) -> R) -> Result<R> {
        loop {
            let guard = self.gate();
            let txn = match self.snapshot() {
                Ok(txn) => txn,
                Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                    drop(guard);
                    run_point!(READ_MAP_GROWN at &self.path);
                    self.adopt_map_size()?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let (txn, generation) = txn;
            let result = f(&txn);
            self.idle.put(txn, generation);
            drop(guard);
            return Ok(result);
        }
    }

    /// A read transaction at the last commit, idle or opened, with the idle cache's generation it belongs to; the
    /// caller holds the gate
    fn snapshot(&self) -> heed::Result<(RoTxn<'static, WithoutTls>, u64)> {
        let (cached, generation) = self.idle.take(&self.env);
        let txn = match cached {
            Some(txn) => txn,
            None => with_reader_recovery(&self.env, || self.env.clone().static_read_txn())?,
        };
        Ok((txn, generation))
    }

    /// Asks for `read` to run on the snapshot of the last commit as it is at this call, on whichever thread runs what
    /// this answers, or here when as many transactions are held for such reads as may be (`call_reads`). `None` says
    /// it has run
    pub(crate) fn ask(
        &self,
        read: impl FnOnce(&RoTxn<'static, WithoutTls>, Db) + Send + 'static,
    ) -> Result<Option<CallRead>> {
        let db = self.db;
        let mut work: call_reads::Work<RoTxn<'static, WithoutTls>> = Box::new(move |txn| read(txn, db));
        loop {
            let guard = self.gate();
            let last = self.env.info().last_txn_id as u64;
            // Another process grew the map: taken at its size, with the gate held for writing, and asked again
            let resized = std::cell::Cell::new(false);
            let open = || {
                self.snapshot().map_err(|error| {
                    resized.set(matches!(error, heed::Error::Mdb(heed::MdbError::MapResized)));
                    Error::from(error)
                })
            };
            match self.calls.ask(last, open, work) {
                Ok(call_reads::Asked::Waiting(ticket)) => return Ok(Some(CallRead(ticket))),
                Ok(call_reads::Asked::Here(here)) => {
                    drop(guard);
                    self.read_on(here)?;
                    return Ok(None);
                }
                Err((_, back)) if resized.get() => {
                    drop(guard);
                    self.adopt_map_size()?;
                    work = back;
                }
                Err((error, _)) => return Err(error),
            }
        }
    }

    /// Runs a read `ask` kept waiting, unless a growth of the map ran it first. Refused `CLOSED` when the store closed
    /// first, and `DATABASE_MISMATCH` when another store asked it: its transaction is that store's, which this one's
    /// gate keeps no growth from, nor would its idle cache hold it. Refused, it is let go of, and its transactions with it
    pub(crate) fn run_asked(&self, read: CallRead) -> Result<()> {
        if !read.0.of(&self.calls) {
            return Err(Error::new(
                ErrorCode::DatabaseMismatch,
                "A read asked of one database was answered through another: answer it through the database that asked it",
            ));
        }
        read.0.run(
            || self.gate(),
            |freed| {
                for (txn, generation) in freed {
                    self.idle.put(txn, generation);
                }
            },
        )
    }

    /// How many transactions are held for reads asked at calls, and the most any one commit holds
    #[cfg(test)]
    pub(crate) fn held_for_calls(&self) -> (usize, usize) {
        self.calls.held()
    }

    /// Read transactions holding every reader slot left, the idle ones let go of first, for a test of a full table
    #[cfg(test)]
    pub(crate) fn every_reader_slot(&self) -> Vec<RoTxn<'static, WithoutTls>> {
        self.drop_idle();
        let mut held = Vec::new();
        while let Ok(txn) = self.env.clone().static_read_txn() {
            held.push(txn);
        }
        held
    }

    /// Drops every idle read transaction: after a commit here, which makes them stale, and before the map is resized
    /// or the environment closed, which no open transaction may outlast
    pub(crate) fn drop_idle(&self) {
        self.idle.clear();
    }

    /// `write_committed` with its commit finished, for a test that writes to the store under the engine
    #[cfg(test)]
    pub fn write<T>(&self, f: impl FnMut(&mut RwTxn<'_>, Db) -> Result<T>, durable: bool) -> Result<T> {
        let (t, commit) = self.write_committed(f, durable)?;
        commit.finish()?;
        Ok(t)
    }

    /// Runs `f` in a write transaction and commits it, or aborts it when `f` fails, answering the commit it made, a
    /// durable write's failed sync apart from the commit, which happened. A write that fills the map is aborted, the
    /// map grown and `f` run again, so `f` must do nothing but write through its transaction
    pub(crate) fn write_committed<T>(
        &self,
        f: impl FnMut(&mut RwTxn<'_>, Db) -> Result<T>,
        durable: bool,
    ) -> Result<(T, Commit)> {
        let op = Op::write(durable);
        let result = self.write_reported(f, durable, &op);
        match &result {
            Ok((_, commit)) => op.end(Ok(match (&commit.synced, durable) {
                (Err(_), _) => "not_durable",
                (Ok(()), true) => "durable",
                (Ok(()), false) => "committed",
            })),
            Err(error) => op.end(Err(error)),
        }
        result
    }

    /// `write_committed`'s work, recording on `op` what it waited for and retried
    fn write_reported<T>(
        &self,
        mut f: impl FnMut(&mut RwTxn<'_>, Db) -> Result<T>,
        durable: bool,
        op: &Op,
    ) -> Result<(T, Commit)> {
        self.check_writable()?;
        if self.unit_is_open() {
            return Err(Error::new(
                ErrorCode::UnitOpen,
                "A unit is open, holding the database's one write transaction: write through it, or commit it first",
            ));
        }
        let mut growths = 0;
        loop {
            run_point!(WRITE_RESERVATION);
            self.hold_to_the_bound()?;
            // Before the write lock, and before the transaction, which may write pages before it commits
            let writing = self.ensure_pin()?;
            run_point!(WRITE_PINNED);
            // Claimed under the unit's lock: a unit that slipped between the earlier check and write_txn would leave
            // this caller waiting on the unit's writer while it holds a schema guard
            let claimed = self.unit.claim_writer(|| {
                let guard = self.gate();
                let writer = self.writers.hold()?.writing(writing);
                match self.env.write_txn() {
                    Ok(txn) if !self.pinned() => {
                        txn.abort();
                        Ok(Claimed::Unpinned)
                    }
                    Ok(txn) => Ok(Claimed::Held(WriteTxn::new(txn, writer, guard))),
                    Err(heed::Error::Mdb(heed::MdbError::MapResized)) => Ok(Claimed::MapResized),
                    Err(e) => Err(Error::from(e)),
                }
            })??;
            let mut held = match claimed {
                Claimed::Held(held) => held,
                Claimed::MapResized => {
                    self.adopt_map_size()?;
                    continue;
                }
                // A growth ended the pin between its making sure of it and the transaction's beginning
                Claimed::Unpinned => continue,
            };
            op.record_elapsed(names::WAITED_US);
            // Only what this write hands over reaches its commit, not what an aborted one left: a replacement that failed
            // before committing can't vouch for this write
            *self.deposited.lock() = None;
            *self.touched.lock() = None;
            self.replacing.store(0, Ordering::Release);
            run_point!(WRITE_HELD);
            let outcome = {
                let result = f(&mut held.txn, self.db);
                match result {
                    Ok(t) => held.commit(self).map(|sequence| (t, sequence)),
                    Err(e) => {
                        drop(held);
                        Err(e)
                    }
                }
            };
            match outcome {
                Ok((t, sequence)) => {
                    let synced = self.committed(durable);
                    return Ok((t, Commit { sequence, synced }));
                }
                Err(e) if e.code == ErrorCode::MapFull && growths < MAX_GROWTHS => {
                    growths += 1;
                    op.record(names::GROWTHS, || growths as u64);
                    self.grow_beside_units()?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Holds a write back before it begins, while what the pin holds back is past its bound, until a sync of the
    /// commits made so far has landed: the sync moves the pin, and the pages freed before it are used again. Where
    /// commits outrun the disk, every commit while the pin holds writes fresh pages, and without this the file grows by
    /// all that is committed while a sync runs, which a sync of more pages makes longer. Outside every lock, as the
    /// sync takes them all. The write is refused where the sync failed: as every commit is refused, where it failed as
    /// it read or flushed the record or flushed the data; and otherwise, having written nothing to rely on, as a full
    /// reader table fails it, with `SYNC_BEHIND`, which the caller may try again. The pin doesn't move until a sync
    /// succeeds, so a write that went on would grow the file past the bound, without end while the failure lasted; and
    /// since a refused write frees nothing, every write is refused until then, each after a sync of its own
    pub(crate) fn hold_to_the_bound(&self) -> Result<()> {
        if let (Some(kept), Some(commit)) = (&self.kept, &self.commit)
            && kept.past_bound()
            && let Err(failure) = commit.durable().wait()
        {
            // A failure that refuses every commit refuses this one as the commits are refused, whichever wait met it
            if let Some(refused) = kept.refused() {
                return Err(refused);
            }
            return Err(Error::new(
                ErrorCode::SyncBehind,
                format!(
                    "Nothing of this write was made: what the last synced commit of the database at {} keeps from \
                     reuse is past its bound, and the sync that would let it go failed having written nothing to rely on \
                     ({}). \
                     Try the write again, after a pause: it goes on once a sync succeeds. A sync that keeps failing \
                     for a full reader table goes on once other processes, or threads of this one, end the read \
                     transactions they hold",
                    self.path.display(),
                    failure.message
                ),
            ));
        }
        Ok(())
    }

    /// Makes sure this process holds the grouped mode's pin, for a write about to begin its transaction
    fn ensure_pin(&self) -> Result<Option<kept::Writing>> {
        self.kept.as_ref().map(|kept| kept.ensure_pin(&|| self.adopt_map_size())).transpose()
    }

    /// Whether the pin a write made sure of is held still, which it checks once its transaction has begun
    fn pinned(&self) -> bool {
        self.kept.as_ref().is_none_or(|kept| kept.pinned())
    }

    fn check_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(Error::new(
                ErrorCode::ReadOnly,
                format!("The database at {} is open read-only", self.path.display()),
            ));
        }
        Ok(())
    }

    /// Commits a write transaction with the next sequence, answering it; a test may refuse it as a full disk would
    /// (the `COMMIT` failpoint). The sequence is read in the transaction, which holds the environment's one write lock, so
    /// it is the last any process committed
    pub fn commit_txn(&self, mut txn: RwTxn<'_>) -> Result<u64> {
        let replacing = self.replacing.swap(0, Ordering::AcqRel);
        let epoch = stored_epoch(&txn, self.db)?;
        // mutant-skip: replace \|\| with && in Store::commit_txn
        // A replacement's transaction stores the epoch it moves to, and a write's start clears one a failed replacement
        // left: a replacing commit reads back its own epoch, so the second clause adds nothing a test can tell apart
        if epoch != self.epoch.load(Ordering::Acquire) && (replacing == 0 || epoch != replacing) {
            return Err(Error::new(
                ErrorCode::DataReplaced,
                "Another process replaced every row in this database since this handle read it: what it read has gone. \
                 Open the database again, or adopt the replacement, before writing",
            ));
        }
        let next = self.next_sequence(&txn)?;
        let last = next - 1;
        self.db.put(&mut txn, &meta_key(meta::SEQUENCE), &next.to_be_bytes())?;
        if let Some(touched) = self.touched.lock().take() {
            crate::engine::changed::stamp(&mut txn, self.db, &touched, next)?;
        }
        crate::engine::changed::trim_tombstones(&mut txn, self.db, next)?;
        // A test's stand-in for a full disk, armed on its thread or for this environment's path; the transaction
        // aborts as it drops
        failure_point!(COMMIT, Error::new(ErrorCode::Io, "No space left on device"));
        failure_point!(COMMIT at &self.path, Error::new(ErrorCode::Io, "No space left on device"));
        // Held from before the commit is visible until it is accounted for, so an answer never sees it half done
        let mut accounted = self.accounted.lock();
        let skipped = last > accounted.through;
        commits::commit(txn, self.kept.as_deref())?;
        accounted.elsewhere |= skipped;
        accounted.through = next;
        // The epoch this commit stored is this handle's own before anyone can ask whether another process moved it
        // (`replaced_elsewhere`): a replacement made here was otherwise, for a moment, one made elsewhere
        self.epoch.store(epoch, Ordering::Release);
        drop(accounted);
        self.sequence.fetch_max(next, Ordering::AcqRel);
        run_point!(COMMIT_LANDED);
        // LMDB's lock went with the commit, and the writer's hold is kept until this returns (`writers.rs`): still
        // this process's one writer, so commits reach the mirror in their order, each with the edges it deposited
        let entries = self.deposited.lock().take();
        self.mirror.lock().commit(last, next, entries.as_deref());
        // The wake another process sent for what this commit skipped may have been lost, or never sent by one that died
        // first: this handle hears it now as if it had arrived
        if skipped {
            self.wake.wake_self();
        }
        self.wake.notify();
        Ok(next)
    }

    /// Whether another handle has committed since this one last asked, or since it opened. A replacement of every row
    /// that took the sequence back counts as one
    pub(crate) fn committed_elsewhere(&self) -> Result<bool> {
        loop {
            // The gate before the count of what is accounted for, the order a commit takes them in
            let gate = self.gate();
            let mut accounted = self.accounted.lock();
            // A transaction of its own rather than an idle read kept for a burst, which may be from before the latest
            // commit. A handle that asks before it reads may hear here first that another process grew the file past
            // its map, which it adopts as a read does
            let txn = match read_txn(&self.env) {
                Ok(txn) => txn,
                Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                    drop(accounted);
                    drop(gate);
                    self.adopt_map_size()?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let held = self.db.get(&txn, &meta_key(meta::SEQUENCE))?;
            let now = held.and_then(|bytes| <[u8; 8]>::try_from(bytes).ok()).map_or(0, u64::from_be_bytes);
            if now != accounted.through {
                accounted.elsewhere = true;
                accounted.through = now;
            }
            return Ok(std::mem::take(&mut accounted.elsewhere));
        }
    }

    /// The sequence the write transaction `txn` commits as. It holds the environment's one write lock, so the stored
    /// sequence it reads is the last any process committed
    pub(crate) fn next_sequence(&self, txn: &heed::RoTxn<'_, heed::AnyTls>) -> Result<u64> {
        let stored = self.db.get(txn, &meta_key(meta::SEQUENCE))?;
        let last = stored.and_then(|bytes| <[u8; 8]>::try_from(bytes).ok()).map_or(0, u64::from_be_bytes);
        last.max(self.sequence.load(Ordering::Acquire)).checked_add(1).ok_or_else(|| {
            Error::new(ErrorCode::Corrupted, "The database commit sequence reached u64::MAX and cannot advance further")
        })
    }

    /// Moves the stored epoch on, in the transaction replacing every row, which this handle's commit may then finish
    pub(crate) fn replacing_every_row(&self, txn: &mut RwTxn<'_>, db: Db) -> Result<()> {
        let next = stored_epoch(txn, db)? + 1;
        put_meta(txn, db.main, meta::EPOCH, &next.to_be_bytes())?;
        self.replacing.store(next, Ordering::Release);
        Ok(())
    }

    /// The epoch another process's replacement of every row moved the database to, when this handle hasn't adopted it
    pub(crate) fn replaced_elsewhere(&self) -> Result<Option<u64>> {
        let stored = self.read(|txn, db| stored_epoch(txn, db))?;
        if stored == self.epoch.load(Ordering::Acquire) {
            return Ok(None);
        }
        // Read between a commit of this handle's becoming visible and its epoch being taken, a replacement made
        // here is one made elsewhere: the commit holds this until it has taken the epoch (`commit_txn`), so asked
        // again past it the two agree. Taken after the read, not around it: a commit takes the gate and then this
        drop(self.accounted.lock());
        Ok((stored != self.epoch.load(Ordering::Acquire)).then_some(stored))
    }

    /// Takes `epoch` as seen, so commits here are taken again, if it is still the stored one: a replacement since is news
    /// the caller hasn't acted on, so it answers false and adopts nothing
    pub(crate) fn adopt_replacement(&self, epoch: u64) -> Result<bool> {
        if self.read(|txn, db| stored_epoch(txn, db))? != epoch {
            return Ok(false);
        }
        self.epoch.store(epoch, Ordering::Release);
        Ok(true)
    }

    /// Entities the write about to commit wrote, which its commit stamps; what is deposited before it is kept too, as a
    /// replacement deposits every entity before its unit deposits what it wrote
    pub(crate) fn deposit_touched(&self, touched: crate::engine::changed::Touched) {
        let mut slot = self.touched.lock();
        match slot.as_mut() {
            Some(held) => held.merge(touched),
            None => *slot = Some(touched),
        }
    }

    /// The edge entries the write about to commit filed and unfiled, for the edge mirror
    pub(crate) fn deposit(&self, entries: Vec<EdgeDelta>) {
        *self.deposited.lock() = Some(entries);
    }

    #[cfg(test)]
    pub(crate) fn when_rank_checked(&self, run: impl FnOnce() + Send + 'static) {
        *self.rank_checked.lock() = Some(Box::new(run));
    }

    #[cfg(test)]
    pub(crate) fn rank_checked(&self) {
        let run = self.rank_checked.lock().take();
        if let Some(run) = run {
            run();
        }
    }

    #[cfg(test)]
    pub(crate) fn when_rank_planned(&self, run: impl FnOnce() + Send + 'static) {
        *self.rank_planned.lock() = Some(Box::new(run));
    }

    #[cfg(test)]
    pub(crate) fn rank_planned(&self) {
        let run = self.rank_planned.lock().take();
        if let Some(run) = run {
            run();
        }
    }

    /// What rank drains read the graph through
    pub(crate) fn mirror(&self) -> Arc<PlainMutex<EdgeMirror>> {
        Arc::clone(&self.mirror)
    }

    /// The last commit's sequence, or 0 before the first: the stored one, which another process's commit may have
    /// raised
    pub fn sequence(&self) -> u64 {
        let stored = self
            .read(|txn, db| Ok(db.get(txn, &meta_key(meta::SEQUENCE))?.map(<[u8]>::to_vec)))
            .ok()
            .flatten()
            .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_slice()).ok())
            .map_or(0, u64::from_be_bytes);
        stored.max(self.sequence.load(Ordering::Acquire))
    }

    /// What follows a commit: group commit hears of it, and a durable one waits for its sync
    pub fn committed(&self, durable: bool) -> Result<()> {
        // Stale now: dropped rather than kept, so they pin none of the pages the commit freed
        self.drop_idle();
        if let Some(commit) = &self.commit {
            commit.on_commit();
            // What the pin holds back makes a sync due as well as the time does
            if self.kept.as_ref().is_some_and(|kept| kept.past_bound()) {
                commit.due_now();
            }
            if durable {
                commit.durable().wait()?;
            }
        }
        // LMDB's meta page, the commit's point, is written with O_DSYNC, which on Apple doesn't flush the drive's cache: a
        // full sync (F_FULLFSYNC, which LMDB's own sync makes of its data file on Apple) does, and a commit that returned
        // is then on disk. Elsewhere LMDB's sync of the meta page is the flush
        // mutant-skip: replace && with \|\| in Store::committed
        // Off Apple the extra sync adds a flush and changes no answer, and no test observes a flush's count
        if self.sync_each_commit && cfg!(target_os = "macos") {
            let _gate = self.gate();
            self.env.force_sync()?;
        }
        Ok(())
    }

    /// Advance LMDB's physical generation after a reset, without changing logical data or sequence. LMDB cannot
    /// reuse the immediately preceding transaction's freed pages: without this checkpoint, the next bulk load
    /// allocates another database-sized set of pages and leaves costly free-page records for later small writes.
    /// Rewrite the sequence read inside this transaction, so an intervening writer's sequence is preserved too.
    pub(crate) fn recycle_reset_pages(&self) -> Result<()> {
        self.check_writable()?;
        // Claimed under the unit's lock, as a write claims its own: a unit that began between a check and the claim
        // would keep the writer's turn for as long as it is open, and this would wait for it with the gate held
        let mut held = loop {
            let writing = self.ensure_pin()?;
            run_point!(WRITE_PINNED);
            let claimed = self
                .unit
                .claim_writer(|| {
                    let guard = self.gate();
                    let writer = self.writers.hold()?.writing(writing);
                    run_point!(RESET_CLAIMING);
                    self.drop_idle();
                    match self.env.write_txn() {
                        Ok(txn) if !self.pinned() => {
                            txn.abort();
                            Ok(Claimed::Unpinned)
                        }
                        Ok(txn) => Ok(Claimed::Held(WriteTxn::new(txn, writer, guard))),
                        Err(heed::Error::Mdb(heed::MdbError::MapResized)) => Ok(Claimed::MapResized),
                        Err(e) => Err(Error::from(e)),
                    }
                })
                .map_err(|_| Error::new(ErrorCode::UnitOpen, "A unit is open; reset page recycling is skipped"))??;
            match claimed {
                Claimed::Held(held) => break held,
                Claimed::MapResized => self.adopt_map_size()?,
                Claimed::Unpinned => {}
            }
        };
        let key = meta_key(meta::SEQUENCE);
        let value = self
            .db
            .get(&held.txn, &key)?
            .ok_or_else(|| Error::new(ErrorCode::Corrupted, "Reset has no committed sequence"))?
            .to_vec();
        self.db.put(&mut held.txn, &key, &value)?;
        failure_point!(RESET_CHECKPOINT, Error::new(ErrorCode::Io, "Injected reset checkpoint failure"));
        held.commit_raw(self.kept.as_deref())?;
        // Route the checkpoint commit through group commit so sync accounting, pin advancement, and failure tracking
        // stay consistent, and a load after the reset can reuse freed pages once the pin advances.
        self.committed(true)
    }

    /// The environment's write transaction for a unit held open across calls, with the gate held for it. Refuses a
    /// second unit, and grows the map first when it is more than half used, since a unit can't grow it once open
    pub(crate) fn begin(&self) -> Result<WriteTxn<'_>> {
        let op = Op::begin();
        let result = self.begin_reported(&op);
        op.ended(result, "begun")
    }

    /// `begin`'s work, recording on `op` what it waited for and retried
    fn begin_reported(&self, op: &Op) -> Result<WriteTxn<'_>> {
        self.check_writable()?;
        // Refused before any wait, as the reservation would refuse it: a unit's owner asking again has its answer
        // without waiting on a sync held back to the bound
        if self.unit_is_open() {
            return Err(Error::new(ErrorCode::UnitOpen, "A unit is already open on this database"));
        }
        // Before the unit's place is reserved: while it waits, writes on other threads go on rather than being refused
        // as beside an open unit
        self.hold_to_the_bound()?;
        self.unit.reserve()?;
        // Let go when the unit doesn't begin, whether it is refused or what begins it panics
        let reserved = Reserved(self);
        let begun = (|| {
            if self.map_half_used() {
                self.grow()?;
            }
            run_point!(UNIT_RESERVED);
            loop {
                let writing = self.ensure_pin()?;
                run_point!(UNIT_PINNED);
                let guard = self.gate.read_arc();
                let writer = self.writers.hold()?.writing(writing);
                match self.env.write_txn() {
                    // A growth ended the pin between its making sure of it and the transaction's beginning
                    Ok(txn) if !self.pinned() => txn.abort(),
                    Ok(txn) => {
                        let held = WriteTxn::new(txn, writer, guard);
                        op.record_elapsed(names::WAITED_US);
                        // A unit's commit adds what it touched to whatever is deposited then, so what a refused or
                        // failed write left is dropped here, as a write's start drops it; the edges one left are
                        // replaced by the unit's own at its commit. Nor can a replacement that failed before
                        // committing vouch for this unit
                        *self.touched.lock() = None;
                        self.replacing.store(0, Ordering::Release);
                        return Ok(held);
                    }
                    // Another process grew the file: adopting its size takes the gate for writing, so this
                    // transaction's hold on it goes first, which would otherwise make the adopting give way every
                    // time, and the writer's turn with it, which on macOS keeps the file lock other processes'
                    // writers wait for
                    Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                        drop(writer);
                        drop(guard);
                        run_point!(UNIT_MAP_GROWN);
                        self.adopt_map_size()?;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        })();
        if begun.is_ok() {
            std::mem::forget(reserved);
        }
        begun
    }

    /// Whether more than half the map is used, which a unit about to open grows it for
    fn map_half_used(&self) -> bool {
        self.map_short_of(2)
    }

    /// The flags the environment was opened with, which say when LMDB syncs (`EnvFlags`)
    #[cfg(test)]
    pub(crate) fn env_flags(&self) -> u32 {
        self.env.get_flags().expect("the environment's flags read")
    }

    /// The size of a page of the data file, which LMDB writes whole
    #[cfg(test)]
    pub(crate) fn page_size(&self) -> usize {
        let _gate = self.gate();
        self.env.unnamed_database_stat().page_size as usize
    }

    /// Whether `times` what the map has in use is more than the map
    pub(crate) fn map_short_of(&self, times: usize) -> bool {
        let _gate = self.gate();
        let info = self.env.info();
        let page = self.env.unnamed_database_stat().page_size as usize;
        info.last_page_number
            .checked_add(1)
            .and_then(|pages| pages.checked_mul(page))
            .and_then(|used| used.checked_mul(times))
            .is_none_or(|needed| needed > info.map_size)
    }

    /// Grows the map until it is `times` what is in use: what a write that makes again most of what the database
    /// holds asks before it begins. The pages such a write frees are no room for it until it commits, so in a map
    /// with less it would run until the map is full, and then again in a larger one. Nothing grows while a unit
    /// is open
    pub fn make_room(&self, times: usize) -> Result<()> {
        while self.map_short_of(times) {
            match self.grow_beside_units() {
                // Nothing grows while a unit is open: the write this room is for is refused for it in turn
                Err(error) if error.code == ErrorCode::UnitOpen => return Ok(()),
                grown => grown?,
            }
        }
        Ok(())
    }

    /// The open unit ended, committed or not: whoever waits for the write transaction hears of it
    pub fn end_unit(&self) {
        self.unit.release();
    }

    pub fn unit_is_open(&self) -> bool {
        self.unit.is_open()
    }

    /// Blocks while a unit is open, until it ends or `stop` answers true; a caller that makes `stop` true wakes it
    /// with `wake_unit_waiters`
    pub fn wait_while_unit_open(&self, stop: impl Fn() -> bool) {
        let op = Op::unit_wait();
        self.unit.wait_while_open(stop);
        op.end(Ok("returned"));
    }

    /// Wakes every `wait_while_unit_open`, to check its `stop` again
    pub fn wake_unit_waiters(&self) {
        self.unit.wake();
    }

    pub fn db(&self) -> Db {
        self.db
    }

    /// Doubles the map, holding every transaction of this process off while it does: for a unit about to begin,
    /// whose reserved place means no other unit is open to wait for
    pub fn grow(&self) -> Result<()> {
        self.check_writable()?;
        let gate = self.gate.write();
        self.grow_holding(gate)
    }

    /// `grow`, for a write or a rebuild that is no unit's own: the gate is taken only while no unit is open, and the
    /// growth refused (`UNIT_OPEN`) once one is, as the write it is for would be. A unit holds the gate for reading
    /// for its whole life, and one begun on another thread, the event loop's while this runs on a thread of the
    /// pool, would leave a growth waiting on it for as long as that thread waits on the growth
    pub fn grow_beside_units(&self) -> Result<()> {
        self.check_writable()?;
        loop {
            if self.unit_is_open() {
                return Err(Error::new(
                    ErrorCode::UnitOpen,
                    "A unit opened while the map was to grow, holding the database's one write transaction: commit or \
                     abort it, then write",
                ));
            }
            run_point!(GROWTH_GATE);
            if let Some(gate) = self.gate.try_write_for(GROWTH_GATE_TRY) {
                return self.grow_holding(gate);
            }
        }
    }

    /// `grow`'s work, the gate held for writing
    fn grow_holding(&self, _gate: RecursiveRwLockWriteGuard<'_, ()>) -> Result<()> {
        // Each read asked at a call is run on its own snapshot before its transaction is ended, never on a later one
        self.calls.run_all();
        self.drop_idle();
        // A pin is a transaction too: the write that grows makes sure of it again before it runs again
        if let Some(kept) = &self.kept {
            kept.end_pin();
        }
        let size = self.env.info().map_size;
        let grown = next_map_size(size)
            .ok_or_else(|| Error::new(ErrorCode::MapFull, format!("The map at {size} bytes cannot grow further")))?;
        #[cfg(test)]
        if grown > TEST_MAP_LIMIT {
            return Err(Error::new(ErrorCode::MapFull, format!("A test's database grew past {TEST_MAP_LIMIT} bytes")));
        }
        // SAFETY: the gate held for writing means no read or write runs in this process, the reads asked at calls have
        // run and their transactions are ended, and the idle read transactions are dropped: no transaction is open,
        // which LMDB requires. Nor does anything else read through
        // the map it unmaps: a sync and each read of the environment's sizes hold the gate for reading
        unsafe { self.env.resize(grown) }?;
        Ok(())
    }

    /// Takes the map size another process grew the file to, or gives way, taking nothing, where the gate isn't free
    /// soon (`gate_to_adopt`): every caller begins its transaction again after it, and takes the size when it is
    /// refused again
    fn adopt_map_size(&self) -> Result<()> {
        if let Some(kept) = &self.kept {
            return kept.adopt();
        }
        let Some(_gate) = gate_to_adopt(&self.gate) else { return Ok(()) };
        self.calls.run_all();
        self.drop_idle();
        // SAFETY: as in `grow`
        unsafe { adopt_grown_map(&self.env, &self.path, self.read_only) }?;
        Ok(())
    }

    /// The map's size now
    pub fn map_size(&self) -> usize {
        let _gate = self.gate();
        self.env.info().map_size
    }

    /// Waits for every unit committed before the call to be on disk. A unit's owner may wait on it while its unit is
    /// open: the unit holds the write transaction every process takes to commit, so no map grows while it is open; a
    /// sync that met a growth from before it gives way to the unit rather than waiting on it (`gate_to_adopt`), and
    /// the sync takes nothing else the unit holds
    pub fn durable(&self) -> Option<Durable> {
        self.commit.as_ref().map(GroupCommit::durable)
    }

    /// Syncs the store's group commit has run
    pub fn syncs(&self) -> u64 {
        self.commit.as_ref().map_or(0, GroupCommit::syncs)
    }

    /// Syncs what is owed and releases the environment
    pub fn close(mut self) -> Result<()> {
        let synced = match self.commit.take() {
            Some(mut commit) => commit.close(),
            None => Ok(()),
        };
        // LMDB closes the environment once its last handle goes, which dropping the store and its idle read
        // transactions is, with the ones held for reads asked at calls, which the drop ends. The lock that says it is
        // open goes only once it has closed, and the in-process registration last of all.
        self.idle.close();
        let registration = self._registration.take();
        let presence = self.presence.take();
        let closed = self.env.clone().prepare_for_closing();
        drop(self);
        closed.wait();
        drop(presence);
        drop(registration);
        synced
    }

    /// Syncs on the calling thread what group commit owes, without its worker, as a process that is exiting does. The
    /// gate is tried and not waited for: a thread that held it for a growth may be gone with it, and a map being grown
    /// isn't one to read through. The locks the grouped mode's record takes are waited for until `deadline`: a live
    /// thread holds one until its sync ends, and one that ended holds it for good
    pub fn sync_owed(&self, deadline: Instant) -> Result<()> {
        match self.commit {
            Some(_) => {
                failure_point!(SYNC at &self.path, Error::new(ErrorCode::Io, "Injected sync failure"));
                let Some(_gate) = self.gate.try_read() else {
                    return Err(Error::new(
                        ErrorCode::Io,
                        "The map was being grown as the process exited, so what was owed wasn't synced",
                    ));
                };
                match &self.kept {
                    Some(kept) => kept.sync_at_exit(deadline),
                    None => self.env.force_sync().map_err(Into::into),
                }
            }
            None => Ok(()),
        }
    }

    /// `close` in a process that is exiting, whose other threads may be gone already: syncs on this thread, stops group
    /// commit without its worker (`GroupCommit::stop_at_exit`), and releases the environment without waiting for it
    /// to close, which a handle a gone thread held would keep open
    pub fn close_at_exit(mut self, wait: Duration) -> Result<()> {
        let deadline = sync::deadline_from(Instant::now(), wait);
        self.wake.stop_at_exit();
        let synced = self.sync_owed(deadline);
        if let Some(commit) = self.commit.take() {
            commit.stop_at_exit(deadline.saturating_duration_since(Instant::now()));
        }
        self.idle.stop_at_exit(deadline);
        // Tried until the deadline, where the drop's close would wait for a thread gone holding the lock
        self.calls.close_before(deadline);
        drop(self);
        synced
    }

    /// LMDB's copy of one read transaction into `target/data.mdb`, compacted: without the pages deletes freed. The
    /// copy uses an owned temporary file, synced and atomically replaced while the destination is reserved against
    /// opens and other publishers in this process (`ALREADY_OPEN`). Across processes, it holds the destination's
    /// decision lock (`synced.lock`, created if absent) and, where an open made one, its presence lock (`open.lock`)
    /// until publication finishes, refusing a target another process has open (`TARGET_IN_USE`) while any open that
    /// begins there waits for the backup to end. Failure before publication preserves an earlier backup; a later
    /// directory sync failure reports that the complete copy is visible and a retry is safe
    pub fn copy_to(&self, target: &Path) -> Result<()> {
        backup::copy(self, target)
    }

    /// Whether the record beside the data file named another file than this one, which was opened as it is
    pub(crate) fn not_borne_out(&self) -> bool {
        self.not_borne_out
    }

    /// The syncs of the record file this process's syncs have made, for a test
    #[cfg(test)]
    pub(crate) fn record_syncs(&self) -> u64 {
        self.kept.as_ref().map_or(0, |kept| kept.record_syncs.load(Ordering::Relaxed))
    }

    /// Turns the grouped mode's pin off, its transaction ended as soon as it is begun, to show what is lost without it
    #[cfg(test)]
    pub(crate) fn unpin(&self) {
        if let Some(kept) = &self.kept {
            kept.unpinned.store(true, Ordering::Relaxed);
        }
    }

    /// The grouped mode's record held by another thread, as a sync of it does, until what this returns is dropped
    #[cfg(test)]
    pub(crate) fn hold_record(&self) -> HeldRecord {
        let kept = Arc::clone(self.kept.as_ref().expect("a store of the grouped mode"));
        let (taken, took) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _record = kept.record_guard();
            taken.send(()).expect("the test waits for the record to be held");
            let _ = released.recv_timeout(Duration::from_secs(60));
        });
        took.recv().expect("the record is held");
        HeldRecord { release: Some(release), holder: Some(holder) }
    }

    /// Sets the fixed part of the bound past which what the pin holds back makes a sync due, for a test
    #[cfg(test)]
    pub(crate) fn set_bound(&self, bytes: u64) {
        if let Some(kept) = &self.kept {
            kept.set_bound(bytes);
        }
    }

    /// The record's commit this process relies on, for a test
    #[cfg(test)]
    pub(crate) fn remembered(&self) -> Option<u64> {
        self.kept.as_ref().and_then(|kept| kept.remembered())
    }

    /// Bytes the data file holds on disk
    pub fn file_size(&self) -> u64 {
        std::fs::metadata(self.path.join("data.mdb")).map(|m| m.len()).unwrap_or(0)
    }

    /// The bytes under a meta key, for store assertions
    #[cfg(test)]
    pub fn meta(&self, name: &str) -> Result<Option<Vec<u8>>> {
        self.read(|txn, db| Ok(db.get(txn, &meta_key(name))?.map(<[u8]>::to_vec)))
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // `close` takes these first; a store dropped unclosed releases them here. `_registration` is the last field on
        // `Store`, so `open_paths()` unregisters only after `env`, `kept`, and `presence` drop.
        self.wake.stop();
        if let Some(mut commit) = self.commit.take() {
            let _ = commit.close();
        }
        self.calls.close();
        self.idle.close();
    }
}

/// Group commit's sync: the grouped mode's, which moves the pin and writes the record after it syncs the data
struct EnvSyncer {
    kept: Arc<Kept>,
}

impl Syncer for EnvSyncer {
    fn sync(&self) -> Result<()> {
        self.kept.sync(&|| self.kept.adopt())
    }

    /// Final where the process refuses its commits: the sync, or a write's, failed as it read or flushed the record or
    /// flushed the data. Every wait then fails with that failure, whichever sync met it. One that failed short of a
    /// flush, as a full reader table fails it, wrote nothing to rely on, and the next sync tries again
    fn final_failure(&self, _failure: &Error) -> Option<Error> {
        self.kept.failure()
    }
}

pub fn meta_key(name: &str) -> Vec<u8> {
    let mut key = Prefix::Meta.key();
    key.string(name);
    key.to_vec()
}

fn stored_epoch(txn: &heed::RoTxn<'_, heed::AnyTls>, db: Db) -> Result<u64> {
    let held = db.get(txn, &meta_key(meta::EPOCH))?;
    Ok(held.and_then(|bytes| <[u8; 8]>::try_from(bytes).ok()).map_or(0, u64::from_be_bytes))
}

pub fn put_meta(txn: &mut RwTxn<'_>, db: heed::Database<'_, Bytes, Bytes>, name: &str, value: &[u8]) -> Result<()> {
    db.put(txn, &meta_key(name), value).map_err(Into::into)
}

fn check_stamp(opening: &Opening<'_>, db: heed::Database<'_, Bytes, Bytes>) -> Result<()> {
    let path = opening.dir;
    let txn = opening.read()?;
    let kind = db.get(&txn, &meta_key(meta::KIND))?;
    if kind != Some(format::KIND.as_bytes()) {
        return Err(Error::new(ErrorCode::NotADatabase, format!("{} holds no elmdb database", path.display())));
    }
    let format =
        db.get(&txn, &meta_key(meta::FORMAT))?.and_then(|b| <[u8; 4]>::try_from(b).ok()).map(u32::from_be_bytes);
    match format {
        Some(format::FORMAT_VERSION) => Ok(()),
        Some(f) if f > format::FORMAT_VERSION => Err(Error::new(
            ErrorCode::WrongFormat,
            format!(
                "The database at {} is in format {f}, newer than this elmdb's {}; update elmdb to open it",
                path.display(),
                format::FORMAT_VERSION
            ),
        )),
        Some(f) => Err(Error::new(
            ErrorCode::WrongFormat,
            format!(
                "The database at {} is in format {f}, from before this elmdb's format {} was fixed; it can't be read, and is \
                 made again",
                path.display(),
                format::FORMAT_VERSION
            ),
        )),
        None => Err(Error::new(ErrorCode::Corrupted, format!("The database at {} records no format", path.display()))),
    }
}

#[cfg(test)]
mod kept_tests;
#[cfg(test)]
mod retention_tests;
#[cfg(test)]
mod synced_model;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod wait_tests;
