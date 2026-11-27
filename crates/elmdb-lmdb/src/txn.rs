use std::ffi::CString;
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use elmdb_storage::{CommitReceipt, DbFlags, KvRead, KvWrite, OrderedArcReadGuard, StorageError, SubDbHandle};
use lmdb_master_sys as ffi;

use crate::cursor::{LmdbCursor, mdb_val_to_slice};
use crate::env::{LmdbEnvInner, LmdbMapReadGuard, LmdbStore};
use crate::error::map_mdb_err;

const _: () = assert!(std::mem::size_of::<usize>() == 8, "MDB_INTEGERKEY requires 64-bit usize == 8 bytes");

/// Read-only MVCC LMDB transaction with zero-copy borrows.
///
/// Field drop order ensures `mdb_txn_abort` runs in `Drop::drop` BEFORE `_map_guard` releases L2 `MapGate`.
pub struct LmdbRoTxn<'env> {
    txn: NonNull<ffi::MDB_txn>,
    _map_guard: OrderedArcReadGuard<()>,
    inner: Arc<LmdbEnvInner>,
    _env: PhantomData<&'env LmdbStore>,
}

impl<'env> LmdbRoTxn<'env> {
    pub(crate) fn begin(store: &'env LmdbStore) -> Result<Self, StorageError> {
        let inner = Arc::clone(store.inner());
        let env_guard = inner.lock_env_read()?;
        let mut raw: *mut ffi::MDB_txn = ptr::null_mut();
        // SAFETY: `inner.env` is open and protected by `EnvReadGuard` (L2 `map_gate` + L4
        // `dbi_publish_gate.read_txn()`), preventing `mdb_txn_renew0` from racing `LmdbRwTxn::commit_raw()`
        // or `ensure_subdbs_open()`.
        let mut rc = unsafe { ffi::mdb_txn_begin(inner.env.as_ptr(), ptr::null_mut(), ffi::MDB_RDONLY, &mut raw) };
        if rc == ffi::MDB_READERS_FULL {
            let mut dead: libc::c_int = 0;
            // SAFETY: Reclaim stale reader slots from crashed processes and retry once.
            let _ = unsafe { ffi::mdb_reader_check(inner.env.as_ptr(), &mut dead) };
            // SAFETY: `inner.env` is open and `env_guard` is still held; retrying `MDB_RDONLY` txn.
            rc = unsafe { ffi::mdb_txn_begin(inner.env.as_ptr(), ptr::null_mut(), ffi::MDB_RDONLY, &mut raw) };
        }
        let map_guard = env_guard.into_map_guard();
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, inner.current_map_size(), inner.max_dbs));
        }
        let txn = NonNull::new(raw).ok_or_else(|| StorageError::InvalidState("null read MDB_txn".into()))?;
        Ok(Self { txn, _map_guard: map_guard, inner, _env: PhantomData })
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

    fn txnid(&self) -> u64 {
        // SAFETY: `self.txn` is a live `MDB_txn` owned exclusively by `self`.
        unsafe { ffi::mdb_txn_id(self.txn.as_ptr()) as u64 }
    }

    fn open_subdb_existing(&self, name: &str, flags: DbFlags) -> Result<Option<SubDbHandle>, StorageError> {
        open_subdb_existing_raw(&self.inner, self.txn.as_ptr(), &[], name, flags)
    }
}

/// Exclusive read-write LMDB transaction supporting native LIFO nested savepoints
/// without `transmute` to `'static`.
///
/// Field drop order ensures:
/// 1. `Drop::drop` aborts all `MDB_txn` handles and decrements `active_rw_txns`,
/// 2. `map_guard` releases L2 `MapGate`,
/// 3. `inner` drops the `Arc<LmdbEnvInner>` last (matching `LmdbRoTxn`).
pub struct LmdbRwTxn<'env> {
    /// Index 0 is the root `MDB_txn`; indices `1..` are active nested savepoint child txns.
    /// Every operation targets `*stack.last().unwrap()`.
    stack: Vec<NonNull<ffi::MDB_txn>>,
    /// Newly created sub-DB handles staged until root `commit_raw` succeeds, preventing
    /// aborted transactions from polluting the shared environment DBI cache.
    pending_dbis: Vec<(String, ffi::MDB_dbi, DbFlags)>,
    /// Whether this transaction currently holds an `inner.active_rw_txns` count.
    counted_active_rw: bool,
    /// L2 `MapGate` read guard proving the env mmap cannot be unmapped while this txn is live.
    map_guard: Option<LmdbMapReadGuard>,
    inner: Arc<LmdbEnvInner>,
    _env: PhantomData<&'env LmdbStore>,
}

impl<'env> LmdbRwTxn<'env> {
    pub(crate) fn begin(store: &'env LmdbStore, map_guard: LmdbMapReadGuard) -> Result<Self, StorageError> {
        Self::begin_from_inner(Arc::clone(store.inner()), map_guard)
    }

    pub(crate) fn begin_owned(
        store: &LmdbStore,
        map_guard: LmdbMapReadGuard,
    ) -> Result<LmdbRwTxn<'static>, StorageError> {
        LmdbRwTxn::<'static>::begin_from_inner(Arc::clone(store.inner()), map_guard)
    }

    fn begin_from_inner(inner: Arc<LmdbEnvInner>, map_guard: LmdbMapReadGuard) -> Result<Self, StorageError> {
        inner.assert_map_guard(&map_guard)?;
        inner.check_map_ok()?;
        // Briefly acquire L4 `dbis.write()` (valid L2 -> L3 -> L4 order) to wait for any in-flight
        // `ensure_subdbs_open()` and mark `active_rw_txns` before releasing L4 so readers are never
        // blocked while waiting on LMDB's writer lock in `mdb_txn_begin(..., 0)`.
        {
            let _dbi_guard = inner.dbis.write();
            inner.active_rw_txns.fetch_add(1, Ordering::AcqRel);
        }
        let mut raw: *mut ffi::MDB_txn = ptr::null_mut();
        // SAFETY: `inner.env` is open and protected by L2 `map_guard`; `parent = null` opens a root RW txn.
        let rc = unsafe { ffi::mdb_txn_begin(inner.env.as_ptr(), ptr::null_mut(), 0, &mut raw) };
        if rc != ffi::MDB_SUCCESS {
            inner.active_rw_txns.fetch_sub(1, Ordering::Release);
            return Err(map_mdb_err(rc, inner.current_map_size(), inner.max_dbs));
        }
        let Some(txn) = NonNull::new(raw) else {
            inner.active_rw_txns.fetch_sub(1, Ordering::Release);
            return Err(StorageError::InvalidState("null write MDB_txn".into()));
        };
        Ok(Self {
            stack: vec![txn],
            pending_dbis: Vec::new(),
            counted_active_rw: true,
            map_guard: Some(map_guard),
            inner,
            _env: PhantomData,
        })
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
        if self.counted_active_rw {
            self.inner.active_rw_txns.fetch_sub(1, Ordering::Release);
            self.counted_active_rw = false;
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
        unsafe {
            LmdbCursor::open(self.active_raw(), db.raw(), flags, self.inner.current_map_size(), self.inner.max_dbs)
        }
    }

    fn entries(&self, db: SubDbHandle) -> Result<u64, StorageError> {
        let _ = self.lookup_flags(db)?;
        raw_entries_unchecked(&self.inner, self.active_raw(), db)
    }

    fn txnid(&self) -> u64 {
        // SAFETY: `active_raw()` is a live write `MDB_txn`.
        unsafe { ffi::mdb_txn_id(self.active_raw()) as u64 }
    }

    fn open_subdb_existing(&self, name: &str, flags: DbFlags) -> Result<Option<SubDbHandle>, StorageError> {
        open_subdb_existing_raw(&self.inner, self.active_raw(), &self.pending_dbis, name, flags)
    }
}

pub(crate) fn open_subdb_existing_raw(
    inner: &LmdbEnvInner,
    raw_txn: *mut ffi::MDB_txn,
    pending_dbis: &[(String, ffi::MDB_dbi, DbFlags)],
    name: &str,
    flags: DbFlags,
) -> Result<Option<SubDbHandle>, StorageError> {
    validate_subdb_name(name)?;
    if let Some((_, dbi, pending_flags)) = pending_dbis.iter().find(|(n, _, _)| n == name) {
        if *pending_flags != flags {
            return Err(StorageError::InvalidState(format!("sub-DB '{name}' opened read-only with mismatched flags")));
        }
        return Ok(Some(SubDbHandle::from_raw(*dbi)));
    }

    // Check exact MVCC visibility in `raw_txn`'s snapshot via `MAIN_DBI` before touching DBI handles.
    let mut k = ffi::MDB_val { mv_size: name.len(), mv_data: name.as_ptr() as *mut libc::c_void };
    let mut v = ffi::MDB_val { mv_size: 0, mv_data: ptr::null_mut() };
    // SAFETY: `raw_txn` and `inner.main_dbi` are live; `k` points to readable UTF-8 bytes.
    let rc = unsafe { ffi::mdb_get(raw_txn, inner.main_dbi, &mut k, &mut v) };
    match rc {
        ffi::MDB_SUCCESS => {}
        ffi::MDB_NOTFOUND => return Ok(None),
        other => return Err(map_mdb_err(other, inner.current_map_size(), inner.max_dbs)),
    }

    let cache = inner.dbis.read();
    if let Some(&(dbi, cached_flags)) = cache.by_name.get(name) {
        if cached_flags != flags {
            return Err(StorageError::InvalidState(format!("sub-DB '{name}' opened read-only with mismatched flags")));
        }
        return Ok(Some(SubDbHandle::from_raw(dbi)));
    }

    // Never call `mdb_dbi_open` on a side RO txn here: (1) it races with an active write txn's
    // `mdb_dbi_open`, and (2) `raw_txn`'s `mt_numdbs` table would lack the new handle (`EINVAL`).
    // Callers discovering a cross-process sub-DB must call `DurabilityGate::ensure_subdbs_open`
    // BEFORE beginning `raw_txn`.
    Err(StorageError::InvalidState(format!(
        "sub-DB '{name}' exists on disk but is not pre-opened in this environment; call ensure_subdbs_open() before beginning a transaction"
    )))
}

impl KvWrite for LmdbRwTxn<'_> {
    type MapReadGuard = LmdbMapReadGuard;

    fn open_subdb(&mut self, name: &str, flags: DbFlags) -> Result<SubDbHandle, StorageError> {
        validate_subdb_name(name)?;
        if let Some((_, dbi, pending_flags)) = self.pending_dbis.iter().find(|(n, _, _)| n == name) {
            if *pending_flags != flags {
                return Err(StorageError::InvalidState(format!("sub-DB '{name}' reopened with mismatched flags")));
            }
            return Ok(SubDbHandle::from_raw(*dbi));
        }
        if let Some(&(dbi, cached_flags)) = self.inner.dbis.read().by_name.get(name) {
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
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        // Verify on-disk flags against caller's requested flags so reopening an existing
        // `MDB_INTEGERKEY` sub-DB with `DbFlags::DEFAULT` (or vice versa) never bypasses key validation.
        let mut actual_raw_flags: libc::c_uint = 0;
        // SAFETY: `active_raw()` and `dbi` are live.
        let rc = unsafe { ffi::mdb_dbi_flags(self.active_raw(), dbi, &mut actual_raw_flags) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        let actual_flags = DbFlags { integer_key: (actual_raw_flags & ffi::MDB_INTEGERKEY) != 0 };
        // Always stage the actual on-disk handle in `pending_dbis` so even if a direct caller
        // ignores the flag-mismatch error and commits, `inner.dbis` stays in sync with `env->me_dbs`.
        self.pending_dbis.push((name.to_owned(), dbi, actual_flags));
        if actual_flags != flags {
            return Err(StorageError::InvalidState(format!(
                "sub-DB '{name}' on-disk MDB_INTEGERKEY={} mismatches requested {}",
                actual_flags.integer_key, flags.integer_key
            )));
        }

        Ok(SubDbHandle::from_raw(dbi))
    }

    fn put(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<(), StorageError> {
        let flags = self.lookup_flags(db)?;
        let key = MdbKey::new(flags, key)?;
        let mut k = key.as_mdb_val();
        let mut v = to_mdb_val(val);
        // SAFETY: `active_raw()` is a live write `MDB_txn`; `key` outlives `k` and `v` has a valid non-null pointer.
        let rc = unsafe { ffi::mdb_put(self.active_raw(), db.raw(), &mut k, &mut v, 0) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        Ok(())
    }

    fn put_no_overwrite(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<bool, StorageError> {
        let flags = self.lookup_flags(db)?;
        let key = MdbKey::new(flags, key)?;
        let mut k = key.as_mdb_val();
        let mut v = to_mdb_val(val);
        // SAFETY: `active_raw()` is a live write `MDB_txn`; `key` outlives `k`, `MDB_NOOVERWRITE` does not modify existing values.
        let rc = unsafe { ffi::mdb_put(self.active_raw(), db.raw(), &mut k, &mut v, ffi::MDB_NOOVERWRITE) };
        match rc {
            ffi::MDB_SUCCESS => Ok(true),
            ffi::MDB_KEYEXIST => Ok(false),
            other => Err(map_mdb_err(other, self.inner.current_map_size(), self.inner.max_dbs)),
        }
    }

    fn del(&mut self, db: SubDbHandle, key: &[u8]) -> Result<bool, StorageError> {
        let flags = self.lookup_flags(db)?;
        let key = MdbKey::new(flags, key)?;
        let mut k = key.as_mdb_val();
        // SAFETY: `active_raw()` is a live write `MDB_txn`; `key` outlives `k`, null data pointer deletes by key.
        let rc = unsafe { ffi::mdb_del(self.active_raw(), db.raw(), &mut k, ptr::null_mut()) };
        match rc {
            ffi::MDB_SUCCESS => Ok(true),
            ffi::MDB_NOTFOUND => Ok(false),
            other => Err(map_mdb_err(other, self.inner.current_map_size(), self.inner.max_dbs)),
        }
    }

    fn truncate(&mut self, db: SubDbHandle) -> Result<(), StorageError> {
        let _ = self.lookup_flags(db)?;
        // SAFETY: `del = 0` empties the sub-DB without closing the `MDB_dbi` handle,
        // preserving MVCC safety for all concurrent read transactions.
        let rc = unsafe { ffi::mdb_drop(self.active_raw(), db.raw(), 0) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        Ok(())
    }

    fn begin_savepoint(&mut self) -> Result<u32, StorageError> {
        let parent = self.active_raw();
        let mut child: *mut ffi::MDB_txn = ptr::null_mut();
        // SAFETY: `self.inner.env` is open and `parent` is the currently active write `MDB_txn`.
        let rc = unsafe { ffi::mdb_txn_begin(self.inner.env.as_ptr(), parent, 0, &mut child) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
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
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
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

    fn commit_raw(mut self) -> Result<(CommitReceipt, Self::MapReadGuard), StorageError> {
        let depth = self.savepoint_depth();
        if depth != 0 {
            return Err(StorageError::SavepointMismatch { expected: 0, actual: depth });
        }
        let root = self.stack.pop().expect("root txn present");
        if self.pending_dbis.is_empty() {
            // SAFETY: `root` is the sole remaining live `MDB_txn`; `mdb_txn_commit` consumes it.
            let rc = unsafe { ffi::mdb_txn_commit(root.as_ptr()) };
            if self.counted_active_rw {
                self.inner.active_rw_txns.fetch_sub(1, Ordering::Release);
                self.counted_active_rw = false;
            }
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
            }
        } else {
            // Acquire L4 `dbi_publish_gate.write()` -> L5 `dbis.write()` *before* `mdb_txn_commit`
            // (strict L2 -> L3 -> L4 -> L5 order) so no concurrent `LmdbRoTxn::begin()` or `copy_compacted()`
            // can observe the newly committed `MAIN_DBI` snapshot with stale `env->me_numdbs`/`env->me_dbflags`
            // or miss the new handle in `dbis`!
            let _publish = self.inner.dbi_publish_gate.write();
            let mut cache = self.inner.dbis.write();
            // SAFETY: `root` is the sole remaining live `MDB_txn`; `mdb_txn_commit` consumes it.
            let rc = unsafe { ffi::mdb_txn_commit(root.as_ptr()) };
            if self.counted_active_rw {
                self.inner.active_rw_txns.fetch_sub(1, Ordering::Release);
                self.counted_active_rw = false;
            }
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
            }
            for (name, dbi, flags) in self.pending_dbis.drain(..) {
                cache.by_name.insert(name, (dbi, flags));
                cache.by_id.insert(dbi, flags);
            }
        }
        // Read committed `me_last_txnid` from in-memory `MDB_envinfo` while L2 `map_guard` is STILL held
        // (zero disk I/O / SHA-256 on Grouped/Unsafe). `DurabilityGate::persist_durable_locked` reads
        // `active_receipt()` once when persisting a Strict/sync slot.
        let mut info = std::mem::MaybeUninit::<ffi::MDB_envinfo>::uninit();
        // SAFETY: `self.inner.env` is open and protected by `self.map_guard`.
        let rc = unsafe { ffi::mdb_env_info(self.inner.env.as_ptr(), info.as_mut_ptr()) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        // SAFETY: `mdb_env_info` succeeded.
        let txnid = unsafe { info.assume_init() }.me_last_txnid as u64;
        let map_guard = self.map_guard.take().expect("map_guard present until commit");
        Ok((CommitReceipt { txnid, roots_sha256: [0u8; 32] }, map_guard))
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

/// Validated, alignment-safe LMDB key wrapper.
///
/// For `MDB_INTEGERKEY` sub-DBs, LMDB's `mdb_cmp_cint` casts `mv_data` to `unsigned short *`
/// (requiring >= 2-byte alignment). Copying the validated 8-byte key into a stack-aligned `u64`
/// guarantees 8-byte alignment for every LMDB call (`get`, `put`, `del`, `cursor.seek_*`)
/// regardless of caller slice alignment.
pub(crate) enum MdbKey<'a> {
    Bytes(&'a [u8]),
    Int(u64),
}

impl<'a> MdbKey<'a> {
    pub(crate) fn new(flags: DbFlags, key: &'a [u8]) -> Result<Self, StorageError> {
        if flags.integer_key {
            // Fixed 8-byte native `u64` only: prevents LMDB `mdb_cmp_cint` OOB & unaligned reads.
            let bytes: [u8; 8] = key.try_into().map_err(|_| StorageError::BadValSize { len: key.len() })?;
            Ok(Self::Int(u64::from_ne_bytes(bytes)))
        } else if key.is_empty() || key.len() > 511 {
            Err(StorageError::BadValSize { len: key.len() })
        } else {
            Ok(Self::Bytes(key))
        }
    }

    #[inline]
    pub(crate) fn as_mdb_val(&self) -> ffi::MDB_val {
        match self {
            Self::Bytes(bytes) => to_mdb_val(bytes),
            Self::Int(val) => ffi::MDB_val {
                mv_size: std::mem::size_of::<u64>(),
                mv_data: (&raw const *val).cast_mut().cast::<libc::c_void>(),
            },
        }
    }
}

fn lookup_flags(inner: &LmdbEnvInner, db: SubDbHandle) -> Result<DbFlags, StorageError> {
    inner.dbis.read().by_id.get(&db.raw()).copied().ok_or(StorageError::InvalidSubDb { handle: db.raw() })
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
    let key = MdbKey::new(flags, key)?;
    let mut k = key.as_mdb_val();
    let mut v = ffi::MDB_val { mv_size: 0, mv_data: ptr::null_mut() };
    // SAFETY: `txn` is live for the caller's borrow `'a`, `key` outlives `k`, and
    // `v` receives a pointer into LMDB's MVCC pages valid until `txn` mutates or ends.
    let rc = unsafe { ffi::mdb_get(txn, db.raw(), &mut k, &mut v) };
    match rc {
        ffi::MDB_SUCCESS => {
            // SAFETY: `mdb_get` succeeded and `v` stays valid for `'a`.
            Ok(Some(unsafe { mdb_val_to_slice(&v) }))
        }
        ffi::MDB_NOTFOUND => Ok(None),
        other => Err(map_mdb_err(other, inner.current_map_size(), inner.max_dbs)),
    }
}

fn raw_cursor<'a>(
    inner: &LmdbEnvInner,
    txn: *mut ffi::MDB_txn,
    db: SubDbHandle,
) -> Result<LmdbCursor<'a>, StorageError> {
    let flags = lookup_flags(inner, db)?;
    // SAFETY: Caller's `&'a self` borrow guarantees `txn` stays valid and unmutated for `'a`.
    unsafe { LmdbCursor::open(txn, db.raw(), flags, inner.current_map_size(), inner.max_dbs) }
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
        return Err(map_mdb_err(rc, inner.current_map_size(), inner.max_dbs));
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

        let mut rw = store.begin_write(store.lock_map_read()).unwrap();
        let db = rw.open_subdb("meta", DbFlags::DEFAULT).unwrap();
        rw.put(db, b"hello", b"world").unwrap();
        let (receipt, map_guard) = rw.commit_raw().unwrap();
        drop(map_guard);

        let meta = read_active_meta_page(store.data_mdb_path()).unwrap();
        let active = store.active_receipt().unwrap();
        assert_eq!(meta.txnid, store.last_txnid().unwrap());
        assert_eq!(meta.txnid, receipt.txnid);
        assert_eq!(meta.txnid, active.txnid);
        assert_eq!(meta.page_size as usize, store.page_size());
        assert_eq!(meta.roots_sha256, active.roots_sha256);

        // Write a second commit so both meta pages 0 and 1 are populated, then corrupt page 0
        // and verify `read_active_meta_page` still locates page 1 even on 16 KiB Apple Silicon!
        let mut rw = store.begin_write(store.lock_map_read()).unwrap();
        rw.put(db, b"hello2", b"world2").unwrap();
        let (_, map_guard) = rw.commit_raw().unwrap();
        drop(map_guard);
        let data_path = store.data_mdb_path().to_path_buf();
        drop(store);

        let mut file = OpenOptions::new().write(true).open(&data_path).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&[0xFF; 64]).unwrap();
        drop(file);

        let recovered = read_active_meta_page(&data_path).unwrap();
        assert_eq!(recovered.slot, 1);
    }

    #[test]
    fn batch_a_integer_key_aligns_odd_slices_across_all_ops_and_rejects_bad_lengths() {
        use elmdb_storage::KvCursor;

        let tmp = TempDir::new().unwrap();
        let store =
            LmdbStore::open(tmp.path(), LmdbConfig { map_size: 4 * 1024 * 1024, ..LmdbConfig::default() }).unwrap();

        let mut buf = [0u8; 17];
        // Pick an odd-addressed 8-byte slice guaranteed not to be 2-byte aligned.
        let offset = if (buf.as_ptr() as usize).is_multiple_of(2) { 1 } else { 0 };
        let odd_slice_1 = &mut buf[offset..offset + 8];
        assert_eq!((odd_slice_1.as_ptr() as usize) & 1, 1);
        odd_slice_1.copy_from_slice(&42u64.to_ne_bytes());
        let odd_key_1 = &buf[offset..offset + 8];

        // Verify `MdbKey::new` produces an 8-byte aligned pointer for `MDB_INTEGERKEY`.
        let wrapped = MdbKey::new(DbFlags::INTEGER_KEY, odd_key_1).unwrap();
        let val = wrapped.as_mdb_val();
        assert_eq!(val.mv_size, 8);
        assert_eq!((val.mv_data as usize) % std::mem::align_of::<u64>(), 0);

        // Verify bad key lengths are rejected and boundary 511-byte key is accepted.
        assert!(matches!(MdbKey::new(DbFlags::INTEGER_KEY, &[0u8; 7]), Err(StorageError::BadValSize { len: 7 })));
        assert!(matches!(MdbKey::new(DbFlags::INTEGER_KEY, &[0u8; 9]), Err(StorageError::BadValSize { len: 9 })));
        assert!(matches!(MdbKey::new(DbFlags::DEFAULT, &[]), Err(StorageError::BadValSize { len: 0 })));
        assert!(MdbKey::new(DbFlags::DEFAULT, &[b'k'; 511]).is_ok());
        assert!(matches!(MdbKey::new(DbFlags::DEFAULT, &[0u8; 512]), Err(StorageError::BadValSize { len: 512 })));

        let mut rw = store.begin_write(store.lock_map_read()).unwrap();
        let db = rw.open_subdb("ints", DbFlags::INTEGER_KEY).unwrap();
        assert!(rw.put_no_overwrite(db, odd_key_1, b"v42").unwrap());
        assert!(!rw.put_no_overwrite(db, odd_key_1, b"v42-dup").unwrap());
        rw.put(db, odd_key_1, b"v42-upd").unwrap();
        assert_eq!(rw.get(db, odd_key_1).unwrap(), Some(b"v42-upd".as_slice()));
        // Insert 400 odd-aligned integer keys to force LMDB B-tree branch splits (`mdb_cmp_long` 8-byte alignment).
        let mut bulk_buf = [0u8; 17];
        let bulk_off = if (bulk_buf.as_ptr() as usize).is_multiple_of(2) { 1 } else { 0 };
        for i in 100u64..500u64 {
            bulk_buf[bulk_off..bulk_off + 8].copy_from_slice(&i.to_ne_bytes());
            let k = &bulk_buf[bulk_off..bulk_off + 8];
            assert_eq!((k.as_ptr() as usize) & 1, 1);
            rw.put(db, k, b"payload-forcing-branch-page-split-1234567890").unwrap();
        }
        {
            let mut cur = rw.cursor(db).unwrap();
            let (k, v) = cur.seek_ge(odd_key_1).unwrap().unwrap();
            assert_eq!(k, odd_key_1);
            assert_eq!(v, b"v42-upd");
            assert_eq!(cur.seek_exact(odd_key_1).unwrap(), Some(b"v42-upd".as_slice()));
            bulk_buf[bulk_off..bulk_off + 8].copy_from_slice(&350u64.to_ne_bytes());
            let k350 = &bulk_buf[bulk_off..bulk_off + 8];
            assert_eq!(cur.seek_exact(k350).unwrap(), Some(b"payload-forcing-branch-page-split-1234567890".as_slice()));
        }
        let (_, mg) = rw.commit_raw().unwrap();
        drop(mg);

        let ro = store.begin_read().unwrap();
        assert_eq!(ro.get(db, odd_key_1).unwrap(), Some(b"v42-upd".as_slice()));
        {
            let mut cur = ro.cursor(db).unwrap();
            assert_eq!(cur.seek_exact(odd_key_1).unwrap(), Some(b"v42-upd".as_slice()));
        }
        drop(ro);

        let mut rw = store.begin_write(store.lock_map_read()).unwrap();
        assert!(rw.del(db, odd_key_1).unwrap());
        assert_eq!(rw.get(db, odd_key_1).unwrap(), None);
        let (_, mg) = rw.commit_raw().unwrap();
        drop(mg);
    }

    #[test]
    fn batch_a_preopen_existing_subdbs_skips_incompatible_and_propagates_dbs_full() {
        let tmp = TempDir::new().unwrap();
        {
            let store = LmdbStore::open(
                tmp.path(),
                LmdbConfig { map_size: 4 * 1024 * 1024, max_dbs: 32, ..LmdbConfig::default() },
            )
            .unwrap();
            let mut rw = store.begin_write(store.lock_map_read()).unwrap();
            for i in 0..8 {
                let db = rw.open_subdb(&format!("sub_{i}"), DbFlags::DEFAULT).unwrap();
                rw.put(db, b"k", b"v").unwrap();
            }
            // Also write a raw non-sub-DB entry into `MAIN_DBI` to exercise `MDB_INCOMPATIBLE` skipping!
            let mut raw_k = to_mdb_val(b"plain_main_entry");
            let mut raw_v = to_mdb_val(b"not_a_subdb");
            let rc = unsafe { ffi::mdb_put(rw.active_raw(), store.inner().main_dbi, &mut raw_k, &mut raw_v, 0) };
            assert_eq!(rc, ffi::MDB_SUCCESS);
            let (_, mg) = rw.commit_raw().unwrap();
            drop(mg);
        }

        // Reopening with ample `max_dbs` MUST skip `plain_main_entry` (`MDB_INCOMPATIBLE`) and open all 8 sub-DBs.
        {
            let store = LmdbStore::open(
                tmp.path(),
                LmdbConfig { map_size: 4 * 1024 * 1024, max_dbs: 32, ..LmdbConfig::default() },
            )
            .unwrap();
            let ro = store.begin_read().unwrap();
            let db0 = ro.open_subdb_existing("sub_0", DbFlags::DEFAULT).unwrap().expect("sub_0 present");
            assert_eq!(ro.get(db0, b"k").unwrap(), Some(b"v".as_slice()));
        }

        // Reopening with `max_dbs` too small to fit all existing named sub-DBs MUST fail with `DbsFull`.
        let err =
            LmdbStore::open(tmp.path(), LmdbConfig { map_size: 4 * 1024 * 1024, max_dbs: 4, ..LmdbConfig::default() })
                .err()
                .expect("reopen with max_dbs=4 over 8 existing sub-DBs must fail");
        assert!(matches!(err, StorageError::DbsFull { max_dbs: 4 }), "expected DbsFull, got {err:?}");
    }

    #[test]
    fn batch_a_copy_compacted_is_race_free_and_never_blocks_routine_writes_or_reads() {
        let tmp = TempDir::new().unwrap();
        let db_dir = tmp.path().join("db");
        let store = Arc::new(
            LmdbStore::open(&db_dir, LmdbConfig { map_size: 8 * 1024 * 1024, max_dbs: 128, ..LmdbConfig::default() })
                .unwrap(),
        );
        // Seed a sub-DB for routine writes & reads.
        let seeded_db = {
            let mut rw = store.begin_write(store.lock_map_read()).unwrap();
            let db = rw.open_subdb("seeded", DbFlags::DEFAULT).unwrap();
            rw.put(db, b"k0", b"v0").unwrap();
            let (_, mg) = rw.commit_raw().unwrap();
            drop(mg);
            db
        };

        // 1. Hold `EnvCopyGuard` on Thread A (simulating an in-flight `copy_compacted` I/O stream) AND
        //    park a `dbi_publish_gate.write()` waiter on Thread B: routine writes AND `begin_read` on the
        //    main thread MUST complete immediately without blocking!
        {
            let copy_store = Arc::clone(&store);
            let (tx_copy_held, rx_copy_held) = std::sync::mpsc::channel();
            let (tx_copy_release, rx_copy_release) = std::sync::mpsc::channel();
            let copy_holder = std::thread::spawn(move || {
                let _copy_guard = copy_store.inner().lock_env_copy().unwrap();
                tx_copy_held.send(()).unwrap();
                rx_copy_release.recv().unwrap();
            });
            rx_copy_held.recv().unwrap();

            let parked_store = Arc::clone(&store);
            let (tx_ready, rx_ready) = std::sync::mpsc::channel();
            let parked_writer = std::thread::spawn(move || {
                tx_ready.send(()).unwrap();
                let _w = parked_store.inner().dbi_publish_gate.write();
            });
            rx_ready.recv().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));

            // Routine write (no new sub-DB) MUST NOT block while `EnvCopyGuard` is held!
            let mut rw = store.begin_write(store.lock_map_read()).unwrap();
            rw.put(seeded_db, b"k1", b"v1").unwrap();
            let (_, mg) = rw.commit_raw().unwrap();
            drop(mg);

            // Reader `begin_read` + `get` MUST NOT block even with a parked `dbi_publish_gate` writer!
            let ro = store.begin_read().unwrap();
            assert_eq!(ro.get(seeded_db, b"k1").unwrap(), Some(b"v1".as_slice()));
            drop(ro);

            tx_copy_release.send(()).unwrap();
            copy_holder.join().unwrap();
            parked_writer.join().unwrap();
        }

        // 2. Concurrent dynamic sub-DB commits vs `copy_compacted` + backup verification.
        let writer_store = Arc::clone(&store);
        let writer = std::thread::spawn(move || {
            for i in 0..32 {
                let mut rw = writer_store.begin_write(writer_store.lock_map_read()).unwrap();
                let db = rw.open_subdb(&format!("dyn_{i}"), DbFlags::DEFAULT).unwrap();
                rw.put(db, b"k", b"v").unwrap();
                let (_, mg) = rw.commit_raw().unwrap();
                drop(mg);
            }
        });

        let mut last_bak = None;
        for i in 0..8 {
            let bak_dir = tmp.path().join(format!("bak_{i}"));
            let receipt = store.copy_compacted(&bak_dir).unwrap();
            assert!(bak_dir.join("data.mdb").exists());
            assert!(receipt.txnid >= 1);
            let _ro = store.begin_read().unwrap();
            last_bak = Some(bak_dir);
        }
        writer.join().unwrap();

        // Open the compacted backup and verify committed data is intact.
        let bak = LmdbStore::open(
            last_bak.unwrap(),
            LmdbConfig { map_size: 8 * 1024 * 1024, max_dbs: 128, read_only: true, ..LmdbConfig::default() },
        )
        .unwrap();
        let ro = bak.begin_read().unwrap();
        let db = ro.open_subdb_existing("seeded", DbFlags::DEFAULT).unwrap().expect("seeded present in backup");
        assert_eq!(ro.get(db, b"k1").unwrap(), Some(b"v1".as_slice()));
        drop(ro);
        drop(bak);

        // 3. `map_broken == true` fails closed in both `lock_env_read` (`begin_read`) and `lock_env_copy` (`copy_compacted`).
        store.inner().map_broken.store(true, Ordering::Release);
        assert!(matches!(store.begin_read(), Err(StorageError::Corrupted(_))));
        assert!(matches!(store.copy_compacted(tmp.path().join("bak_broken")), Err(StorageError::Corrupted(_))));
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "child process helper invoked by batch_a_begin_read_reclaims_stale_readers_and_rejects_live_full"]
    fn batch_a_stale_reader_child_helper() {
        let dir = std::env::var("ELMDB_STALE_READER_DIR").expect("ELMDB_STALE_READER_DIR set");
        let store =
            LmdbStore::open(&dir, LmdbConfig { map_size: 4 * 1024 * 1024, max_readers: 1, ..LmdbConfig::default() })
                .unwrap();
        let ro = store.begin_read().unwrap();
        // Deliberately exit without dropping `LmdbRoTxn` to leave a real stale PID reader slot in `lock.mdb`.
        std::mem::forget(ro);
        std::mem::forget(store);
        std::process::exit(0);
    }

    #[test]
    fn batch_a_begin_read_reclaims_stale_readers_and_rejects_live_full() {
        let tmp = TempDir::new().unwrap();
        let store = Arc::new(
            LmdbStore::open(
                tmp.path(),
                LmdbConfig { map_size: 4 * 1024 * 1024, max_readers: 1, ..LmdbConfig::default() },
            )
            .unwrap(),
        );

        // 1. Live reader on another thread occupies the sole slot -> `begin_read` hits `MDB_READERS_FULL` -> Err(ReadersFull).
        {
            let reader_store = Arc::clone(&store);
            let (tx_held, rx_held) = std::sync::mpsc::channel();
            let (tx_drop, rx_drop) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let _live_ro = reader_store.begin_read().unwrap();
                tx_held.send(()).unwrap();
                rx_drop.recv().unwrap();
            });
            rx_held.recv().unwrap();
            assert!(matches!(store.begin_read(), Err(StorageError::ReadersFull)));
            tx_drop.send(()).unwrap();
            holder.join().unwrap();
        }

        // 2. Child process opens a reader slot and exits without aborting it -> parent hits `MDB_READERS_FULL`,
        //    `mdb_reader_check` reclaims the stale slot, and retry succeeds.
        //    Gated on Unix: upstream LMDB's Windows `mdb_reader_pid` opens the exited PID with
        //    `PROCESS_QUERY_LIMITED_INFORMATION` (without `SYNCHRONIZE`), so `WaitForSingleObject(h, 0)`
        //    returns `WAIT_FAILED` ("alive") while Windows kernel/runner teardown still holds any handle.
        #[cfg(unix)]
        {
            let exe = std::env::current_exe().unwrap();
            let status = std::process::Command::new(exe)
                .args(["--exact", "txn::tests::batch_a_stale_reader_child_helper", "--ignored", "--nocapture"])
                .env("ELMDB_STALE_READER_DIR", tmp.path())
                .status()
                .unwrap();
            assert!(status.success(), "stale reader child helper failed");

            let recovered_ro = store.begin_read().expect("stale reader slot must be reclaimed on MDB_READERS_FULL");
            drop(recovered_ro);
        }
    }
}
