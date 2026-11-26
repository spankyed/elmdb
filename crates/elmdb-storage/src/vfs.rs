use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::OnceLock;

use crate::error::StorageError;

/// Opaque 128-bit identifier for the current OS boot session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BootId(pub [u8; 16]);

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
}

/// Production filesystem implementation backed by `std::fs`.
#[derive(Debug, Default)]
pub struct OsVfs;

impl OsVfs {
    pub fn new() -> Self {
        Self
    }

    fn detect_boot_id() -> BootId {
        static CACHED: OnceLock<BootId> = OnceLock::new();
        *CACHED.get_or_init(|| {
            // Linux exposes a kernel-generated UUID per boot; other platforms fall back to
            // a process-cached random ID (replaced by platform boot-epoch anchor in elmdb-lmdb).
            #[cfg(target_os = "linux")]
            if let Ok(text) = fs::read_to_string("/proc/sys/kernel/random/boot_id") {
                let hex: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
                if hex.len() == 32 {
                    let mut out = [0u8; 16];
                    let mut ok = true;
                    for (i, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
                        match std::str::from_utf8(chunk).ok().and_then(|s| u8::from_str_radix(s, 16).ok()) {
                            Some(byte) => out[i] = byte,
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
                        return BootId(out);
                    }
                }
            }
            let mut out = [0u8; 16];
            let _ = getrandom::fill(&mut out);
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
        Self::detect_boot_id()
    }
}
