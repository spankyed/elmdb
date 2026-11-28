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

const _: () = assert!(ffi::MDB_KEYEXIST <= ffi::MDB_LAST_ERRCODE);

pub(crate) fn mdb_strerror(rc: c_int) -> String {
    // On Windows, LMDB's `mdb_strerror` formats unrecognized error codes into a stack-local
    // `char buf[...]` and returns a pointer to it (stack-use-after-return UB). Restrict calls
    // to `MDB_SUCCESS` and LMDB's static `mdb_errstr` table (`MDB_KEYEXIST..=MDB_LAST_ERRCODE`).
    if rc != ffi::MDB_SUCCESS && !(ffi::MDB_KEYEXIST..=ffi::MDB_LAST_ERRCODE).contains(&rc) {
        return format!("LMDB error {rc}");
    }
    // SAFETY: `rc` is 0 or in LMDB's static error table, so `mdb_strerror` returns a static
    // NUL-terminated ASCII C string on every platform.
    let ptr = unsafe { ffi::mdb_strerror(rc) };
    if ptr.is_null() {
        return format!("LMDB error {rc}");
    }
    // SAFETY: `ptr` is non-null and NUL-terminated per LMDB's contract.
    unsafe { CStr::from_ptr(ptr) }.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdb_strerror_uses_static_table_only_for_known_lmdb_codes() {
        assert!(!mdb_strerror(ffi::MDB_SUCCESS).starts_with("LMDB error "));
        assert!(!mdb_strerror(ffi::MDB_KEYEXIST).starts_with("LMDB error "));
        assert!(!mdb_strerror(ffi::MDB_LAST_ERRCODE).starts_with("LMDB error "));

        // Out-of-range codes must never invoke LMDB's Windows stack-buffer fallback.
        for rc in [-1, ffi::MDB_KEYEXIST - 1, ffi::MDB_LAST_ERRCODE + 1, i32::MIN, 1] {
            assert_eq!(mdb_strerror(rc), format!("LMDB error {rc}"));
        }
    }
}
