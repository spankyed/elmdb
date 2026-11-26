use std::collections::HashMap;
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use elmdb_storage::{DbFlags, KvStore, MetaLock, StorageError, SubDbHandle};
use lmdb_master_sys as ffi;

use crate::error::map_mdb_err;
use crate::meta::{read_active_meta_page, verify_data_file_headers};
use crate::txn::{LmdbRoTxn, LmdbRwTxn};

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
    /// Opens (or creates) an LMDB environment inside `dir`, verifying pre-map headers first.
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

        let init_result = (|| -> Result<usize, StorageError> {
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
            Ok(page_size)
        })();

        let page_size = match init_result {
            Ok(ps) => ps,
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
                map_size: config.map_size,
                max_dbs: config.max_dbs,
                page_size,
                dbis: MetaLock::new(DbiCache::default()),
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

    pub(crate) fn inner(&self) -> &Arc<LmdbEnvInner> {
        &self.inner
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
        if let Some(&(dbi, cached_flags)) = self.inner.dbis.lock().by_name.get(name) {
            if cached_flags != flags {
                return Err(StorageError::InvalidState(format!(
                    "sub-DB '{name}' opened read-only with mismatched flags"
                )));
            }
            return Ok(Some(SubDbHandle::from_raw(dbi)));
        }

        let cname =
            CString::new(name).map_err(|_| StorageError::InvalidState("sub-DB name contains interior NUL".into()))?;
        let mdb_flags = if flags.integer_key { ffi::MDB_INTEGERKEY } else { 0 };
        let mut dbi: ffi::MDB_dbi = 0;
        // SAFETY: `txn.raw()` is a live read-only `MDB_txn`, `cname` is NUL-terminated,
        // and `dbi` is a valid out-pointer.
        let rc = unsafe { ffi::mdb_dbi_open(txn.raw(), cname.as_ptr(), mdb_flags, &mut dbi) };
        match rc {
            ffi::MDB_SUCCESS => {
                let mut cache = self.inner.dbis.lock();
                cache.by_name.insert(name.to_owned(), (dbi, flags));
                cache.by_id.insert(dbi, flags);
                Ok(Some(SubDbHandle::from_raw(dbi)))
            }
            ffi::MDB_NOTFOUND => Ok(None),
            other => Err(map_mdb_err(other, self.inner.map_size, self.inner.max_dbs)),
        }
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
