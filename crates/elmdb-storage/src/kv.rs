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
}

/// Write transaction interface with native LIFO nested savepoints and O(1) sub-DB truncate.
///
/// Higher layers should commit exclusively through `DurabilityGate::commit` rather than
/// invoking `commit_raw` directly.
pub trait KvWrite: KvRead + Sized {
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

    /// Commits the root transaction. Must fail if any nested savepoint is still open.
    fn commit_raw(self) -> Result<CommitReceipt, StorageError>;
    fn abort(self);
}

/// Storage kernel abstraction implemented by `elmdb-lmdb` (production) and `elmdb-sim` (DST).
pub trait KvStore: Send + Sync {
    type RoTxn<'a>: KvRead
    where
        Self: 'a;
    type RwTxn<'a>: KvWrite
    where
        Self: 'a;

    fn begin_read(&self) -> Result<Self::RoTxn<'_>, StorageError>;
    fn begin_write(&self) -> Result<Self::RwTxn<'_>, StorageError>;
    fn open_subdb_ro(
        &self,
        txn: &Self::RoTxn<'_>,
        name: &str,
        flags: DbFlags,
    ) -> Result<Option<SubDbHandle>, StorageError>;
    fn sync(&self) -> Result<(), StorageError>;
    fn last_txnid(&self) -> Result<u64, StorageError>;
    fn roots_sha256(&self) -> Result<[u8; 32], StorageError>;
    fn map_size(&self) -> usize;
    fn page_size(&self) -> usize;
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
