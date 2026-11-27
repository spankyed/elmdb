use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::OnceLock;

use crate::error::StorageError;

/// Opaque 128-bit identifier for the current OS boot session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BootId(pub [u8; 16]);

/// RAII guard holding an exclusive OS advisory file lock (or a no-op guard in simulation).
#[derive(Debug)]
pub struct VfsLockGuard(Option<File>);

impl VfsLockGuard {
    pub fn noop() -> Self {
        Self(None)
    }

    pub fn from_locked_file(file: File) -> Self {
        Self(Some(file))
    }
}

impl Drop for VfsLockGuard {
    fn drop(&mut self) {
        if let Some(file) = self.0.take() {
            let _ = file.unlock();
        }
    }
}

/// Filesystem seam for sidecar durability records, atomic staging, and crash simulation.
pub trait Vfs: Send + Sync {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>, StorageError>;
    fn read_at(&self, path: &Path, offset: u64, len: usize) -> Result<Vec<u8>, StorageError>;
    fn write_at(&self, path: &Path, offset: u64, data: &[u8]) -> Result<(), StorageError>;
    fn write_all(&self, path: &Path, data: &[u8]) -> Result<(), StorageError>;
    fn sync_file(&self, path: &Path) -> Result<(), StorageError>;
    fn sync_dir(&self, dir: &Path) -> Result<(), StorageError>;
    fn file_len(&self, path: &Path) -> Result<u64, StorageError>;
    fn exists(&self, path: &Path) -> bool;
    fn remove_file(&self, path: &Path) -> Result<(), StorageError>;
    fn rename(&self, from: &Path, to: &Path) -> Result<(), StorageError>;
    fn boot_id(&self) -> BootId;
    /// Acquires an exclusive advisory OS file lock on `path` (creating it if needed) to serialize
    /// cross-process durability sidecar updates. Simulation backends return `VfsLockGuard::noop()`.
    fn lock_file(&self, _path: &Path) -> Result<VfsLockGuard, StorageError> {
        Ok(VfsLockGuard::noop())
    }
}

/// Production filesystem implementation backed by `std::fs`.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsVfs {
    boot_id_override: Option<BootId>,
}

impl OsVfs {
    pub fn new() -> Self {
        Self { boot_id_override: None }
    }

    /// Constructs an `OsVfs` with a platform-supplied OS `BootId` (e.g. from `elmdb_lmdb::platform_boot_id()`).
    pub fn with_boot_id(boot_id: BootId) -> Self {
        Self { boot_id_override: Some(boot_id) }
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

    fn detect_boot_id() -> BootId {
        static CACHED: OnceLock<BootId> = OnceLock::new();
        *CACHED.get_or_init(|| {
            // Linux exposes a kernel-generated UUID per boot at /proc/sys/kernel/random/boot_id.
            #[cfg(target_os = "linux")]
            if let Ok(text) = fs::read_to_string("/proc/sys/kernel/random/boot_id")
                && let Some(id) = Self::parse_uuid_boot_id(&text)
            {
                return id;
            }
            // macOS exposes a kernel-generated UUID per boot via `kern.bootsessionuuid`.
            #[cfg(target_os = "macos")]
            if let Ok(output) =
                std::process::Command::new("/usr/sbin/sysctl").args(["-n", "kern.bootsessionuuid"]).output()
                && output.status.success()
                && let Ok(text) = std::str::from_utf8(&output.stdout)
                && let Some(id) = Self::parse_uuid_boot_id(text)
            {
                return id;
            }
            let mut out = [0u8; 16];
            if getrandom::fill(&mut out).is_err() {
                let pid = std::process::id().to_le_bytes();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .to_le_bytes();
                out[0..4].copy_from_slice(&pid);
                out[4..16].copy_from_slice(&now[0..12]);
            }
            BootId(out)
        })
    }
}

impl Vfs for OsVfs {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>, StorageError> {
        Ok(fs::read(path)?)
    }

    fn read_at(&self, path: &Path, offset: u64, len: usize) -> Result<Vec<u8>, StorageError> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; len];
        let mut read = 0;
        while read < len {
            let n = file.read(&mut buf[read..])?;
            if n == 0 {
                buf.truncate(read);
                break;
            }
            read += n;
        }
        Ok(buf)
    }

    fn write_at(&self, path: &Path, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        let mut file = OpenOptions::new().create(true).truncate(false).write(true).read(true).open(path)?;
        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        Ok(())
    }

    fn write_all(&self, path: &Path, data: &[u8]) -> Result<(), StorageError> {
        let mut file = OpenOptions::new().create(true).truncate(true).write(true).open(path)?;
        file.write_all(data)?;
        Ok(())
    }

    fn sync_file(&self, path: &Path) -> Result<(), StorageError> {
        let file = OpenOptions::new().write(true).read(true).open(path)?;
        file.sync_all()?;
        Ok(())
    }

    fn sync_dir(&self, dir: &Path) -> Result<(), StorageError> {
        #[cfg(unix)]
        {
            let handle = File::open(dir)?;
            handle.sync_all()?;
        }
        #[cfg(not(unix))]
        let _ = dir;
        Ok(())
    }

    fn file_len(&self, path: &Path) -> Result<u64, StorageError> {
        Ok(fs::metadata(path)?.len())
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn remove_file(&self, path: &Path) -> Result<(), StorageError> {
        Ok(fs::remove_file(path)?)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), StorageError> {
        Ok(fs::rename(from, to)?)
    }

    fn boot_id(&self) -> BootId {
        self.boot_id_override.unwrap_or_else(Self::detect_boot_id)
    }

    fn lock_file(&self, path: &Path) -> Result<VfsLockGuard, StorageError> {
        let file = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(path)?;
        file.lock()?;
        Ok(VfsLockGuard::from_locked_file(file))
    }
}
