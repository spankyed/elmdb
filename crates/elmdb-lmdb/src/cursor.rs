use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::slice;

use elmdb_storage::{DbFlags, KvCursor, KvEntry, StorageError};
use lmdb_master_sys as ffi;

use crate::error::map_mdb_err;
use crate::txn::validate_key;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorState {
    Unpositioned,
    Positioned,
    Bof,
    Eof,
}

/// Zero-copy LMDB cursor tied to the lifetime `'txn` of its parent transaction.
pub struct LmdbCursor<'txn> {
    cursor: NonNull<ffi::MDB_cursor>,
    flags: DbFlags,
    map_size: usize,
    max_dbs: u32,
    state: CursorState,
    _txn: PhantomData<&'txn ()>,
}

impl<'txn> LmdbCursor<'txn> {
    /// Opens an LMDB cursor bound to `txn`.
    ///
    /// # Safety
    /// `txn` must remain valid and unmutated for the entire lifetime `'txn`.
    pub(crate) unsafe fn open(
        txn: *mut ffi::MDB_txn,
        dbi: ffi::MDB_dbi,
        flags: DbFlags,
        map_size: usize,
        max_dbs: u32,
    ) -> Result<Self, StorageError> {
        let mut raw: *mut ffi::MDB_cursor = ptr::null_mut();
        // SAFETY: Caller guarantees `txn` is valid for `'txn`; `raw` is a valid out-pointer.
        let rc = unsafe { ffi::mdb_cursor_open(txn, dbi, &mut raw) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, map_size, max_dbs));
        }
        let cursor = NonNull::new(raw).ok_or_else(|| StorageError::InvalidState("null MDB_cursor".into()))?;
        Ok(Self { cursor, flags, map_size, max_dbs, state: CursorState::Unpositioned, _txn: PhantomData })
    }

    fn get_op(
        &mut self,
        op: ffi::MDB_cursor_op,
        key_in: Option<&[u8]>,
        not_found_state: CursorState,
    ) -> Result<Option<KvEntry<'txn>>, StorageError> {
        let mut key_val = match key_in {
            Some(bytes) => {
                validate_key(self.flags, bytes)?;
                ffi::MDB_val { mv_size: bytes.len(), mv_data: bytes.as_ptr() as *mut libc::c_void }
            }
            None => ffi::MDB_val { mv_size: 0, mv_data: ptr::null_mut() },
        };
        let mut data_val = ffi::MDB_val { mv_size: 0, mv_data: ptr::null_mut() };

        // SAFETY: `self.cursor` is valid for `'txn`, `key_val` and `data_val` are valid pointers.
        // LMDB does not mutate `key_val.mv_data` for read cursor ops; on return both point into
        // the transaction's immutable MVCC pages valid for `'txn`.
        let rc = unsafe { ffi::mdb_cursor_get(self.cursor.as_ptr(), &mut key_val, &mut data_val, op) };
        match rc {
            ffi::MDB_SUCCESS => {
                self.state = CursorState::Positioned;
                // SAFETY: `mdb_cursor_get` returned `MDB_SUCCESS`; returned pointers remain valid
                // for the immutable borrow `'txn`.
                let k = unsafe { mdb_val_to_slice(&key_val) };
                let v = unsafe { mdb_val_to_slice(&data_val) };
                Ok(Some((k, v)))
            }
            ffi::MDB_NOTFOUND => {
                self.state = not_found_state;
                Ok(None)
            }
            other => Err(map_mdb_err(other, self.map_size, self.max_dbs)),
        }
    }
}

impl<'txn> KvCursor<'txn> for LmdbCursor<'txn> {
    fn first(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        self.get_op(ffi::MDB_FIRST, None, CursorState::Eof)
    }

    fn last(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        self.get_op(ffi::MDB_LAST, None, CursorState::Bof)
    }

    fn seek_ge(&mut self, key: &[u8]) -> Result<Option<KvEntry<'txn>>, StorageError> {
        self.get_op(ffi::MDB_SET_RANGE, Some(key), CursorState::Eof)
    }

    fn seek_exact(&mut self, key: &[u8]) -> Result<Option<&'txn [u8]>, StorageError> {
        Ok(self.get_op(ffi::MDB_SET_KEY, Some(key), CursorState::Unpositioned)?.map(|(_, v)| v))
    }

    fn next(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        match self.state {
            CursorState::Unpositioned | CursorState::Bof => self.first(),
            CursorState::Eof => Ok(None),
            CursorState::Positioned => self.get_op(ffi::MDB_NEXT, None, CursorState::Eof),
        }
    }

    fn prev(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        match self.state {
            CursorState::Unpositioned | CursorState::Eof => self.last(),
            CursorState::Bof => Ok(None),
            CursorState::Positioned => self.get_op(ffi::MDB_PREV, None, CursorState::Bof),
        }
    }
}

impl Drop for LmdbCursor<'_> {
    fn drop(&mut self) {
        // SAFETY: `self.cursor` was opened by `mdb_cursor_open` and has not been closed yet;
        // `'txn` ensures the owning transaction is still alive during cursor drop.
        unsafe {
            ffi::mdb_cursor_close(self.cursor.as_ptr());
        }
    }
}

/// Converts a returned LMDB `MDB_val` into a Rust byte slice.
///
/// # Safety
/// `val.mv_data` must be valid for reads of `val.mv_size` bytes for `'a` whenever `mv_size > 0`.
pub(crate) unsafe fn mdb_val_to_slice<'a>(val: &ffi::MDB_val) -> &'a [u8] {
    if val.mv_size == 0 || val.mv_data.is_null() {
        &[]
    } else {
        // SAFETY: Guaranteed by caller and LMDB's MVCC page lifetime contract.
        unsafe { slice::from_raw_parts(val.mv_data.cast::<u8>(), val.mv_size) }
    }
}
