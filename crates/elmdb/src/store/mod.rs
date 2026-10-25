//! The store: one LMDB environment, its one database (`format::DB_NAME`), the format stamp, reads, and writes with
//! group commit and map growth (`docs/engine.md`, Durability; `docs/format.md`).
//!
//! Every transaction holds `gate` for reading, and growing the map holds it for writing, since LMDB grows a map only
//! while its process has no transaction open. So `read` and `write` never call each other: a write reads through its
//! own transaction.

pub mod sync;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock, RwLockReadGuard};
use std::time::Duration;

use heed::types::Bytes;
use heed::{CommitOrAbort, Env, EnvFlags, EnvOpenOptions, RoTxn, RwTxn, WithoutTls};

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
    gate: RwLock<()>,
    commit: Option<GroupCommit>,
}

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
        let mut builder = EnvOpenOptions::new().read_txn_without_tls();
        builder.max_dbs(8).map_size(options.map_size_bytes.unwrap_or_else(|| initial_map_size(file_bytes)));
        // SAFETY: the flags change when LMDB writes and flushes, which the binding doesn't rely on: commits are
        // flushed by group commit's syncs, and a read-only environment is never written
        unsafe {
            builder.flags(if options.read_only { EnvFlags::READ_ONLY } else { EnvFlags::NO_SYNC });
        }
        // SAFETY: `open_paths` guarantees this process holds no other handle to the environment, and its files are
        // LMDB's, written only through LMDB
        let env = unsafe { builder.open(&canonical) }.map_err(|e| match e {
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
        Ok(Self { path: canonical, read_only: options.read_only, env, db, gate: RwLock::new(()), commit })
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
        self.gate.read().expect("the map gate is never poisoned")
    }

    /// Runs `f` in a read transaction, a snapshot of the last commit
    pub fn read<T>(&self, f: impl FnOnce(&RoTxn<'_, WithoutTls>, Db) -> Result<T>) -> Result<T> {
        let mut f = Some(f);
        loop {
            let guard = self.gate();
            match self.env.read_txn() {
                Ok(txn) => {
                    let result = (f.take().expect("a read runs once"))(&txn, self.db);
                    drop(txn);
                    drop(guard);
                    return result;
                }
                Err(heed::Error::Mdb(heed::MdbError::MapResized)) => {
                    drop(guard);
                    self.adopt_map_size()?;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Runs `f` in a write transaction and commits it, or aborts it when `f` fails. A write that fills the map is
    /// aborted, the map grown and `f` run again, so `f` must do nothing but write through its transaction
    pub fn write<T>(&self, mut f: impl FnMut(&mut RwTxn<'_>, Db) -> Result<T>, durable: bool) -> Result<T> {
        if self.read_only {
            return Err(Error::new(
                ErrorCode::ReadOnly,
                format!("The database at {} is open read-only", self.path.display()),
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
            let outcome = f(&mut txn, self.db).and_then(|t| txn.commit().map(|()| t).map_err(Error::from));
            drop(guard);
            match outcome {
                Ok(t) => {
                    if let Some(commit) = &self.commit {
                        commit.on_commit();
                        if durable {
                            commit.durable().wait()?;
                        }
                    }
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

    /// Doubles the map, holding every transaction of this process off while it does
    fn grow(&self) -> Result<()> {
        let _gate = self.gate.write().expect("the map gate is never poisoned");
        let size = self.env.info().map_size;
        let grown = size.saturating_mul(2).div_ceil(MAP_STEP) * MAP_STEP;
        // SAFETY: the gate held for writing means no transaction of this process is open, which LMDB requires
        unsafe { self.env.resize(grown) }?;
        Ok(())
    }

    /// Takes the map size another process grew the file to
    fn adopt_map_size(&self) -> Result<()> {
        let _gate = self.gate.write().expect("the map gate is never poisoned");
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
        // LMDB closes the environment once its last handle goes, which dropping the store is
        let closed = self.env.clone().prepare_for_closing();
        drop(self);
        closed.wait();
        synced
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
