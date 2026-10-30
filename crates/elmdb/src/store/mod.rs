//! The store: one LMDB environment, its one database (`format::DB_NAME`), the format stamp, reads, and writes with
//! group commit and map growth (`docs/architecture/engine.md`, Durability; `docs/architecture/format.md`).
//!
//! Every transaction holds `gate` for reading, and growing the map holds it for writing, since LMDB grows a map only
//! while its process has no transaction open. So `read` and `write` never call each other: a write reads through its
//! own transaction. The gate lets a reader in whenever another holds it, so an open unit, which holds it across calls,
//! never waits on a growth that waits on the unit.

mod backup;
#[cfg(unix)]
pub(crate) use backup::directories_to_sync as publication_directories;
pub(crate) use backup::publish_new;
pub mod sync;
mod unit_slot;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use heed::types::Bytes;
use heed::{CommitOrAbort, Env, EnvFlags, EnvOpenOptions, RoTxn, RwTxn, WithoutTls};
use parking_lot::lock_api::ArcRwLockReadGuard;
use parking_lot::{RawRwLockRecursive, RecursiveRwLock, RecursiveRwLockReadGuard};

use crate::error::{Error, ErrorCode, Result};
use crate::format::{self, Prefix, meta};
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
            sync_idle: Duration::from_millis(100),
            sync_interval: Duration::from_millis(1000),
            map_size_bytes: None,
        }
    }
}

/// The map grows in steps of this, and starts at it at least. LMDB needs a multiple of the OS page size, which this is
pub const MAP_STEP: usize = 64 << 20;

/// The largest map a unit test's database grows to. No test needs near it, so a change that keeps a database growing
/// fails the test at it, as `MAP_FULL`, rather than filling the disk
#[cfg(test)]
const TEST_MAP_LIMIT: usize = 1 << 30;

/// How many times one write grows the map before giving up with `MAP_FULL`
const MAX_GROWTHS: usize = 16;

/// The map's size for a data file of `file_bytes`: twice the file, so a database can double before its first growth,
/// and a step at least. This is writable mapping capacity, independent of the file's current physical length.
pub fn initial_map_size(file_bytes: u64) -> usize {
    let wanted = (file_bytes as usize).saturating_mul(2).max(MAP_STEP);
    wanted.div_ceil(MAP_STEP) * MAP_STEP
}

// Windows read-only sections map only the current file, not a writer's reserved capacity. The logical bound
// must not exceed that view: transaction renewal then reports MAP_RESIZED before accessing newly appended pages.
#[cfg(any(windows, test))]
pub(crate) fn read_only_map_size(file_bytes: u64, requested: Option<usize>) -> usize {
    let physical = usize::try_from(file_bytes).unwrap_or(usize::MAX);
    requested.unwrap_or(physical).min(physical)
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

pub struct Store {
    path: PathBuf,
    read_only: bool,
    env: Env<WithoutTls>,
    db: Db,
    gate: Arc<RecursiveRwLock<()>>,
    commit: Option<GroupCommit>,
    /// Whether a unit is open, holding the environment's one write transaction
    unit: UnitSlot,
    /// Read transactions a read finished with, for the next read to take while it is still current
    idle: Mutex<Vec<RoTxn<'static, WithoutTls>>>,
    /// The last commit's sequence (`meta::SEQUENCE`), which every commit raises by one
    sequence: AtomicU64,
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

/// Reserve an existing canonical destination against opens and publishers in this process.
pub(crate) fn reserve_destination(target: &Path) -> Result<Registration> {
    if !open_paths().lock().expect("the open-paths lock is never poisoned").insert(target.to_owned()) {
        return Err(Error::new(
            ErrorCode::AlreadyOpen,
            format!("The database at {} is open or being published; use another directory", target.display()),
        ));
    }
    Ok(Registration(Some(target.to_owned())))
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

// LMDB also silently omits its lock table after EROFS, even without NO_LOCK. Beginning a reader first makes
// mti_numreaders nonzero for every registered environment; a missing lock table always reports zero.
pub(crate) fn require_registered_reader(env: &Env<WithoutTls>, path: &Path) -> Result<()> {
    let _reader = read_txn(env)?;
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
        let mut registration = Registration(Some(canonical.clone()));

        let file_bytes = std::fs::metadata(&data).map(|m| m.len()).unwrap_or(0);
        let open = |flags: EnvFlags| {
            let mut builder = EnvOpenOptions::new().read_txn_without_tls();
            let map_size = options.map_size_bytes.unwrap_or_else(|| initial_map_size(file_bytes));
            #[cfg(windows)]
            let map_size =
                if options.read_only { read_only_map_size(file_bytes, options.map_size_bytes) } else { map_size };
            builder.max_dbs(8).map_size(map_size);
            // SAFETY: the flags change when LMDB writes and flushes, which the binding doesn't rely on: commits are
            // flushed by group commit's syncs, and a read-only environment is never written
            unsafe {
                builder.flags(flags);
            }
            // SAFETY: `open_paths` guarantees this process holds no other handle to the environment, and its files are
            // LMDB's, written only through LMDB
            unsafe { builder.open(&canonical) }
        };
        let env =
            open(if options.read_only { EnvFlags::READ_ONLY } else { EnvFlags::NO_SYNC }).map_err(|e| match e {
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

        if options.read_only {
            require_registered_reader(&env, &canonical)?;
        }
        let db = Self::open_db(&env, &canonical, options.read_only)?;
        let commit = (!options.read_only && options.syncs).then(|| {
            GroupCommit::start(
                Policy::new(options.sync_idle, options.sync_interval),
                Box::new(EnvSyncer { env: env.clone(), path: canonical.clone() }),
                Box::new(SystemClock),
            )
        });
        let sequence = {
            let txn = read_txn(&env)?;
            let held = db.get(&txn, &meta_key(meta::SEQUENCE))?;
            held.and_then(|bytes| <[u8; 8]>::try_from(bytes).ok()).map_or(0, u64::from_be_bytes)
        };
        registration.0 = None;
        Ok(Self {
            path: canonical,
            read_only: options.read_only,
            env,
            db,
            gate: Arc::new(RecursiveRwLock::new(())),
            commit,
            unit: UnitSlot::new(),
            idle: Mutex::new(Vec::new()),
            sequence: AtomicU64::new(sequence),
        })
    }

    /// The environment's elmdb database, checked to be in this version's format; a new environment's is created and
    /// stamped first
    fn open_db(env: &Env<WithoutTls>, path: &Path, read_only: bool) -> Result<Db> {
        let rtxn = read_txn(env)?;
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
                let txn = read_txn(env)?;
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
            check_stamp(env, db, path)?;
            return Self::with_graphs(env, db, read_only);
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
        let wtxn = env.write_txn()?;
        let db = env
            .create_databases_and_commit(wtxn, |opener, wtxn| -> Result<_> {
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
        env.force_sync()?;
        Self::with_graphs(env, db, read_only)
    }

    /// The databases around `main`: hannoy's graph databases, created in a writable environment that lacks them, and
    /// opened, when there, in a read-only one
    fn with_graphs(env: &Env<WithoutTls>, main: heed::Database<'static, Bytes, Bytes>, read_only: bool) -> Result<Db> {
        let rtxn = read_txn(env)?;
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
        let wtxn = env.write_txn()?;
        let (cosine, euclidean) = env
            .create_databases_and_commit(wtxn, |opener, wtxn| -> heed::Result<_> {
                let cosine = opener.create_database(wtxn, Some(format::COSINE_GRAPHS))?;
                let euclidean = opener.create_database(wtxn, Some(format::EUCLIDEAN_GRAPHS))?;
                Ok(CommitOrAbort::<_, ()>::Commit((cosine, euclidean)))
            })?
            .unwrap_commit();
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
    /// is what a read saves; a read transaction otherwise outlives its read only until the next commit here
    pub fn read<T>(&self, f: impl FnOnce(&RoTxn<'_, WithoutTls>, Db) -> Result<T>) -> Result<T> {
        let mut f = Some(f);
        loop {
            let guard = self.gate();
            let txn = match self.current_idle() {
                Some(txn) => txn,
                None => match with_reader_recovery(&self.env, || self.env.clone().static_read_txn()) {
                    Ok(txn) => txn,
                    Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                        drop(guard);
                        self.adopt_map_size()?;
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                },
            };
            let result = (f.take().expect("a read runs once"))(&txn, self.db);
            let mut idle = self.idle.lock().expect("the idle transactions' lock is never poisoned");
            if idle.len() < MAX_IDLE {
                idle.push(txn);
            }
            drop(idle);
            drop(guard);
            return result;
        }
    }

    /// An idle read transaction that is still the last commit's snapshot, the stale ones dropped
    fn current_idle(&self) -> Option<RoTxn<'static, WithoutTls>> {
        let mut idle = self.idle.lock().expect("the idle transactions' lock is never poisoned");
        if idle.is_empty() {
            return None;
        }
        let last = self.env.info().last_txn_id;
        while let Some(txn) = idle.pop() {
            if txn.id() == last {
                return Some(txn);
            }
        }
        None
    }

    /// Drops every idle read transaction: after a commit here, which makes them stale, and before the map is resized
    /// or the environment closed, which no open transaction may outlast
    fn drop_idle(&self) {
        self.idle.lock().expect("the idle transactions' lock is never poisoned").clear();
    }

    /// Runs `f` in a write transaction and commits it, or aborts it when `f` fails. A write that fills the map is
    /// aborted, the map grown and `f` run again, so `f` must do nothing but write through its transaction
    pub fn write<T>(&self, f: impl FnMut(&mut RwTxn<'_>, Db) -> Result<T>, durable: bool) -> Result<T> {
        self.write_sequenced(f, durable).map(|(t, _)| t)
    }

    /// `write`, answering also the sequence of the commit it made
    pub fn write_sequenced<T>(
        &self,
        f: impl FnMut(&mut RwTxn<'_>, Db) -> Result<T>,
        durable: bool,
    ) -> Result<(T, u64)> {
        let (t, commit) = self.write_committed(f, durable)?;
        let sequence = commit.sequence;
        commit.finish()?;
        Ok((t, sequence))
    }

    /// `write_sequenced`, answering a durable write's failed sync apart from the commit, which happened
    pub(crate) fn write_committed<T>(
        &self,
        mut f: impl FnMut(&mut RwTxn<'_>, Db) -> Result<T>,
        durable: bool,
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
            // Claimed under the unit's lock: a unit that slipped between the earlier check and write_txn would leave
            // this caller waiting on the unit's writer while it holds a schema guard
            let claimed = self.unit.claim_writer(|| {
                let guard = self.gate();
                match self.env.write_txn() {
                    Ok(txn) => Ok(Some((txn, guard))),
                    Err(heed::Error::Mdb(heed::MdbError::MapResized)) => Ok(None),
                    Err(e) => Err(Error::from(e)),
                }
            })??;
            let Some((mut txn, guard)) = claimed else {
                self.adopt_map_size()?;
                continue;
            };
            let outcome = f(&mut txn, self.db).and_then(|t| self.commit_txn(txn).map(|sequence| (t, sequence)));
            drop(guard);
            match outcome {
                Ok((t, sequence)) => {
                    let synced = self.committed(durable);
                    return Ok((t, Commit { sequence, synced }));
                }
                Err(e) if e.code == ErrorCode::MapFull && growths < MAX_GROWTHS => {
                    growths += 1;
                    self.grow()?;
                }
                Err(e) => return Err(e),
            }
        }
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
        let stored = self.db.get(&txn, &meta_key(meta::SEQUENCE))?;
        let last = stored.and_then(|bytes| <[u8; 8]>::try_from(bytes).ok()).map_or(0, u64::from_be_bytes);
        let next = last.max(self.sequence.load(Ordering::Acquire)) + 1;
        self.db.put(&mut txn, &meta_key(meta::SEQUENCE), &next.to_be_bytes())?;
        // A test's stand-in for a full disk, armed on its thread or for this environment's path; the transaction
        // aborts as it drops
        failure_point!(COMMIT, Error::new(ErrorCode::Io, "No space left on device"));
        failure_point!(COMMIT at &self.path, Error::new(ErrorCode::Io, "No space left on device"));
        txn.commit()?;
        self.sequence.fetch_max(next, Ordering::AcqRel);
        Ok(next)
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
            if durable {
                commit.durable().wait()?;
            }
        }
        Ok(())
    }

    /// Advance LMDB's physical generation after a reset, without changing logical data or sequence. LMDB cannot
    /// reuse the immediately preceding transaction's freed pages: without this checkpoint, the next bulk load
    /// allocates another database-sized set of pages and leaves costly free-page records for later small writes.
    /// Rewrite the sequence read inside this transaction, so an intervening writer's sequence is preserved too.
    pub(crate) fn recycle_reset_pages(&self) -> Result<()> {
        self.check_writable()?;
        if self.unit_is_open() {
            return Err(Error::new(ErrorCode::UnitOpen, "A unit is open; reset page recycling is skipped"));
        }
        let _guard = self.gate();
        self.drop_idle();
        let mut txn = self.env.write_txn()?;
        let key = meta_key(meta::SEQUENCE);
        let value = self
            .db
            .get(&txn, &key)?
            .ok_or_else(|| Error::new(ErrorCode::Corrupted, "Reset has no committed sequence"))?
            .to_vec();
        self.db.put(&mut txn, &key, &value)?;
        failure_point!(RESET_CHECKPOINT, Error::new(ErrorCode::Io, "Injected reset checkpoint failure"));
        txn.commit()?;
        Ok(())
    }

    /// The environment's write transaction for a unit held open across calls, with the gate held for it. Refuses a
    /// second unit, and grows the map first when it is more than half used, since a unit can't grow it once open
    pub fn begin(&self) -> Result<(RwTxn<'_>, ArcRwLockReadGuard<RawRwLockRecursive, ()>)> {
        self.check_writable()?;
        self.unit.reserve()?;
        let begun = (|| {
            let info = self.env.info();
            let page = self.env.unnamed_database_stat().page_size as usize;
            if (info.last_page_number + 1) * page * 2 > info.map_size {
                self.grow()?;
            }
            loop {
                let guard = self.gate.read_arc();
                match self.env.write_txn() {
                    Ok(txn) => return Ok((txn, guard)),
                    // Another process grew the file: adopting its size takes the gate for writing, so this
                    // transaction's hold on it goes first
                    Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                        drop(guard);
                        self.adopt_map_size()?;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        })();
        if begun.is_err() {
            self.end_unit();
        }
        begun
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
        self.unit.wait_while_open(stop);
    }

    /// Wakes every `wait_while_unit_open`, to check its `stop` again
    pub fn wake_unit_waiters(&self) {
        self.unit.wake();
    }

    pub fn db(&self) -> Db {
        self.db
    }

    /// Doubles the map, holding every transaction of this process off while it does
    pub fn grow(&self) -> Result<()> {
        self.check_writable()?;
        let _gate = self.gate.write();
        self.drop_idle();
        let size = self.env.info().map_size;
        let grown = size.saturating_mul(2).div_ceil(MAP_STEP) * MAP_STEP;
        #[cfg(test)]
        if grown > TEST_MAP_LIMIT {
            return Err(Error::new(ErrorCode::MapFull, format!("A test's database grew past {TEST_MAP_LIMIT} bytes")));
        }
        // SAFETY: the gate held for writing means no read or write runs in this process, and the idle read
        // transactions are dropped: no transaction is open, which LMDB requires
        unsafe { self.env.resize(grown) }?;
        Ok(())
    }

    /// Takes the map size another process grew the file to
    fn adopt_map_size(&self) -> Result<()> {
        let _gate = self.gate.write();
        self.drop_idle();
        #[cfg(windows)]
        let size = if self.read_only {
            read_only_map_size(std::fs::metadata(self.path.join("data.mdb"))?.len(), None)
        } else {
            0
        };
        #[cfg(not(windows))]
        let size = 0;
        // SAFETY: as in `grow`; a size of 0 adopts the recorded capacity. Windows read-only mappings instead use
        // physical file length, so the next external extension is detected before any page outside that view is read.
        unsafe { self.env.resize(size) }?;
        Ok(())
    }

    /// The map's size now
    pub fn map_size(&self) -> usize {
        self.env.info().map_size
    }

    /// Waits for every unit committed before the call to be on disk
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
        // transactions is
        self.drop_idle();
        let closed = self.env.clone().prepare_for_closing();
        drop(self);
        closed.wait();
        synced
    }

    /// Syncs on the calling thread what group commit owes, without its worker
    pub fn sync_owed(&self) -> Result<()> {
        match self.commit {
            Some(_) => {
                failure_point!(SYNC at &self.path, Error::new(ErrorCode::Io, "Injected sync failure"));
                self.env.force_sync().map_err(Into::into)
            }
            None => Ok(()),
        }
    }

    /// `close` in a process that is exiting, whose other threads may be gone already: syncs on this thread, stops group
    /// commit without its worker (`GroupCommit::stop_at_exit`), and releases the environment without waiting for it
    /// to close, which a handle a gone thread held would keep open
    pub fn close_at_exit(mut self, wait: Duration) -> Result<()> {
        let synced = self.sync_owed();
        if let Some(commit) = self.commit.take() {
            commit.stop_at_exit(wait);
        }
        self.drop_idle();
        drop(self);
        synced
    }

    /// LMDB's copy of one read transaction into `target/data.mdb`, compacted: without the pages deletes freed. The
    /// copy uses an owned temporary file, synced and atomically replaced while the destination is reserved against
    /// opens/publishers in this process. Failure before publication preserves an earlier backup; a later directory
    /// sync failure reports that the complete copy is visible and a retry is safe. The caller owns the destination
    /// exclusively against other processes; a backup artifact must not be an actively used database
    pub fn copy_to(&self, target: &Path) -> Result<()> {
        backup::copy(self, target)
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
        // `close` takes these first; a store dropped unclosed releases them here
        if let Some(mut commit) = self.commit.take() {
            let _ = commit.close();
        }
        open_paths().lock().expect("the open-paths lock is never poisoned").remove(&self.path);
    }
}

struct EnvSyncer {
    env: Env<WithoutTls>,
    /// What a test arms the `SYNC` failpoint for
    #[cfg_attr(not(test), allow(dead_code))]
    path: PathBuf,
}

impl Syncer for EnvSyncer {
    fn sync(&self) -> Result<()> {
        failure_point!(SYNC at &self.path, Error::new(ErrorCode::Io, "Injected sync failure"));
        self.env.force_sync().map_err(Into::into)
    }
}

pub fn meta_key(name: &str) -> Vec<u8> {
    let mut key = Prefix::Meta.key();
    key.string(name);
    key.to_vec()
}

pub fn put_meta(txn: &mut RwTxn<'_>, db: heed::Database<'_, Bytes, Bytes>, name: &str, value: &[u8]) -> Result<()> {
    db.put(txn, &meta_key(name), value).map_err(Into::into)
}

fn check_stamp(env: &Env<WithoutTls>, db: heed::Database<'_, Bytes, Bytes>, path: &Path) -> Result<()> {
    let txn = read_txn(env)?;
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
mod tests;
