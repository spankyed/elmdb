use std::ffi::CStr;
use std::io;

use elmdb_storage::StorageError;
use libc::c_int;
use lmdb_master_sys as ffi;

/// Maps an LMDB C return code into `StorageError`.
pub(crate) fn map_mdb_err(rc: c_int, map_size: usize, max_dbs: u32) -> StorageError {
    match rc {
        ffi::MDB_NOTFOUND => StorageError::NotFound,
        ffi::MDB_KEYEXIST => StorageError::KeyExist,
        ffi::MDB_MAP_FULL => StorageError::MapFull { map_size },
        ffi::MDB_MAP_RESIZED => StorageError::MapResized,
        ffi::MDB_DBS_FULL => StorageError::DbsFull { max_dbs },
        ffi::MDB_READERS_FULL | ffi::MDB_TLS_FULL => StorageError::ReadersFull,
        ffi::MDB_TXN_FULL => StorageError::TxnFull,
        ffi::MDB_BAD_VALSIZE => StorageError::BadValSize { len: 0 },
        ffi::MDB_BAD_DBI => StorageError::InvalidSubDb { handle: 0 },
        ffi::MDB_CORRUPTED | ffi::MDB_PAGE_NOTFOUND | ffi::MDB_PANIC | ffi::MDB_INVALID => {
            StorageError::Corrupted(mdb_strerror(rc))
        }
        code if code > 0 => StorageError::Io(io::Error::from_raw_os_error(code)),
        _ => StorageError::InvalidState(mdb_strerror(rc)),
    }
}

pub(crate) fn mdb_strerror(rc: c_int) -> String {
    // SAFETY: `mdb_strerror` returns a static NUL-terminated ASCII C string for any `c_int`.
    let ptr = unsafe { ffi::mdb_strerror(rc) };
    if ptr.is_null() {
        return format!("LMDB error {rc}");
    }
    // SAFETY: `ptr` is non-null and NUL-terminated per LMDB's contract.
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}
