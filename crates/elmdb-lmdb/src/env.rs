use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use elmdb_storage::{
    BootId, CommitReceipt, DbFlags, KvStore, MapGate, MetaLock, OrderedArcReadGuard, StorageError, SubDbHandle,
};
use lmdb_master_sys as ffi;
use sha2::{Digest, Sha256};

use crate::cursor::mdb_val_to_slice;
use crate::error::map_mdb_err;
use crate::meta::{read_active_meta_page, verify_data_file_headers};
use crate::txn::{LmdbRoTxn, LmdbRwTxn};

/// Process-wide single-open registry preventing two `MDB_env` handles from opening the same
/// LMDB directory in one process (which on POSIX breaks `fcntl` locks on `close(fd)`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum EnvIdentity {
    #[cfg(unix)]
    DevIno(u64, u64),
    #[cfg(not(unix))]
    CanonicalPath(String),
}

static OPEN_ENVS: LazyLock<Mutex<HashSet<EnvIdentity>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn env_identity(dir: &Path) -> Result<EnvIdentity, StorageError> {
    let canonical = fs::canonicalize(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(&canonical)?;
        Ok(EnvIdentity::DevIno(meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        Ok(EnvIdentity::CanonicalPath(canonical.to_string_lossy().to_ascii_lowercase()))
    }
}

/// Returns the kernel-stable 128-bit OS boot session identifier on Linux, macOS, and Windows.
///
/// - Linux: `/proc/sys/kernel/random/boot_id`
/// - macOS: `sysctlbyname("kern.bootsessionuuid")` (fallback `KERN_BOOTTIME`)
/// - Windows: `NtQuerySystemInformation(SystemTimeOfDayInformation).BootTime` hashed with domain separator
pub fn platform_boot_id() -> BootId {
    static CACHED: OnceLock<BootId> = OnceLock::new();
    *CACHED.get_or_init(|| {
        #[cfg(target_os = "linux")]
        if let Ok(text) = fs::read_to_string("/proc/sys/kernel/random/boot_id")
            && let Some(id) = parse_uuid_boot_id(&text)
        {
            return id;
        }

        #[cfg(target_os = "macos")]
        if let Some(id) = macos_boot_id() {
            return id;
        }

        #[cfg(windows)]
        if let Some(id) = windows_boot_id() {
            return id;
        }

        elmdb_storage::Vfs::boot_id(&elmdb_storage::OsVfs::new())
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn parse_uuid_boot_id(text: &str) -> Option<BootId> {
    let hex: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(BootId(out))
}

#[cfg(target_os = "macos")]
fn macos_boot_id() -> Option<BootId> {
    let mut buf = [0u8; 64];
    let mut len = buf.len();
    // SAFETY: `kern.bootsessionuuid` is a valid NUL-terminated sysctl name and `buf` is writable for `len` bytes.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &mut len,
            ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && len > 0 {
        let slice = &buf[..len.min(buf.len())];
        if let Ok(text) = std::str::from_utf8(slice)
            && let Some(id) = parse_uuid_boot_id(text.trim_end_matches('\0'))
        {
            return Some(id);
        }
    }
    // Fallback to `kern.boottime` (`struct timeval`) if `kern.bootsessionuuid` is unavailable.
    let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
    let mut tv_len = std::mem::size_of::<libc::timeval>();
    // SAFETY: `kern.boottime` populates a `libc::timeval` of size `tv_len`.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&raw mut tv).cast::<libc::c_void>(),
            &mut tv_len,
            ptr::null_mut(),
            0,
        )
    };
    if rc == 0 && tv_len == std::mem::size_of::<libc::timeval>() {
        let mut h = Sha256::new();
        h.update(b"elmdb-macos-boottime-v2");
        h.update(tv.tv_sec.to_le_bytes());
        h.update(tv.tv_usec.to_le_bytes());
        let digest = h.finalize();
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest[..16]);
        return Some(BootId(out));
    }
    None
}

#[cfg(windows)]
fn windows_boot_id() -> Option<BootId> {
    #[repr(C)]
    struct SystemTimeOfDayInformation {
        boot_time: i64,
        current_time: i64,
        time_zone_bias: i64,
        time_zone_id: u32,
        reserved: u32,
        boot_time_bias: u64,
        sleep_time_bias: u64,
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQuerySystemInformation(
            system_information_class: u32,
            system_information: *mut libc::c_void,
            system_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }
    let mut info = std::mem::MaybeUninit::<SystemTimeOfDayInformation>::zeroed();
    let mut ret_len: u32 = 0;
    const SYSTEM_TIMEOFDAY_INFORMATION_CLASS: u32 = 3;
    // SAFETY: `info` is a valid 48-byte buffer matching `SYSTEM_TIMEOFDAY_INFORMATION`.
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_TIMEOFDAY_INFORMATION_CLASS,
            info.as_mut_ptr().cast::<libc::c_void>(),
            std::mem::size_of::<SystemTimeOfDayInformation>() as u32,
            &mut ret_len,
        )
    };
    if status >= 0 {
        // SAFETY: `NtQuerySystemInformation` succeeded (`NT_SUCCESS(status)`).
        let info = unsafe { info.assume_init() };
        let mut h = Sha256::new();
        h.update(b"elmdb-win-boottime-v2");
        // Subtracting `boot_time_bias` keeps the boot anchor invariant across sleep/resume & clock adjustments.
        let stable_boot = info.boot_time.wrapping_sub(info.boot_time_bias as i64);
        h.update(stable_boot.to_le_bytes());
        let digest = h.finalize();
        let mut out = [0u8; 16];
        out.copy_from_slice(&digest[..16]);
        return Some(BootId(out));
    }
    None
}

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
    pub read_only: bool,
}

impl Default for LmdbConfig {
    fn default() -> Self {
        Self {
            map_size: DEFAULT_MAP_SIZE,
            max_dbs: DEFAULT_MAX_DBS,
            max_readers: DEFAULT_MAX_READERS,
            no_sync: true,
            no_readahead: true,
            read_only: false,
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
    pub(crate) map_size: AtomicUsize,
    pub(crate) max_dbs: u32,
    pub(crate) page_size: usize,
    /// Handle to LMDB's unnamed `MAIN_DBI` (committed during `LmdbStore::open`) used to check
    /// exact MVCC snapshot visibility of named sub-DBs without calling `mdb_dbi_open`.
    pub(crate) main_dbi: ffi::MDB_dbi,
    /// L2 gate guarding dynamic LMDB `mdb_env_set_mapsize` growth & external resize adoption.
    pub(crate) map_gate: MapGate<()>,
    /// Monotonically populated DBI cache. Handles are NEVER passed to `mdb_dbi_close()`.
    pub(crate) dbis: MetaLock<DbiCache>,
    /// Held until strictly AFTER `mdb_env_close` completes so duplicate opens are blocked cleanly.
    env_identity: EnvIdentity,
}

impl LmdbEnvInner {
    #[inline]
    pub(crate) fn current_map_size(&self) -> usize {
        self.map_size.load(Ordering::Acquire)
    }
}

// SAFETY: `MDB_env` is thread-safe for concurrent read transactions and externally serialized
// write transactions when opened with `MDB_NOTLS`. All Rust-side metadata uses ordered locks.
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
        // Release the process-wide single-open guard strictly AFTER `mdb_env_close` finishes!
        let mut open = OPEN_ENVS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        open.remove(&self.env_identity);
    }
}

struct OpenEnvRegistration(Option<EnvIdentity>);
impl Drop for OpenEnvRegistration {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            let mut open = OPEN_ENVS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            open.remove(&id);
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
        if !config.read_only {
            fs::create_dir_all(dir)?;
        }
        let identity = env_identity(dir)?;
        {
            let mut open = OPEN_ENVS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if !open.insert(identity.clone()) {
                return Err(StorageError::DatabaseLocked(dir.display().to_string()));
            }
        }
        let mut reg_guard = OpenEnvRegistration(Some(identity));

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
            if config.read_only {
                flags |= ffi::MDB_RDONLY;
            }
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

            // Reclaim stale reader slots left by crashed processes (best-effort on read-only mounts).
            let mut dead: libc::c_int = 0;
            // SAFETY: `env` is open and `dead` is a valid out-pointer.
            let _ = unsafe { ffi::mdb_reader_check(env.as_ptr(), &mut dead) };

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
            // so readers never call `mdb_dbi_open` on an aborting RO txn or concurrently.
            let (main_dbi, cache) = preopen_existing_subdbs(env.as_ptr(), actual_map_size, config.max_dbs)?;

            Ok((page_size, actual_map_size, main_dbi, cache))
        })();

        let (page_size, actual_map_size, main_dbi, cache) = match init_result {
            Ok(res) => res,
            Err(err) => {
                // SAFETY: `env` was created by `mdb_env_create` and is not shared yet.
                unsafe { ffi::mdb_env_close(env.as_ptr()) };
                // `reg_guard` drops right after `mdb_env_close`, releasing `OPEN_ENVS`.
                return Err(err);
            }
        };

        let env_identity = reg_guard.0.take().expect("registered");
        Ok(Self {
            inner: Arc::new(LmdbEnvInner {
                env,
                data_mdb,
                map_size: AtomicUsize::new(actual_map_size),
                max_dbs: config.max_dbs,
                page_size,
                main_dbi,
                map_gate: MapGate::new(()),
                dbis: MetaLock::new(cache),
                env_identity,
            }),
        })
    }

    /// Reclaims stale reader slots left behind by crashed processes. Returns the number reclaimed.
    pub fn reader_check(&self) -> Result<i32, StorageError> {
        let mut dead: libc::c_int = 0;
        // SAFETY: `self.inner.env` is an open `MDB_env` and `dead` is a valid out-pointer.
        let rc = unsafe { ffi::mdb_reader_check(self.inner.env.as_ptr(), &mut dead) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        Ok(dead)
    }

    pub fn data_mdb_path(&self) -> &Path {
        &self.inner.data_mdb
    }

    /// Creates a consistent, live, compacted copy of the LMDB environment into `target_dir`
    /// using `mdb_env_copy2(..., MDB_CP_COMPACT)` without blocking concurrent readers, fsyncs
    /// the copied `data.mdb`, and returns its active meta page's `CommitReceipt`.
    pub fn copy_compacted(&self, target_dir: impl AsRef<Path>) -> Result<elmdb_storage::CommitReceipt, StorageError> {
        let target_dir = target_dir.as_ref();
        fs::create_dir_all(target_dir)?;
        let c_path = path_to_cstring(target_dir)?;
        let _map_guard = self.inner.map_gate.read();
        // SAFETY: `self.inner.env` is an open `MDB_env` and `c_path` is a valid NUL-terminated path.
        let rc = unsafe { ffi::mdb_env_copy2(self.inner.env.as_ptr(), c_path.as_ptr(), ffi::MDB_CP_COMPACT) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        let copied_data = target_dir.join("data.mdb");
        {
            // Windows FlushFileBuffers (File::sync_all) requires GENERIC_WRITE access.
            let f = fs::OpenOptions::new().read(true).write(true).open(&copied_data)?;
            f.sync_all()?;
        }
        let meta = read_active_meta_page(&copied_data)?;
        Ok(elmdb_storage::CommitReceipt { txnid: meta.txnid, roots_sha256: meta.roots_sha256 })
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
    type OwnedRwTxn = LmdbRwTxn<'static>;
    type MapReadGuard = OrderedArcReadGuard<()>;

    fn lock_map_read(&self) -> Self::MapReadGuard {
        self.inner.map_gate.read_arc()
    }

    fn begin_read(&self) -> Result<Self::RoTxn<'_>, StorageError> {
        LmdbRoTxn::begin(self)
    }

    fn begin_write(&self) -> Result<Self::RwTxn<'_>, StorageError> {
        LmdbRwTxn::begin(self)
    }

    fn begin_write_owned(&self) -> Result<Self::OwnedRwTxn, StorageError> {
        LmdbRwTxn::begin_owned(self)
    }

    fn open_subdb_ro(
        &self,
        txn: &Self::RoTxn<'_>,
        name: &str,
        flags: DbFlags,
    ) -> Result<Option<SubDbHandle>, StorageError> {
        elmdb_storage::KvRead::open_subdb_existing(txn, name, flags)
    }

    fn ensure_subdbs_open(&self, names: &[(&str, DbFlags)]) -> Result<(), StorageError> {
        let missing: Vec<(&str, DbFlags)> = {
            let cache = self.inner.dbis.lock();
            names.iter().copied().filter(|(name, _)| !cache.by_name.contains_key(*name)).collect()
        };
        if missing.is_empty() {
            return Ok(());
        }

        let map_size = self.inner.current_map_size();
        let mut open_txn: *mut ffi::MDB_txn = ptr::null_mut();
        // SAFETY: Caller holds L2 read + L3 WriterLock, so no `LmdbRwTxn` is active or calling `mdb_dbi_open`.
        let rc =
            unsafe { ffi::mdb_txn_begin(self.inner.env.as_ptr(), ptr::null_mut(), ffi::MDB_RDONLY, &mut open_txn) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, map_size, self.inner.max_dbs));
        }

        let mut opened: Vec<(String, ffi::MDB_dbi, DbFlags)> = Vec::with_capacity(missing.len());
        let open_res = (|| -> Result<(), StorageError> {
            for (name, expected_flags) in missing {
                let c_name =
                    CString::new(name).map_err(|_| StorageError::InvalidState("sub-DB name contains NUL".into()))?;
                let mut dbi: ffi::MDB_dbi = 0;
                // SAFETY: `open_txn` is live; opening without `MDB_CREATE` only opens committed sub-DBs.
                let rc = unsafe { ffi::mdb_dbi_open(open_txn, c_name.as_ptr(), 0, &mut dbi) };
                match rc {
                    ffi::MDB_SUCCESS => {
                        let mut raw_flags: libc::c_uint = 0;
                        // SAFETY: `open_txn` and `dbi` are live.
                        let frc = unsafe { ffi::mdb_dbi_flags(open_txn, dbi, &mut raw_flags) };
                        if frc != ffi::MDB_SUCCESS {
                            return Err(map_mdb_err(frc, map_size, self.inner.max_dbs));
                        }
                        let actual_flags = DbFlags { integer_key: (raw_flags & ffi::MDB_INTEGERKEY) != 0 };
                        if actual_flags != expected_flags {
                            return Err(StorageError::InvalidState(format!(
                                "sub-DB '{name}' opened read-only with mismatched flags"
                            )));
                        }
                        opened.push((name.to_owned(), dbi, actual_flags));
                    }
                    ffi::MDB_NOTFOUND => {}
                    other => return Err(map_mdb_err(other, map_size, self.inner.max_dbs)),
                }
            }
            Ok(())
        })();

        if let Err(err) = open_res {
            // SAFETY: `open_txn` is live and aborted cleanly on error.
            unsafe { ffi::mdb_txn_abort(open_txn) };
            return Err(err);
        }

        // SAFETY: Committing an `MDB_RDONLY` txn permanently publishes opened DBIs into `MDB_env`.
        let rc = unsafe { ffi::mdb_txn_commit(open_txn) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, map_size, self.inner.max_dbs));
        }

        if !opened.is_empty() {
            let mut cache = self.inner.dbis.lock();
            for (name, dbi, flags) in opened {
                cache.by_name.insert(name, (dbi, flags));
                cache.by_id.insert(dbi, flags);
            }
        }
        Ok(())
    }

    fn needs_map_growth(&self) -> Result<Option<usize>, StorageError> {
        let mut info = std::mem::MaybeUninit::<ffi::MDB_envinfo>::uninit();
        // SAFETY: `self.inner.env` is open; `info` is a valid out-pointer.
        let rc = unsafe { ffi::mdb_env_info(self.inner.env.as_ptr(), info.as_mut_ptr()) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        // SAFETY: `mdb_env_info` succeeded.
        let info = unsafe { info.assume_init() };
        let cur_map = info.me_mapsize;
        let used_bytes = info.me_last_pgno.saturating_add(4).saturating_mul(self.inner.page_size);
        let min_headroom = (2 * 1024 * 1024).min(cur_map / 4);
        if used_bytes.saturating_mul(4) >= cur_map.saturating_mul(3)
            || cur_map.saturating_sub(used_bytes) < min_headroom
        {
            Ok(Some(cur_map.saturating_mul(2)))
        } else {
            Ok(None)
        }
    }

    fn grow_map(&self, min_size: Option<usize>) -> Result<usize, StorageError> {
        // Acquire L2 exclusive write lock so ZERO read or write transactions are active in this process.
        let _gate = self.inner.map_gate.write();
        let mut info = std::mem::MaybeUninit::<ffi::MDB_envinfo>::uninit();
        // SAFETY: `self.inner.env` is open and no transactions are active in this process under L2 write.
        let rc = unsafe { ffi::mdb_env_info(self.inner.env.as_ptr(), info.as_mut_ptr()) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        // SAFETY: `mdb_env_info` succeeded.
        let cur = unsafe { info.assume_init() }.me_mapsize;
        let doubled = cur.saturating_mul(2).max(self.inner.page_size * 16);
        let requested = min_size.unwrap_or(doubled).max(doubled);
        let page = self.inner.page_size.max(4096);
        let aligned = requested.saturating_add(page - 1) / page * page;
        if aligned <= cur {
            self.inner.map_size.store(cur, Ordering::Release);
            return Ok(cur);
        }
        // SAFETY: L2 `map_gate.write()` guarantees zero active transactions in this process.
        let rc = unsafe { ffi::mdb_env_set_mapsize(self.inner.env.as_ptr(), aligned) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, aligned, self.inner.max_dbs));
        }
        self.inner.map_size.store(aligned, Ordering::Release);
        Ok(aligned)
    }

    fn adopt_resized_map(&self) -> Result<usize, StorageError> {
        // Acquire L2 exclusive write lock so ZERO read or write transactions are active in this process.
        let _gate = self.inner.map_gate.write();
        // SAFETY: `size = 0` tells LMDB to adopt the external cross-process map size cleanly.
        let rc = unsafe { ffi::mdb_env_set_mapsize(self.inner.env.as_ptr(), 0) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        let mut info = std::mem::MaybeUninit::<ffi::MDB_envinfo>::uninit();
        // SAFETY: `self.inner.env` is open.
        let rc = unsafe { ffi::mdb_env_info(self.inner.env.as_ptr(), info.as_mut_ptr()) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        // SAFETY: `mdb_env_info` succeeded.
        let actual = unsafe { info.assume_init() }.me_mapsize;
        self.inner.map_size.store(actual, Ordering::Release);
        Ok(actual)
    }

    fn sync(&self) -> Result<(), StorageError> {
        // SAFETY: `self.inner.env` is an open `MDB_env`; `force = 1` flushes even under `MDB_NOSYNC`.
        let rc = unsafe { ffi::mdb_env_sync(self.inner.env.as_ptr(), 1) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        Ok(())
    }

    fn last_txnid(&self) -> Result<u64, StorageError> {
        let mut info = std::mem::MaybeUninit::<ffi::MDB_envinfo>::uninit();
        // SAFETY: `self.inner.env` is open; `info` is a valid out-pointer.
        let rc = unsafe { ffi::mdb_env_info(self.inner.env.as_ptr(), info.as_mut_ptr()) };
        if rc != ffi::MDB_SUCCESS {
            return Err(map_mdb_err(rc, self.inner.current_map_size(), self.inner.max_dbs));
        }
        // SAFETY: `mdb_env_info` succeeded and initialized `info`.
        Ok(unsafe { info.assume_init() }.me_last_txnid as u64)
    }

    fn roots_sha256(&self) -> Result<[u8; 32], StorageError> {
        Ok(read_active_meta_page(&self.inner.data_mdb)?.roots_sha256)
    }

    fn active_receipt(&self) -> Result<CommitReceipt, StorageError> {
        let meta = read_active_meta_page(&self.inner.data_mdb)?;
        Ok(CommitReceipt { txnid: meta.txnid, roots_sha256: meta.roots_sha256 })
    }

    fn map_size(&self) -> usize {
        self.inner.current_map_size()
    }

    fn page_size(&self) -> usize {
        self.inner.page_size
    }

    fn copy_compacted(&self, target_dir: &Path) -> Result<elmdb_storage::CommitReceipt, StorageError> {
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
