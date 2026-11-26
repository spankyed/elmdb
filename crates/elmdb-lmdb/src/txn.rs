use std::ffi::CString;
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::sync::Arc;

use elmdb_storage::{CommitReceipt, DbFlags, KvRead, KvWrite, StorageError, SubDbHandle};
use lmdb_master_sys as ffi;

use crate::cursor::{LmdbCursor, mdb_val_to_slice};
use crate::env::{LmdbEnvInner, LmdbStore};
use crate::error::map_mdb_err;
use crate::meta::read_active_meta_page;

const _: () = assert!(std::mem::size_of::<usize>() == 8, "MDB_INTEGERKEY requires 64-bit usize == 8 bytes");

/// Read-only MVCC LMDB transaction with zero-copy borrows.
pub struct LmdbRoTxn<'env> {
    inner: Arc<LmdbEnvInner>,
    txn: NonNull<ffi::MDB_txn>,
    _env: PhantomData<&'env LmdbStore>,
}

impl<'env> LmdbRoTxn<'env> {
    pub(crate) fn begin(store: &'env LmdbStore) -> Result<Self, StorageError> {
        let inner = Arc::clone(store.inner());
        let mut raw: *mut ffi::MDB_txn = ptr::null_mut();
        // SAFETY: `inner.env` is an open `MDB_env`; `MDB_RDONLY` opens a read-only transaction.
        let rc = unsafe { ffi::mdb_txn_begin(inner.env.as_ptr(), ptr::null_mut(), ffi::MDB_RDONLY, &mut raw) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, inner.map_size, inner.max_dbs));
        }
        let txn = NonNull::new(raw).ok_or_else(|| StorageError::InvalidState("null read MDB_txn".into()))?;
        Ok(Self { inner, txn, _env: PhantomData })
    }

    pub(crate) fn raw(&self) -> *mut ffi::MDB_txn {
        self.txn.as_ptr()
    }
}

impl Drop for LmdbRoTxn<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.txn` is owned exclusively by `LmdbRoTxn` and never invokes `mdb_dbi_open`,
        // so `mdb_txn_abort` cleanly releases only the reader slot.
        unsafe {
            ffi::mdb_txn_abort(self.txn.as_ptr());
        }
    }
}

impl KvRead for LmdbRoTxn<'_> {
    type Cursor<'a>
        = LmdbCursor<'a>
    where
        Self: 'a;

    fn get(&self, db: SubDbHandle, key: &[u8]) -> Result<Option<&[u8]>, StorageError> {
        raw_get(&self.inner, self.txn.as_ptr(), db, key)
    }

    fn cursor(&self, db: SubDbHandle) -> Result<Self::Cursor<'_>, StorageError> {
        raw_cursor(&self.inner, self.txn.as_ptr(), db)
    }

    fn entries(&self, db: SubDbHandle) -> Result<u64, StorageError> {
        raw_entries(&self.inner, self.txn.as_ptr(), db)
    }
}

/// Exclusive read-write LMDB transaction supporting native LIFO nested savepoints
/// without `transmute` to `'static`.
pub struct LmdbRwTxn<'env> {
    inner: Arc<LmdbEnvInner>,
    /// Index 0 is the root `MDB_txn`; indices `1..` are active nested savepoint child txns.
    /// Every operation targets `*stack.last().unwrap()`.
    stack: Vec<NonNull<ffi::MDB_txn>>,
    /// Newly created sub-DB handles staged until root `commit_raw` succeeds, preventing
    /// aborted transactions from polluting the shared environment DBI cache.
    pending_dbis: Vec<(String, ffi::MDB_dbi, DbFlags)>,
    _env: PhantomData<&'env LmdbStore>,
}

impl<'env> LmdbRwTxn<'env> {
    pub(crate) fn begin(store: &'env LmdbStore) -> Result<Self, StorageError> {
        let inner = Arc::clone(store.inner());
        let mut raw: *mut ffi::MDB_txn = ptr::null_mut();
        // SAFETY: `inner.env` is open; `parent = null` opens a root read-write transaction.
        let rc = unsafe { ffi::mdb_txn_begin(inner.env.as_ptr(), ptr::null_mut(), 0, &mut raw) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, inner.map_size, inner.max_dbs));
        }
        let txn = NonNull::new(raw).ok_or_else(|| StorageError::InvalidState("null write MDB_txn".into()))?;
        Ok(Self { inner, stack: vec![txn], pending_dbis: Vec::new(), _env: PhantomData })
    }

    fn active_raw(&self) -> *mut ffi::MDB_txn {
        self.stack.last().expect("write txn active").as_ptr()
    }

    fn lookup_flags(&self, db: SubDbHandle) -> Result<DbFlags, StorageError> {
        if let Some((_, _, flags)) = self.pending_dbis.iter().find(|(_, dbi, _)| *dbi == db.raw()) {
            return Ok(*flags);
        }
        lookup_flags(&self.inner, db)
    }
}

impl Drop for LmdbRwTxn<'_> {
    fn drop(&mut self) {
        // Abort nested savepoints strictly LIFO before aborting the root transaction.
        for txn in self.stack.drain(..).rev() {
            // SAFETY: Each `MDB_txn` in `stack` is live and aborted in strict child-before-parent order.
            unsafe {
                ffi::mdb_txn_abort(txn.as_ptr());
            }
        }
    }
}

impl KvRead for LmdbRwTxn<'_> {
    type Cursor<'a>
        = LmdbCursor<'a>
    where
        Self: 'a;

    fn get(&self, db: SubDbHandle, key: &[u8]) -> Result<Option<&[u8]>, StorageError> {
        let flags = self.lookup_flags(db)?;
        raw_get_with_flags(&self.inner, self.active_raw(), db, flags, key)
    }

    fn cursor(&self, db: SubDbHandle) -> Result<Self::Cursor<'_>, StorageError> {
        let flags = self.lookup_flags(db)?;
        // SAFETY: `&self` guarantees `active_raw()` stays valid and unmutated for cursor lifetime.
        unsafe { LmdbCursor::open(self.active_raw(), db.raw(), flags, self.inner.map_size, self.inner.max_dbs) }
    }

    fn entries(&self, db: SubDbHandle) -> Result<u64, StorageError> {
        let _ = self.lookup_flags(db)?;
        raw_entries_unchecked(&self.inner, self.active_raw(), db)
    }
}

impl KvWrite for LmdbRwTxn<'_> {
    fn open_subdb(&mut self, name: &str, flags: DbFlags) -> Result<SubDbHandle, StorageError> {
        validate_subdb_name(name)?;
        if let Some((_, dbi, pending_flags)) = self.pending_dbis.iter().find(|(n, _, _)| n == name) {
            if *pending_flags != flags {
                return Err(StorageError::InvalidState(format!("sub-DB '{name}' reopened with mismatched flags")));
            }
            return Ok(SubDbHandle::from_raw(*dbi));
        }
        if let Some(&(dbi, cached_flags)) = self.inner.dbis.lock().by_name.get(name) {
            if cached_flags != flags {
                return Err(StorageError::InvalidState(format!("sub-DB '{name}' reopened with mismatched flags")));
            }
            return Ok(SubDbHandle::from_raw(dbi));
        }
        if self.savepoint_depth() != 0 {
            return Err(StorageError::InvalidState(
                "creating a new sub-DB inside a nested savepoint is forbidden; open at root write txn".into(),
            ));
        }

        let cname =
            CString::new(name).map_err(|_| StorageError::InvalidState("sub-DB name contains interior NUL".into()))?;
        let mut mdb_flags = ffi::MDB_CREATE;
        if flags.integer_key {
            mdb_flags |= ffi::MDB_INTEGERKEY;
        }
        let mut dbi: ffi::MDB_dbi = 0;
        // SAFETY: `active_raw()` is the serialized root write `MDB_txn`, `cname` is NUL-terminated.
        let rc = unsafe { ffi::mdb_dbi_open(self.active_raw(), cname.as_ptr(), mdb_flags, &mut dbi) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        // Verify on-disk flags against caller's requested flags so reopening an existing
        // `MDB_INTEGERKEY` sub-DB with `DbFlags::DEFAULT` (or vice versa) never bypasses key validation.
        let mut actual_raw_flags: libc::c_uint = 0;
        // SAFETY: `active_raw()` and `dbi` are live.
        let rc = unsafe { ffi::mdb_dbi_flags(self.active_raw(), dbi, &mut actual_raw_flags) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        let actual_integer_key = (actual_raw_flags & ffi::MDB_INTEGERKEY) != 0;
        if actual_integer_key != flags.integer_key {
            return Err(StorageError::InvalidState(format!(
                "sub-DB '{name}' on-disk MDB_INTEGERKEY={actual_integer_key} mismatches requested {}",
                flags.integer_key
            )));
        }

        self.pending_dbis.push((name.to_owned(), dbi, flags));
        Ok(SubDbHandle::from_raw(dbi))
    }

    fn put(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<(), StorageError> {
        let flags = self.lookup_flags(db)?;
        validate_key(flags, key)?;
        let mut k = to_mdb_val(key);
        let mut v = to_mdb_val(val);
        // SAFETY: `active_raw()` is a live write `MDB_txn`; `k` and `v` have valid non-null pointers.
        let rc = unsafe { ffi::mdb_put(self.active_raw(), db.raw(), &mut k, &mut v, 0) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        Ok(())
    }

    fn put_no_overwrite(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<bool, StorageError> {
        let flags = self.lookup_flags(db)?;
        validate_key(flags, key)?;
        let mut k = to_mdb_val(key);
        let mut v = to_mdb_val(val);
        // SAFETY: `active_raw()` is a live write `MDB_txn`; `MDB_NOOVERWRITE` does not modify existing values.
        let rc = unsafe { ffi::mdb_put(self.active_raw(), db.raw(), &mut k, &mut v, ffi::MDB_NOOVERWRITE) };
        match rc {
            ffi::MDB_SUCCESS => Ok(true),
            ffi::MDB_KEYEXIST => Ok(false),
            other => Err(map_mdb_err(other, self.inner.map_size, self.inner.max_dbs)),
        }
    }

    fn del(&mut self, db: SubDbHandle, key: &[u8]) -> Result<bool, StorageError> {
        let flags = self.lookup_flags(db)?;
        validate_key(flags, key)?;
        let mut k = to_mdb_val(key);
        // SAFETY: `active_raw()` is a live write `MDB_txn`; null data pointer deletes by key.
        let rc = unsafe { ffi::mdb_del(self.active_raw(), db.raw(), &mut k, ptr::null_mut()) };
        match rc {
            ffi::MDB_SUCCESS => Ok(true),
            ffi::MDB_NOTFOUND => Ok(false),
            other => Err(map_mdb_err(other, self.inner.map_size, self.inner.max_dbs)),
        }
    }

    fn truncate(&mut self, db: SubDbHandle) -> Result<(), StorageError> {
        let _ = self.lookup_flags(db)?;
        // SAFETY: `del = 0` empties the sub-DB without closing the `MDB_dbi` handle,
        // preserving MVCC safety for all concurrent read transactions.
        let rc = unsafe { ffi::mdb_drop(self.active_raw(), db.raw(), 0) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        Ok(())
    }

    fn begin_savepoint(&mut self) -> Result<u32, StorageError> {
        let parent = self.active_raw();
        let mut child: *mut ffi::MDB_txn = ptr::null_mut();
        // SAFETY: `self.inner.env` is open and `parent` is the currently active write `MDB_txn`.
        let rc = unsafe { ffi::mdb_txn_begin(self.inner.env.as_ptr(), parent, 0, &mut child) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        let child = NonNull::new(child).ok_or_else(|| StorageError::InvalidState("null child MDB_txn".into()))?;
        self.stack.push(child);
        Ok(u32::try_from(self.stack.len() - 1).expect("savepoint depth fits u32"))
    }

    fn commit_savepoint(&mut self, depth: u32) -> Result<(), StorageError> {
        let actual = self.savepoint_depth();
        if actual == 0 || depth != actual {
            return Err(StorageError::SavepointMismatch { expected: depth, actual });
        }
        let child = self.stack.pop().expect("child present");
        // SAFETY: `child` is the top-most live nested savepoint; `mdb_txn_commit` merges it into
        // its parent and frees `child` regardless of return code.
        let rc = unsafe { ffi::mdb_txn_commit(child.as_ptr()) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        Ok(())
    }

    fn abort_savepoint(&mut self, depth: u32) -> Result<(), StorageError> {
        let actual = self.savepoint_depth();
        if actual == 0 || depth != actual {
            return Err(StorageError::SavepointMismatch { expected: depth, actual });
        }
        let child = self.stack.pop().expect("child present");
        // SAFETY: `child` is the top-most live nested savepoint; `mdb_txn_abort` discards it.
        unsafe {
            ffi::mdb_txn_abort(child.as_ptr());
        }
        Ok(())
    }

    fn savepoint_depth(&self) -> u32 {
        u32::try_from(self.stack.len().saturating_sub(1)).expect("depth fits u32")
    }

    fn commit_raw(mut self) -> Result<CommitReceipt, StorageError> {
        let depth = self.savepoint_depth();
        if depth != 0 {
            return Err(StorageError::SavepointMismatch { expected: 0, actual: depth });
        }
        let root = self.stack.pop().expect("root txn present");
        if self.pending_dbis.is_empty() {
            // SAFETY: `root` is the sole remaining live `MDB_txn`; `mdb_txn_commit` consumes it.
            let rc = unsafe { ffi::mdb_txn_commit(root.as_ptr()) };
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
            }
        } else {
            // Acquire L4 (`dbis`) *before* `mdb_txn_commit` (strict L3 -> L4 order) so no concurrent
            // reader observing the newly committed snapshot can miss the newly created DBI handle!
            let mut cache = self.inner.dbis.lock();
            // SAFETY: `root` is the sole remaining live `MDB_txn`; `mdb_txn_commit` consumes it.
            let rc = unsafe { ffi::mdb_txn_commit(root.as_ptr()) };
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
            }
            for (name, dbi, flags) in self.pending_dbis.drain(..) {
                cache.by_name.insert(name, (dbi, flags));
                cache.by_id.insert(dbi, flags);
            }
        }
        let meta = read_active_meta_page(&self.inner.data_mdb)?;
        Ok(CommitReceipt { txnid: meta.txnid, roots_sha256: meta.roots_sha256 })
    }

    fn abort(self) {
        drop(self);
    }
}

pub(crate) fn validate_subdb_name(name: &str) -> Result<(), StorageError> {
    if name.is_empty() || name.len() > 511 {
        return Err(StorageError::BadValSize { len: name.len() });
    }
    if name.as_bytes().contains(&0) {
        return Err(StorageError::InvalidState("sub-DB name contains interior NUL".into()));
    }
    Ok(())
}

pub(crate) fn validate_key(flags: DbFlags, key: &[u8]) -> Result<(), StorageError> {
    if flags.integer_key {
        // Fixed 8-byte native `u64` only: prevents LMDB `mdb_cmp_cint` OOB reads when mixing key widths.
        if key.len() != 8 {
            return Err(StorageError::BadValSize { len: key.len() });
        }
    } else if key.is_empty() || key.len() > 511 {
        return Err(StorageError::BadValSize { len: key.len() });
    }
    Ok(())
}

fn lookup_flags(inner: &LmdbEnvInner, db: SubDbHandle) -> Result<DbFlags, StorageError> {
    inner.dbis.lock().by_id.get(&db.raw()).copied().ok_or(StorageError::InvalidSubDb { handle: db.raw() })
}

fn to_mdb_val(bytes: &[u8]) -> ffi::MDB_val {
    // Always pass a non-null pointer (Rust empty slices carry an aligned dangling pointer)
    // because LMDB invokes `memcpy(dst, mv_data, 0)` on empty values, where NULL is C UB.
    ffi::MDB_val { mv_size: bytes.len(), mv_data: bytes.as_ptr() as *mut libc::c_void }
}

fn raw_get<'a>(
    inner: &LmdbEnvInner,
    txn: *mut ffi::MDB_txn,
    db: SubDbHandle,
    key: &[u8],
) -> Result<Option<&'a [u8]>, StorageError> {
    let flags = lookup_flags(inner, db)?;
    raw_get_with_flags(inner, txn, db, flags, key)
}

fn raw_get_with_flags<'a>(
    inner: &LmdbEnvInner,
    txn: *mut ffi::MDB_txn,
    db: SubDbHandle,
    flags: DbFlags,
    key: &[u8],
) -> Result<Option<&'a [u8]>, StorageError> {
    validate_key(flags, key)?;
    let mut k = to_mdb_val(key);
    let mut v = ffi::MDB_val { mv_size: 0, mv_data: ptr::null_mut() };
    // SAFETY: `txn` is live for the caller's borrow `'a`, `k` points to readable bytes, and
    // `v` receives a pointer into LMDB's MVCC pages valid until `txn` mutates or ends.
    let rc = unsafe { ffi::mdb_get(txn, db.raw(), &mut k, &mut v) };
    match rc {
        ffi::MDB_SUCCESS => {
            // SAFETY: `mdb_get` succeeded and `v` stays valid for `'a`.
            Ok(Some(unsafe { mdb_val_to_slice(&v) }))
        }
        ffi::MDB_NOTFOUND => Ok(None),
        other => Err(map_mdb_err(other, inner.map_size, inner.max_dbs)),
    }
}

fn raw_cursor<'a>(
    inner: &LmdbEnvInner,
    txn: *mut ffi::MDB_txn,
    db: SubDbHandle,
) -> Result<LmdbCursor<'a>, StorageError> {
    let flags = lookup_flags(inner, db)?;
    // SAFETY: Caller's `&'a self` borrow guarantees `txn` stays valid and unmutated for `'a`.
    unsafe { LmdbCursor::open(txn, db.raw(), flags, inner.map_size, inner.max_dbs) }
}

fn raw_entries(inner: &LmdbEnvInner, txn: *mut ffi::MDB_txn, db: SubDbHandle) -> Result<u64, StorageError> {
    let _ = lookup_flags(inner, db)?;
    raw_entries_unchecked(inner, txn, db)
}

fn raw_entries_unchecked(inner: &LmdbEnvInner, txn: *mut ffi::MDB_txn, db: SubDbHandle) -> Result<u64, StorageError> {
    let mut stat = std::mem::MaybeUninit::<ffi::MDB_stat>::uninit();
    // SAFETY: `txn` is live; `stat` is a valid out-pointer.
    let rc = unsafe { ffi::mdb_stat(txn, db.raw(), stat.as_mut_ptr()) };
    if rc != ffi::MDB_SUCCESS {
        return Err(map_mdb_err(rc, inner.map_size, inner.max_dbs));
    }
    // SAFETY: `mdb_stat` succeeded and initialized `stat`.
    Ok(unsafe { stat.assume_init() }.ms_entries as u64)
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};

    use elmdb_storage::KvStore;
    use tempfile::TempDir;

    use super::*;
    use crate::LmdbConfig;
    use crate::meta::read_active_meta_page;

    #[test]
    fn meta_page_offsets_match_live_lmdb_env_info_and_survive_corrupted_page0() {
        let tmp = TempDir::new().unwrap();
        let store =
            LmdbStore::open(tmp.path(), LmdbConfig { map_size: 4 * 1024 * 1024, ..LmdbConfig::default() }).unwrap();

        let mut rw = store.begin_write().unwrap();
        let db = rw.open_subdb("meta", DbFlags::DEFAULT).unwrap();
        rw.put(db, b"hello", b"world").unwrap();
        let receipt = rw.commit_raw().unwrap();

        let meta = read_active_meta_page(store.data_mdb_path()).unwrap();
        assert_eq!(meta.txnid, store.last_txnid().unwrap());
        assert_eq!(meta.txnid, receipt.txnid);
        assert_eq!(meta.page_size as usize, store.page_size());
        assert_eq!(meta.roots_sha256, receipt.roots_sha256);

        // Write a second commit so both meta pages 0 and 1 are populated, then corrupt page 0
        // and verify `read_active_meta_page` still locates page 1 even on 16 KiB Apple Silicon!
        let mut rw = store.begin_write().unwrap();
        rw.put(db, b"hello2", b"world2").unwrap();
        rw.commit_raw().unwrap();
        let data_path = store.data_mdb_path().to_path_buf();
        drop(store);

        let mut file = OpenOptions::new().write(true).open(&data_path).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[0xFF; 64]).unwrap();
        drop(file);

        let recovered = read_active_meta_page(&data_path).unwrap();
        assert_eq!(recovered.slot, 1);
    }
}
