use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use elmdb_storage::{DbFlags, KvStore, MetaLock, StorageError, SubDbHandle};
use lmdb_master_sys as ffi;

use crate::cursor::mdb_val_to_slice;
use crate::error::map_mdb_err;
use crate::meta::{read_active_meta_page, verify_data_file_headers};
use crate::txn::{LmdbRoTxn, LmdbRwTxn, validate_subdb_name};

/// Default virtual address map reservation: 16 GiB sparse on 64-bit Unix, 64 MiB on Windows.
#[cfg(all(unix, target_pointer_width = "64"))]
pub const DEFAULT_MAP_SIZE: usize = 16 * 1024 * 1024 * 1024;
#[cfg(not(all(unix, target_pointer_width = "64")))]
pub const DEFAULT_MAP_SIZE: usize = 64 * 1024 * 1024;

pub const DEFAULT_MAX_DBS: u32 = 512;
pub const DEFAULT_MAX_READERS: u32 = 512;

/// Configuration for opening an `LmdbStore`.
#[derive(Debug, Clone)]
pub struct LmdbConfig {
    pub map_size: usize,
    pub max_dbs: u32,
    pub max_readers: u32,
    pub no_sync: bool,
    pub no_readahead: bool,
}

impl Default for LmdbConfig {
    fn default() -> Self {
        Self {
            map_size: DEFAULT_MAP_SIZE,
            max_dbs: DEFAULT_MAX_DBS,
            max_readers: DEFAULT_MAX_READERS,
            no_sync: true,
            no_readahead: true,
        }
    }
}

#[derive(Default)]
pub(crate) struct DbiCache {
    pub(crate) by_name: HashMap<String, (ffi::MDB_dbi, DbFlags)>,
    pub(crate) by_id: HashMap<ffi::MDB_dbi, DbFlags>,
}

pub(crate) struct LmdbEnvInner {
    pub(crate) env: NonNull<ffi::MDB_env>,
    pub(crate) data_mdb: PathBuf,
    pub(crate) map_size: usize,
    pub(crate) max_dbs: u32,
    pub(crate) page_size: usize,
    /// Handle to LMDB's unnamed `MAIN_DBI` (committed during `LmdbStore::open`) used to check
    /// exact MVCC snapshot visibility of named sub-DBs in `open_subdb_ro` without calling `mdb_dbi_open`.
    pub(crate) main_dbi: ffi::MDB_dbi,
    /// Monotonically populated DBI cache. Handles are NEVER passed to `mdb_dbi_close()`.
    pub(crate) dbis: MetaLock<DbiCache>,
}

// SAFETY: `MDB_env` is thread-safe for concurrent read transactions and externally serialized
// write transactions when opened with `MDB_NOTLS`. All Rust-side metadata uses `MetaLock`.
unsafe impl Send for LmdbEnvInner {}
// SAFETY: Same as `Send`; `MDB_env` supports concurrent multi-thread access under `MDB_NOTLS`.
unsafe impl Sync for LmdbEnvInner {}

impl Drop for LmdbEnvInner {
    fn drop(&mut self) {
        // SAFETY: `LmdbEnvInner` is held inside `Arc`, so `Drop` only runs after every
        // `LmdbRoTxn`, `LmdbRwTxn`, and `LmdbStore` handle has been dropped.
        unsafe {
            ffi::mdb_env_close(self.env.as_ptr());
        }
    }
}

/// Owned safe LMDB environment implementing `elmdb_storage::KvStore`.
#[derive(Clone)]
pub struct LmdbStore {
    inner: Arc<LmdbEnvInner>,
}

impl LmdbStore {
    /// Opens (or creates) an LMDB environment inside `dir`, verifying pre-map headers first
    /// and pre-opening all existing named sub-DBs in a single committed read transaction.
    pub fn open(dir: impl AsRef<Path>, config: LmdbConfig) -> Result<Self, StorageError> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        let data_mdb = dir.join("data.mdb");
        verify_data_file_headers(&data_mdb)?;

        let path_cstr = path_to_cstring(dir)?;

        let mut raw_env: *mut ffi::MDB_env = ptr::null_mut();
        // SAFETY: `raw_env` is a valid out-pointer; `mdb_env_create` allocates an `MDB_env`.
        let rc = unsafe { ffi::mdb_env_create(&mut raw_env) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, config.map_size, config.max_dbs));
        }
        let env = NonNull::new(raw_env).ok_or_else(|| StorageError::InvalidState("null MDB_env".into()))?;

        let init_result = (|| -> Result<(usize, usize, ffi::MDB_dbi, DbiCache), StorageError> {
            // SAFETY: `env` is a newly created, unopened `MDB_env`.
            let rc = unsafe { ffi::mdb_env_set_mapsize(env.as_ptr(), config.map_size) };
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, config.map_size, config.max_dbs));
            }
            // SAFETY: `env` is unopened.
            let rc = unsafe { ffi::mdb_env_set_maxdbs(env.as_ptr(), config.max_dbs) };
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, config.map_size, config.max_dbs));
            }
            // SAFETY: `env` is unopened.
            let rc = unsafe { ffi::mdb_env_set_maxreaders(env.as_ptr(), config.max_readers) };
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, config.map_size, config.max_dbs));
            }

            let mut flags = ffi::MDB_NOTLS;
            if config.no_sync {
                flags |= ffi::MDB_NOSYNC;
            }
            if config.no_readahead {
                flags |= ffi::MDB_NORDAHEAD;
            }

            // SAFETY: `path_cstr` is a valid NUL-terminated path; `env` is configured and unopened.
            let rc = unsafe { ffi::mdb_env_open(env.as_ptr(), path_cstr.as_ptr(), flags, 0o600) };
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, config.map_size, config.max_dbs));
            }

            let mut stat = std::mem::MaybeUninit::<ffi::MDB_stat>::uninit();
            // SAFETY: `env` is open; `stat` is a valid out-pointer.
            let rc = unsafe { ffi::mdb_env_stat(env.as_ptr(), stat.as_mut_ptr()) };
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, config.map_size, config.max_dbs));
            }
            // SAFETY: `mdb_env_stat` succeeded and initialized `stat`.
            let page_size = unsafe { stat.assume_init() }.ms_psize as usize;

            let mut info = std::mem::MaybeUninit::<ffi::MDB_envinfo>::uninit();
            // SAFETY: `env` is open; `info` is a valid out-pointer.
            let rc = unsafe { ffi::mdb_env_info(env.as_ptr(), info.as_mut_ptr()) };
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, config.map_size, config.max_dbs));
            }
            // SAFETY: `mdb_env_info` succeeded and initialized `info`.
            let actual_map_size = unsafe { info.assume_init() }.me_mapsize;

            // Pre-open `MAIN_DBI` and all existing named sub-DBs inside ONE committed RO transaction
            // so `open_subdb_ro` never calls `mdb_dbi_open` on an aborting RO txn or concurrently.
            let (main_dbi, cache) = preopen_existing_subdbs(env.as_ptr(), actual_map_size, config.max_dbs)?;

            Ok((page_size, actual_map_size, main_dbi, cache))
        })();

        let (page_size, actual_map_size, main_dbi, cache) = match init_result {
            Ok(res) => res,
            Err(err) => {
                // SAFETY: `env` was created by `mdb_env_create` and is not shared yet.
                unsafe { ffi::mdb_env_close(env.as_ptr()) };
                return Err(err);
            }
        };

        Ok(Self {
            inner: Arc::new(LmdbEnvInner {
                env,
                data_mdb,
                map_size: actual_map_size,
                max_dbs: config.max_dbs,
                page_size,
                main_dbi,
                dbis: MetaLock::new(cache),
            }),
        })
    }

    /// Reclaims stale reader slots left behind by crashed processes. Returns the number reclaimed.
    pub fn reader_check(&self) -> Result<i32, StorageError> {
        let mut dead: libc::c_int = 0;
        // SAFETY: `self.inner.env` is an open `MDB_env` and `dead` is a valid out-pointer.
        let rc = unsafe { ffi::mdb_reader_check(self.inner.env.as_ptr(), &mut dead) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        Ok(dead)
    }

    pub fn data_mdb_path(&self) -> &Path {
        &self.inner.data_mdb
    }

    /// Creates a consistent, live, compacted copy of the LMDB environment into `target_dir`
    /// using `mdb_env_copy2(..., MDB_CP_COMPACT)` without blocking concurrent readers.
    pub fn copy_compacted(&self, target_dir: impl AsRef<Path>) -> Result<(), StorageError> {
        let target_dir = target_dir.as_ref();
        fs::create_dir_all(target_dir)?;
        let c_path = path_to_cstring(target_dir)?;
        // SAFETY: `self.inner.env` is an open `MDB_env` and `c_path` is a valid NUL-terminated path.
        let rc = unsafe { ffi::mdb_env_copy2(self.inner.env.as_ptr(), c_path.as_ptr(), ffi::MDB_CP_COMPACT) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        Ok(())
    }

    pub(crate) fn inner(&self) -> &Arc<LmdbEnvInner> {
        &self.inner
    }
}

/// Discovers and opens all existing named sub-DBs from `MAIN_DBI` inside a single committed
/// read-only transaction, recording each sub-DB's real on-disk flags via `mdb_dbi_flags`.
fn preopen_existing_subdbs(
    env: *mut ffi::MDB_env,
    map_size: usize,
    max_dbs: u32,
) -> Result<(ffi::MDB_dbi, DbiCache), StorageError> {
    let mut txn: *mut ffi::MDB_txn = ptr::null_mut();
    // SAFETY: `env` is open and not yet shared across threads.
    let rc = unsafe { ffi::mdb_txn_begin(env, ptr::null_mut(), ffi::MDB_RDONLY, &mut txn) };
    if rc != ffi::MDB_SUCCESS {
        return Err(map_mdb_err(rc, map_size, max_dbs));
    }

    let result = (|| -> Result<(ffi::MDB_dbi, DbiCache), StorageError> {
        let mut main_dbi: ffi::MDB_dbi = 0;
        // SAFETY: `txn` is live; `null` opens LMDB's unnamed `MAIN_DBI`.
        let rc = unsafe { ffi::mdb_dbi_open(txn, ptr::null(), 0, &mut main_dbi) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, map_size, max_dbs));
        }

        let mut names = Vec::new();
        let mut cur: *mut ffi::MDB_cursor = ptr::null_mut();
        // SAFETY: `txn` and `main_dbi` are live.
        let rc = unsafe { ffi::mdb_cursor_open(txn, main_dbi, &mut cur) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, map_size, max_dbs));
        }

        let scan_res = (|| -> Result<(), StorageError> {
            let mut k = ffi::MDB_val { mv_size: 0, mv_data: ptr::null_mut() };
            let mut v = ffi::MDB_val { mv_size: 0, mv_data: ptr::null_mut() };
            let mut op = ffi::MDB_FIRST;
            loop {
                // SAFETY: `cur` is valid inside `txn`.
                let rc = unsafe { ffi::mdb_cursor_get(cur, &mut k, &mut v, op) };
                match rc {
                    ffi::MDB_SUCCESS => {
                        // SAFETY: `k` points into `txn`'s MVCC pages.
                        let bytes = unsafe { mdb_val_to_slice(&k) };
                        if let Ok(name) = std::str::from_utf8(bytes)
                            && !name.is_empty()
                            && !name.as_bytes().contains(&0)
                        {
                            names.push(name.to_owned());
                        }
                        op = ffi::MDB_NEXT;
                    }
                    ffi::MDB_NOTFOUND => break,
                    other => return Err(map_mdb_err(other, map_size, max_dbs)),
                }
            }
            Ok(())
        })();
        // SAFETY: `cur` was opened in this function and is closed before `txn` commits.
        unsafe { ffi::mdb_cursor_close(cur) };
        scan_res?;

        let mut cache = DbiCache::default();
        for name in names {
            let Ok(cname) = CString::new(name.as_str()) else {
                continue;
            };
            let mut dbi: ffi::MDB_dbi = 0;
            // SAFETY: `txn` is live; opening without `MDB_CREATE` only succeeds for genuine sub-DBs.
            let rc = unsafe { ffi::mdb_dbi_open(txn, cname.as_ptr(), 0, &mut dbi) };
            if rc != ffi::MDB_SUCCESS {
                // Non-sub-DB entries in MAIN_DBI (if any) return MDB_INCOMPATIBLE; skip them safely.
                continue;
            }
            let mut raw_flags: libc::c_uint = 0;
            // SAFETY: `txn` and `dbi` are live.
            let rc = unsafe { ffi::mdb_dbi_flags(txn, dbi, &mut raw_flags) };
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, map_size, max_dbs));
            }
            let flags = DbFlags { integer_key: (raw_flags & ffi::MDB_INTEGERKEY) != 0 };
            cache.by_name.insert(name, (dbi, flags));
            cache.by_id.insert(dbi, flags);
        }

        Ok((main_dbi, cache))
    })();

    match result {
        Ok(res) => {
            // SAFETY: Committing (never aborting!) this RO txn makes every opened `MDB_dbi`
            // permanent in `MDB_env` per LMDB's `mdb_dbi_open` contract.
            let rc = unsafe { ffi::mdb_txn_commit(txn) };
            if rc != ffi::MDB_SUCCESS {
                return Err(map_mdb_err(rc, map_size, max_dbs));
            }
            Ok(res)
        }
        Err(err) => {
            // SAFETY: `txn` is live and aborted cleanly on error.
            unsafe { ffi::mdb_txn_abort(txn) };
            Err(err)
        }
    }
}

impl KvStore for LmdbStore {
    type RoTxn<'a> = LmdbRoTxn<'a>;
    type RwTxn<'a> = LmdbRwTxn<'a>;

    fn begin_read(&self) -> Result<Self::RoTxn<'_>, StorageError> {
        LmdbRoTxn::begin(self)
    }

    fn begin_write(&self) -> Result<Self::RwTxn<'_>, StorageError> {
        LmdbRwTxn::begin(self)
    }

    fn open_subdb_ro(
        &self,
        txn: &Self::RoTxn<'_>,
        name: &str,
        flags: DbFlags,
    ) -> Result<Option<SubDbHandle>, StorageError> {
        validate_subdb_name(name)?;

        // Check exact MVCC visibility in `txn`'s snapshot via `MAIN_DBI` before touching DBI handles:
        // a sub-DB created by a writer *after* `txn` began must return `Ok(None)`.
        let mut k = ffi::MDB_val { mv_size: name.len(), mv_data: name.as_ptr() as *mut libc::c_void };
        let mut v = ffi::MDB_val { mv_size: 0, mv_data: ptr::null_mut() };
        // SAFETY: `txn.raw()` and `self.inner.main_dbi` are live; `k` points to readable UTF-8 bytes.
        let rc = unsafe { ffi::mdb_get(txn.raw(), self.inner.main_dbi, &mut k, &mut v) };
        match rc {
            ffi::MDB_SUCCESS => {}
            ffi::MDB_NOTFOUND => return Ok(None),
            other => return Err(map_mdb_err(other, self.inner.map_size, self.inner.max_dbs)),
        }

        // Look up the permanently committed DBI handle (never calls `mdb_dbi_open` on an aborting RO txn).
        let mut cache = self.inner.dbis.lock();
        if let Some(&(dbi, cached_flags)) = cache.by_name.get(name) {
            if cached_flags != flags {
                return Err(StorageError::InvalidState(format!(
                    "sub-DB '{name}' opened read-only with mismatched flags"
                )));
            }
            return Ok(Some(SubDbHandle::from_raw(dbi)));
        }

        // Cross-process catch-up: `MAIN_DBI` confirmed `name` is committed and visible in `txn`,
        // so another process created this sub-DB after our `LmdbStore::open`. Open it permanently
        // in a short dedicated committed RO txn under L4 so readers never miss cross-process slots.
        let c_name = CString::new(name).map_err(|_| StorageError::InvalidState("sub-DB name contains NUL".into()))?;
        let mut open_txn: *mut ffi::MDB_txn = ptr::null_mut();
        // SAFETY: `self.inner.env` is open; `MDB_RDONLY | MDB_NOTLS` allows a nested/independent RO txn on the same thread.
        let rc =
            unsafe { ffi::mdb_txn_begin(self.inner.env.as_ptr(), ptr::null_mut(), ffi::MDB_RDONLY, &mut open_txn) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        let mut dbi: ffi::MDB_dbi = 0;
        // SAFETY: `open_txn` is live and `c_name` is a valid NUL-terminated C string.
        let rc = unsafe { ffi::mdb_dbi_open(open_txn, c_name.as_ptr(), 0, &mut dbi) };
        if rc != ffi::MDB_SUCCESS {
            // SAFETY: `open_txn` is live; abort on error.
            unsafe { ffi::mdb_txn_abort(open_txn) };
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        let mut raw_flags: libc::c_uint = 0;
        // SAFETY: `open_txn` and `dbi` are valid.
        let rc = unsafe { ffi::mdb_dbi_flags(open_txn, dbi, &mut raw_flags) };
        if rc != ffi::MDB_SUCCESS {
            // SAFETY: `open_txn` is live.
            unsafe { ffi::mdb_txn_abort(open_txn) };
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        // SAFETY: committing an `MDB_RDONLY` txn publishes `dbi` permanently across `MDB_env`!
        let rc = unsafe { ffi::mdb_txn_commit(open_txn) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        let actual_flags = DbFlags { integer_key: (raw_flags & ffi::MDB_INTEGERKEY) != 0 };
        cache.by_name.insert(name.to_owned(), (dbi, actual_flags));
        cache.by_id.insert(dbi, actual_flags);
        if actual_flags != flags {
            return Err(StorageError::InvalidState(format!("sub-DB '{name}' opened read-only with mismatched flags")));
        }
        Ok(Some(SubDbHandle::from_raw(dbi)))
    }

    fn sync(&self) -> Result<(), StorageError> {
        // SAFETY: `self.inner.env` is an open `MDB_env`; `force = 1` flushes even under `MDB_NOSYNC`.
        let rc = unsafe { ffi::mdb_env_sync(self.inner.env.as_ptr(), 1) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        Ok(())
    }

    fn last_txnid(&self) -> Result<u64, StorageError> {
        let mut info = std::mem::MaybeUninit::<ffi::MDB_envinfo>::uninit();
        // SAFETY: `self.inner.env` is open; `info` is a valid out-pointer.
        let rc = unsafe { ffi::mdb_env_info(self.inner.env.as_ptr(), info.as_mut_ptr()) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.map_size, self.inner.max_dbs));
        }
        // SAFETY: `mdb_env_info` succeeded and initialized `info`.
        Ok(unsafe { info.assume_init() }.me_last_txnid as u64)
    }

    fn roots_sha256(&self) -> Result<[u8; 32], StorageError> {
        Ok(read_active_meta_page(&self.inner.data_mdb)?.roots_sha256)
    }

    fn map_size(&self) -> usize {
        self.inner.map_size
    }

    fn page_size(&self) -> usize {
        self.inner.page_size
    }

    fn copy_compacted(&self, target_dir: &Path) -> Result<(), StorageError> {
        LmdbStore::copy_compacted(self, target_dir)
    }
}

fn path_to_cstring(path: &Path) -> Result<CString, StorageError> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| StorageError::InvalidState("database path contains interior NUL".into()))
    }
    #[cfg(not(unix))]
    {
        let s = path.to_str().ok_or_else(|| StorageError::InvalidState("database path is not valid UTF-8".into()))?;
        CString::new(s).map_err(|_| StorageError::InvalidState("database path contains interior NUL".into()))
    }
}
