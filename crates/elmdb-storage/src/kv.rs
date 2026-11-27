use crate::error::StorageError;

/// Flags controlling key ordering of a named sub-database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DbFlags {
    pub integer_key: bool,
}

impl DbFlags {
    pub const DEFAULT: Self = Self { integer_key: false };
    pub const INTEGER_KEY: Self = Self { integer_key: true };
}

/// Opaque handle to an open sub-database within a `KvStore`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubDbHandle(u32);

impl SubDbHandle {
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// Metadata returned when a write transaction commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommitReceipt {
    pub txnid: u64,
    pub roots_sha256: [u8; 32],
}

/// Zero-copy borrowed `(key, value)` pair returned by a cursor.
pub type KvEntry<'txn> = (&'txn [u8], &'txn [u8]);

/// Zero-copy ordered cursor over a sub-database within a transaction.
pub trait KvCursor<'txn> {
    fn first(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError>;
    fn last(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError>;
    fn seek_ge(&mut self, key: &[u8]) -> Result<Option<KvEntry<'txn>>, StorageError>;
    fn seek_exact(&mut self, key: &[u8]) -> Result<Option<&'txn [u8]>, StorageError>;
    fn next(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError>;
    fn prev(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError>;
}

/// Zero-copy read interface shared by read-only and read-write transactions.
pub trait KvRead {
    type Cursor<'a>: KvCursor<'a>
    where
        Self: 'a;

    fn get(&self, db: SubDbHandle, key: &[u8]) -> Result<Option<&[u8]>, StorageError>;
    fn cursor(&self, db: SubDbHandle) -> Result<Self::Cursor<'_>, StorageError>;
    fn entries(&self, db: SubDbHandle) -> Result<u64, StorageError>;
    /// Returns the MVCC snapshot transaction ID of this transaction (`mdb_txn_id` on LMDB).
    fn txnid(&self) -> u64;
    /// Opens an already-existing named sub-DB visible in this transaction (`Ok(None)` if absent).
    fn open_subdb_existing(&self, name: &str, flags: DbFlags) -> Result<Option<SubDbHandle>, StorageError>;
}

/// Write transaction interface with native LIFO nested savepoints and O(1) sub-DB truncate.
///
/// Higher layers should commit exclusively through `DurabilityGate::commit` rather than
/// invoking `commit_raw` directly.
pub trait KvWrite: KvRead + Sized {
    /// L2 `MapGate` read guard owned by this write transaction and returned back on commit.
    type MapReadGuard: 'static;

    fn open_subdb(&mut self, name: &str, flags: DbFlags) -> Result<SubDbHandle, StorageError>;
    fn put(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<(), StorageError>;
    /// Inserts `(key, val)` only if `key` does not already exist. Returns `true` if inserted.
    fn put_no_overwrite(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<bool, StorageError>;
    /// Deletes `key`. Returns `true` if the key existed.
    fn del(&mut self, db: SubDbHandle, key: &[u8]) -> Result<bool, StorageError>;
    /// Empties all entries from `db` without closing its `MDB_dbi` handle (`mdb_drop(..., 0)`).
    fn truncate(&mut self, db: SubDbHandle) -> Result<(), StorageError>;

    /// Opens a native nested savepoint and returns its 1-based depth.
    fn begin_savepoint(&mut self) -> Result<u32, StorageError>;
    /// Commits (merges into parent) the top-most savepoint at `depth`.
    fn commit_savepoint(&mut self, depth: u32) -> Result<(), StorageError>;
    /// Aborts (rolls back) the top-most savepoint at `depth`.
    fn abort_savepoint(&mut self, depth: u32) -> Result<(), StorageError>;
    fn savepoint_depth(&self) -> u32;

    /// Commits the root transaction, returning the commit receipt and the held L2 `MapReadGuard`.
    /// Must fail if any nested savepoint is still open.
    fn commit_raw(self) -> Result<(CommitReceipt, Self::MapReadGuard), StorageError>;
    fn abort(self);
}

/// Storage kernel abstraction implemented by `elmdb-lmdb` (production) and `elmdb-sim` (DST).
pub trait KvStore: Send + Sync {
    type RoTxn<'a>: KvRead
    where
        Self: 'a;
    type RwTxn<'a>: KvWrite<MapReadGuard = Self::MapReadGuard>
    where
        Self: 'a;
    type OwnedRwTxn: KvWrite<MapReadGuard = Self::MapReadGuard> + 'static;
    /// Guard token proving the caller holds L2 `MapGate::read()` for this environment.
    type MapReadGuard: 'static;

    fn begin_read(&self) -> Result<Self::RoTxn<'_>, StorageError>;
    fn begin_write(&self, map_guard: Self::MapReadGuard) -> Result<Self::RwTxn<'_>, StorageError>;
    fn begin_write_owned(&self, map_guard: Self::MapReadGuard) -> Result<Self::OwnedRwTxn, StorageError>;
    fn lock_map_read(&self) -> Self::MapReadGuard;
    fn open_subdb_ro(
        &self,
        txn: &Self::RoTxn<'_>,
        name: &str,
        flags: DbFlags,
    ) -> Result<Option<SubDbHandle>, StorageError>;
    /// Pre-opens any committed named sub-DBs into the environment handle cache in an isolated RO txn.
    /// Callers MUST pass an active `MapReadGuard` (L2) and hold `WriterLock` (L3).
    fn ensure_subdbs_open(
        &self,
        _map_guard: &Self::MapReadGuard,
        _names: &[(&str, DbFlags)],
    ) -> Result<(), StorageError> {
        Ok(())
    }
    /// Returns `Some(target_map_size)` when free headroom is below the proactive growth threshold.
    fn needs_map_growth(&self) -> Result<Option<usize>, StorageError> {
        Ok(None)
    }
    /// Grows the underlying mmap to at least `min_size` (or doubles current size) under L2 `MapGate::write()`.
    /// Caller MUST NOT hold any active transaction (`L2` or `L3`).
    fn grow_map(&self, _min_size: Option<usize>) -> Result<usize, StorageError> {
        Ok(self.map_size())
    }
    /// Adopts an external cross-process map resize (`mdb_env_set_mapsize(env, 0)`) under L2 `MapGate::write()`.
    /// Caller MUST NOT hold any active transaction (`L2` or `L3`).
    fn adopt_resized_map(&self) -> Result<usize, StorageError> {
        Ok(self.map_size())
    }
    fn sync(&self, map_guard: &Self::MapReadGuard) -> Result<(), StorageError>;
    fn last_txnid(&self) -> Result<u64, StorageError>;
    fn roots_sha256(&self) -> Result<[u8; 32], StorageError>;
    /// Atomically reads `(txnid, roots_sha256)` from a single active meta-page snapshot (no TOCTOU).
    fn active_receipt(&self) -> Result<CommitReceipt, StorageError> {
        Ok(CommitReceipt { txnid: self.last_txnid()?, roots_sha256: self.roots_sha256()? })
    }
    fn map_size(&self) -> usize;
    fn page_size(&self) -> usize;
    /// Writes a consistent compacted copy of the store into `target_dir` and returns the copied file's receipt.
    fn copy_compacted(&self, _target_dir: &std::path::Path) -> Result<CommitReceipt, StorageError> {
        Err(StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "copy_compacted is not supported by this storage backend",
        )))
    }
}

/// Executes `f` inside a native nested savepoint, committing on `Ok` and aborting on `Err`.
///
/// If `f` panics, the enclosing `RwTxn`'s `Drop` unwinds and aborts all open savepoints safely.
pub fn with_savepoint<W, T, E, F>(txn: &mut W, f: F) -> Result<T, E>
where
    W: KvWrite,
    E: From<StorageError>,
    F: FnOnce(&mut W) -> Result<T, E>,
{
    let depth = txn.begin_savepoint().map_err(E::from)?;
    match f(txn) {
        Ok(value) => {
            txn.commit_savepoint(depth).map_err(E::from)?;
            Ok(value)
        }
        Err(err) => {
            while txn.savepoint_depth() >= depth {
                let d = txn.savepoint_depth();
                let _ = txn.abort_savepoint(d);
            }
            Err(err)
        }
    }
}
