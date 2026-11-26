use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use elmdb_storage::{BootId, StorageError, Vfs};
use parking_lot::Mutex;

use crate::rng::SimRng;

/// Injectable VFS fault counters for deterministic failure testing.
#[derive(Debug, Default)]
pub struct SimVfsFaults {
    pub fail_next_write: AtomicU32,
    pub fail_next_sync: AtomicU32,
    pub fail_dir_sync: AtomicBool,
}

#[derive(Debug, Default)]
struct VfsInner {
    /// Files committed to durable storage via `sync_file`.
    durable: BTreeMap<PathBuf, Vec<u8>>,
    /// Files visible in OS page cache (includes unsynced writes).
    dirty: BTreeMap<PathBuf, Vec<u8>>,
    boot_id: [u8; 16],
}

/// Deterministic in-memory VFS modeling OS page cache, fsync boundaries, torn 4 KiB writes,
/// and OS reboots.
#[derive(Debug, Clone)]
pub struct SimVfs {
    inner: Arc<Mutex<VfsInner>>,
    faults: Arc<SimVfsFaults>,
}

impl SimVfs {
    pub fn new(initial_boot: BootId) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VfsInner {
                durable: BTreeMap::new(),
                dirty: BTreeMap::new(),
                boot_id: initial_boot.0,
            })),
            faults: Arc::new(SimVfsFaults::default()),
        }
    }

    pub fn faults(&self) -> &SimVfsFaults {
        &self.faults
    }

    /// Simulates a sudden power loss: unsynced files revert to their durable state or
    /// receive torn partial-sector bytes; optionally rotates `BootId` to simulate OS reboot.
    pub fn crash_power_loss(&self, rng: &mut SimRng, reboot: bool) {
        let mut inner = self.inner.lock();
        let mut next_dirty = inner.durable.clone();

        // For any file that had unsynced dirty modifications, randomly simulate either clean
        // reversion to durable bytes OR a torn partial write into the modified region.
        let mut torn_updates = Vec::new();
        for (path, dirty_bytes) in &inner.dirty {
            let durable_bytes = inner.durable.get(path);
            if durable_bytes == Some(dirty_bytes) {
                continue;
            }
            if rng.gen_bool(1, 2) {
                // Inject a torn write into the first differing 4 KiB page.
                let mut torn = durable_bytes.cloned().unwrap_or_else(|| vec![0u8; dirty_bytes.len()]);
                if torn.len() < dirty_bytes.len() {
                    torn.resize(dirty_bytes.len(), 0);
                }
                let diff_pos = torn.iter().zip(dirty_bytes.iter()).position(|(a, b)| a != b).unwrap_or(0);
                let page_start = (diff_pos / 4096) * 4096;
                let page_end = (page_start + 4096).min(dirty_bytes.len());
                if page_end > page_start {
                    let cut = page_start + 1 + rng.gen_range(page_end - page_start);
                    torn[page_start..cut].copy_from_slice(&dirty_bytes[page_start..cut]);
                }
                next_dirty.insert(path.clone(), torn.clone());
                torn_updates.push((path.clone(), torn));
            }
        }
        for (path, torn) in torn_updates {
            inner.durable.insert(path, torn);
        }

        inner.dirty = next_dirty;
        if reboot {
            rng.fill_bytes(&mut inner.boot_id);
        }
    }

    /// Directly corrupts a byte range in a durable file to test torn-slot recovery.
    pub fn corrupt_byte(&self, path: &Path, offset: usize, xor_mask: u8) {
        let mut inner = self.inner.lock();
        if let Some(buf) = inner.dirty.get_mut(path)
            && offset < buf.len()
        {
            buf[offset] ^= xor_mask;
        }
        if let Some(buf) = inner.durable.get_mut(path)
            && offset < buf.len()
        {
            buf[offset] ^= xor_mask;
        }
    }

    fn take_fault(counter: &AtomicU32) -> bool {
        counter.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1)).is_ok()
    }
}

impl Default for SimVfs {
    fn default() -> Self {
        Self::new(BootId([0x11; 16]))
    }
}

impl Vfs for SimVfs {
    fn read_file(&self, path: &Path) -> Result<Vec<u8>, StorageError> {
        let inner = self.inner.lock();
        inner
            .dirty
            .get(path)
            .cloned()
            .ok_or_else(|| StorageError::Io(io::Error::new(io::ErrorKind::NotFound, "sim file not found")))
    }

    fn read_at(&self, path: &Path, offset: u64, len: usize) -> Result<Vec<u8>, StorageError> {
        let inner = self.inner.lock();
        let bytes = inner
            .dirty
            .get(path)
            .ok_or_else(|| StorageError::Io(io::Error::new(io::ErrorKind::NotFound, "sim file not found")))?;
        let start = usize::try_from(offset).unwrap_or(bytes.len()).min(bytes.len());
        let end = start.saturating_add(len).min(bytes.len());
        Ok(bytes[start..end].to_vec())
    }

    fn write_at(&self, path: &Path, offset: u64, data: &[u8]) -> Result<(), StorageError> {
        if Self::take_fault(&self.faults.fail_next_write) {
            return Err(StorageError::Io(io::Error::other("injected sim write_at fault")));
        }
        let mut inner = self.inner.lock();
        let entry = inner.dirty.entry(path.to_path_buf()).or_default();
        let start = usize::try_from(offset).map_err(|_| StorageError::BadValSize { len: data.len() })?;
        let end = start.saturating_add(data.len());
        if entry.len() < end {
            entry.resize(end, 0);
        }
        entry[start..end].copy_from_slice(data);
        Ok(())
    }

    fn write_all(&self, path: &Path, data: &[u8]) -> Result<(), StorageError> {
        if Self::take_fault(&self.faults.fail_next_write) {
            return Err(StorageError::Io(io::Error::other("injected sim write_all fault")));
        }
        let mut inner = self.inner.lock();
        inner.dirty.insert(path.to_path_buf(), data.to_vec());
        Ok(())
    }

    fn sync_file(&self, path: &Path) -> Result<(), StorageError> {
        if Self::take_fault(&self.faults.fail_next_sync) {
            return Err(StorageError::Io(io::Error::other("injected sim sync_file fault")));
        }
        let mut inner = self.inner.lock();
        let bytes = inner
            .dirty
            .get(path)
            .cloned()
            .ok_or_else(|| StorageError::Io(io::Error::new(io::ErrorKind::NotFound, "sim file not found")))?;
        inner.durable.insert(path.to_path_buf(), bytes);
        Ok(())
    }

    fn sync_dir(&self, _dir: &Path) -> Result<(), StorageError> {
        if self.faults.fail_dir_sync.swap(false, Ordering::AcqRel) {
            return Err(StorageError::Io(io::Error::other("injected sim sync_dir fault")));
        }
        Ok(())
    }

    fn file_len(&self, path: &Path) -> Result<u64, StorageError> {
        let inner = self.inner.lock();
        let bytes = inner
            .dirty
            .get(path)
            .ok_or_else(|| StorageError::Io(io::Error::new(io::ErrorKind::NotFound, "sim file not found")))?;
        Ok(bytes.len() as u64)
    }

    fn exists(&self, path: &Path) -> bool {
        self.inner.lock().dirty.contains_key(path)
    }

    fn remove_file(&self, path: &Path) -> Result<(), StorageError> {
        let mut inner = self.inner.lock();
        if inner.dirty.remove(path).is_none() {
            return Err(StorageError::Io(io::Error::new(io::ErrorKind::NotFound, "sim file not found")));
        }
        inner.durable.remove(path);
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), StorageError> {
        let mut inner = self.inner.lock();
        let bytes = inner
            .dirty
            .remove(from)
            .ok_or_else(|| StorageError::Io(io::Error::new(io::ErrorKind::NotFound, "sim file not found")))?;
        inner.dirty.insert(to.to_path_buf(), bytes);
        if let Some(durable) = inner.durable.remove(from) {
            inner.durable.insert(to.to_path_buf(), durable);
        }
        Ok(())
    }

    fn boot_id(&self) -> BootId {
        BootId(self.inner.lock().boot_id)
    }
}
