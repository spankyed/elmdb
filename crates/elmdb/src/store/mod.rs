//! The store: one LMDB environment, its one database (`format::DB_NAME`), the format stamp, reads, and writes with
//! group commit and map growth (`docs/architecture/engine.md`, Durability; `docs/architecture/format.md`).
//!
//! Every transaction holds `gate` for reading, and growing the map holds it for writing, since LMDB grows a map only
//! while its process has no transaction open. So `read` and `write` never call each other: a write reads through its
//! own transaction.

pub mod sync;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use heed::types::Bytes;
use heed::{CommitOrAbort, Env, EnvFlags, EnvOpenOptions, RoTxn, RwTxn, WithoutTls};
use parking_lot::lock_api::ArcRwLockReadGuard;
use parking_lot::{RawRwLock, RwLock, RwLockReadGuard};

use crate::error::{Error, ErrorCode, Result};
use crate::format::{self, Prefix, meta};
use sync::{Durable, GroupCommit, Policy, Syncer, SystemClock};

/// The store's database handle
pub type Db = heed::Database<'static, Bytes, Bytes>;

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

/// How many times one write grows the map before giving up with `MAP_FULL`
const MAX_GROWTHS: usize = 16;

/// The map's size for a data file of `file_bytes`: twice the file, so a database can double before its first growth,
/// and a step at least. On Windows LMDB sizes the file to the map, so this is also what an open costs on disk there
pub fn initial_map_size(file_bytes: u64) -> usize {
    let wanted = (file_bytes as usize).saturating_mul(2).max(MAP_STEP);
    wanted.div_ceil(MAP_STEP) * MAP_STEP
}

pub struct Store {
    path: PathBuf,
    read_only: bool,
    env: Env<WithoutTls>,
    db: Db,
    gate: Arc<RwLock<()>>,
    commit: Option<GroupCommit>,
    /// Whether a unit is open, holding the environment's one write transaction, and the signal that one ended
    unit: Mutex<bool>,
    unit_ended: Condvar,
    /// Read transactions a read finished with, for the next read to take while it is still current
    idle: Mutex<Vec<RoTxn<'static, WithoutTls>>>,
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
struct Registration(Option<PathBuf>);

impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            open_paths().lock().expect("the open-paths lock is never poisoned").remove(&path);
        }
    }
}

/// A test's stand-in for a disk that fills while a new environment is stamped: stamping the environment at this path fails
#[cfg(test)]
pub(crate) static FAIL_STAMP_AT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// A test's stand-in for a disk that fills: the next commit of the environment at this path fails, once
#[cfg(test)]
pub(crate) static FAIL_COMMIT_AT: Mutex<Option<PathBuf>> = Mutex::new(None);

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
            builder.max_dbs(8).map_size(options.map_size_bytes.unwrap_or_else(|| initial_map_size(file_bytes)));
            // SAFETY: the flags change when LMDB writes and flushes, which the binding doesn't rely on: commits are
            // flushed by group commit's syncs, and a read-only environment is never written
            unsafe {
                builder.flags(flags);
            }
            // SAFETY: `open_paths` guarantees this process holds no other handle to the environment, and its files are
            // LMDB's, written only through LMDB
            unsafe { builder.open(&canonical) }
        };
        let denied =
            |e: &heed::Error| matches!(e, heed::Error::Io(io) if io.kind() == std::io::ErrorKind::PermissionDenied);
        let env = match open(if options.read_only { EnvFlags::READ_ONLY } else { EnvFlags::NO_SYNC }) {
            // Files this user can't write, which no writer of theirs can be changing either: read without the lock
            // file a reader registers in, which LMDB would otherwise have to write
            Err(e) if options.read_only && denied(&e) => open(EnvFlags::READ_ONLY | EnvFlags::NO_LOCK),
            opened => opened,
        }
        .map_err(|e| match e {
            heed::Error::Mdb(heed::MdbError::Invalid | heed::MdbError::VersionMismatch) => Error::new(
                ErrorCode::NotADatabase,
                format!(
                    "{} isn't a file this LMDB reads: {e}. Files lmdb-js wrote are in its own LMDB data version; \
                     export them with elmdb's tools/lmdb-js-export",
                    data.display()
                ),
            ),
            e => e.into(),
        })?;

        let db = Self::open_db(&env, &canonical, options.read_only)?;
        let commit = (!options.read_only && options.syncs).then(|| {
            GroupCommit::start(
                Policy::new(options.sync_idle, options.sync_interval),
                Box::new(EnvSyncer(env.clone())),
                Box::new(SystemClock),
            )
        });
        registration.0 = None;
        Ok(Self {
            path: canonical,
            read_only: options.read_only,
            env,
            db,
            gate: Arc::new(RwLock::new(())),
            commit,
            unit: Mutex::new(false),
            unit_ended: Condvar::new(),
            idle: Mutex::new(Vec::new()),
        })
    }

    /// The environment's elmdb database, checked to be in this version's format; a new environment's is created and
    /// stamped first
    fn open_db(env: &Env<WithoutTls>, path: &Path, read_only: bool) -> Result<Db> {
        let rtxn = env.read_txn()?;
        let (db, main) = env
            .open_databases_and_commit(rtxn, |opener, rtxn| -> heed::Result<_> {
                let elmdb = opener.open_database::<Bytes, Bytes>(rtxn, Some(format::DB_NAME))?;
                let main = opener.open_database::<Bytes, Bytes>(rtxn, None)?;
                Ok(CommitOrAbort::<_, ()>::Commit((elmdb, main)))
            })?
            .unwrap_commit();
        // The unnamed database lists the named ones: anything in it but elmdb's is another program's data
        let foreign = match main {
            Some(main) => {
                let txn = env.read_txn()?;
                main.iter(&txn)?.any(|e| e.is_ok_and(|(k, _)| k != format::DB_NAME.as_bytes()))
            }
            None => false,
        };
        if let Some(db) = db {
            check_stamp(env, db, path)?;
            return Ok(db);
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
                #[cfg(test)]
                if FAIL_STAMP_AT.lock().unwrap().as_deref() == Some(path) {
                    return Err(Error::new(ErrorCode::Io, "No space left on device"));
                }
                put_meta(wtxn, db, meta::KIND, format::KIND.as_bytes())?;
                put_meta(wtxn, db, meta::FORMAT, &format::FORMAT_VERSION.to_be_bytes())?;
                Ok(CommitOrAbort::<_, ()>::Commit(db))
            })
            .map_err(|e: Error| {
                Error::new(e.code, format!("Couldn't record the storage format of {}: {}", path.display(), e.message))
            })?
            .unwrap_commit();
        env.force_sync()?;
        Ok(db)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn gate(&self) -> RwLockReadGuard<'_, ()> {
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
                None => match self.env.clone().static_read_txn() {
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
    pub fn write<T>(&self, mut f: impl FnMut(&mut RwTxn<'_>, Db) -> Result<T>, durable: bool) -> Result<T> {
        self.check_writable()?;
        if self.unit_is_open() {
            return Err(Error::new(
                ErrorCode::UnitOpen,
                "A unit is open, holding the database's one write transaction: write through it, or commit it first",
            ));
        }
        let mut growths = 0;
        loop {
            let guard = self.gate();
            let mut txn = match self.env.write_txn() {
                Ok(txn) => txn,
                Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                    drop(guard);
                    self.adopt_map_size()?;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            let outcome = f(&mut txn, self.db).and_then(|t| self.commit_txn(txn).map(|()| t));
            drop(guard);
            match outcome {
                Ok(t) => {
                    self.committed(durable)?;
                    return Ok(t);
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

    /// Commits a write transaction, which a test may refuse as a full disk would (`FAIL_COMMIT_AT`)
    pub fn commit_txn(&self, txn: RwTxn<'_>) -> Result<()> {
        #[cfg(test)]
        if FAIL_COMMIT_AT.lock().unwrap().take_if(|p| *p == self.path).is_some() {
            drop(txn);
            return Err(Error::new(ErrorCode::Io, "No space left on device"));
        }
        txn.commit().map_err(Into::into)
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

    /// The environment's write transaction for a unit held open across calls, with the gate held for it. Refuses a
    /// second unit, and grows the map first when it is more than half used, since a unit can't grow it once open
    pub fn begin(&self) -> Result<(RwTxn<'_>, ArcRwLockReadGuard<RawRwLock, ()>)> {
        self.check_writable()?;
        {
            let mut open = self.unit.lock().expect("the unit's lock is never poisoned");
            if *open {
                return Err(Error::new(ErrorCode::UnitOpen, "A unit is already open on this database"));
            }
            *open = true;
        }
        let begun = (|| {
            let info = self.env.info();
            let page = self.env.unnamed_database_stat().page_size as usize;
            if (info.last_page_number + 1) * page * 2 > info.map_size {
                self.grow()?;
            }
            let guard = self.gate.read_arc();
            let txn = loop {
                match self.env.write_txn() {
                    Ok(txn) => break txn,
                    Err(heed::Error::Mdb(heed::MdbError::MapResized)) => self.adopt_map_size()?,
                    Err(e) => return Err(e.into()),
                }
            };
            Ok((txn, guard))
        })();
        if begun.is_err() {
            self.end_unit();
        }
        begun
    }

    /// The open unit ended, committed or not: whoever waits for the write transaction hears of it
    pub fn end_unit(&self) {
        *self.unit.lock().expect("the unit's lock is never poisoned") = false;
        self.unit_ended.notify_all();
    }

    fn unit_is_open(&self) -> bool {
        *self.unit.lock().expect("the unit's lock is never poisoned")
    }

    /// Blocks while a unit is open, until it ends or `stop` answers true; a caller that makes `stop` true wakes it
    /// with `wake_unit_waiters`
    pub fn wait_while_unit_open(&self, stop: impl Fn() -> bool) {
        let mut open = self.unit.lock().expect("the unit's lock is never poisoned");
        while *open && !stop() {
            open = self.unit_ended.wait(open).expect("the unit's lock is never poisoned");
        }
    }

    /// Wakes every `wait_while_unit_open`, to check its `stop` again
    pub fn wake_unit_waiters(&self) {
        let _open = self.unit.lock().expect("the unit's lock is never poisoned");
        self.unit_ended.notify_all();
    }

    pub fn db(&self) -> Db {
        self.db
    }

    /// Doubles the map, holding every transaction of this process off while it does
    pub fn grow(&self) -> Result<()> {
        let _gate = self.gate.write();
        self.drop_idle();
        let size = self.env.info().map_size;
        let grown = size.saturating_mul(2).div_ceil(MAP_STEP) * MAP_STEP;
        // SAFETY: the gate held for writing means no read or write runs in this process, and the idle read
        // transactions are dropped: no transaction is open, which LMDB requires
        unsafe { self.env.resize(grown) }?;
        Ok(())
    }

    /// Takes the map size another process grew the file to
    fn adopt_map_size(&self) -> Result<()> {
        let _gate = self.gate.write();
        self.drop_idle();
        // SAFETY: as in `grow`; a size of 0 adopts the size the file's meta records
        unsafe { self.env.resize(0) }?;
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

    /// LMDB's copy of one read transaction into `target/data.mdb`, compacted: without the pages deletes freed
    pub fn copy_to(&self, target: &Path) -> Result<()> {
        std::fs::create_dir_all(target)?;
        self.env.copy_to_path(target.join("data.mdb"), heed::CompactionOption::Enabled)?;
        Ok(())
    }

    /// Bytes the data file holds on disk
    pub fn file_size(&self) -> u64 {
        std::fs::metadata(self.path.join("data.mdb")).map(|m| m.len()).unwrap_or(0)
    }

    /// The bytes under a meta key
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

struct EnvSyncer(Env<WithoutTls>);

impl Syncer for EnvSyncer {
    fn sync(&self) -> Result<()> {
        self.0.force_sync().map_err(Into::into)
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

fn check_stamp(env: &Env<WithoutTls>, db: Db, path: &Path) -> Result<()> {
    let txn = env.read_txn()?;
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
                "The database at {} is in format {f}; this elmdb reads format {}. Upgrade a copy with `elmdb upgrade`",
                path.display(),
                format::FORMAT_VERSION
            ),
        )),
        None => Err(Error::new(ErrorCode::Corrupted, format!("The database at {} records no format", path.display()))),
    }
}

#[cfg(test)]
mod tests;
