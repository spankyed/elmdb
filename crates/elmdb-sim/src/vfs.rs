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
    /// Files durably linked and synced on disk.
    durable: BTreeMap<PathBuf, Vec<u8>>,
    /// Files visible in OS page cache / dcache (includes unsynced writes and unsynced renames/unlinks).
    dirty: BTreeMap<PathBuf, Vec<u8>>,
    /// Files whose contents were synced (`sync_file`) even if a new directory entry is not yet `sync_dir`'d.
    synced_contents: BTreeMap<PathBuf, Vec<u8>>,
    boot_id: [u8; 16],
}

/// Deterministic in-memory VFS modeling OS page cache, fsync/dir-sync boundaries,
/// torn 4 KiB writes, and OS reboots.
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
                synced_contents: BTreeMap::new(),
                boot_id: initial_boot.0,
            })),
            faults: Arc::new(SimVfsFaults::default()),
        }
    }

    pub fn faults(&self) -> &SimVfsFaults {
        &self.faults
    }

    /// Simulates a sudden power loss: unsynced writes revert to their durable state or
    /// receive torn partial-page bytes in the first differing 4 KiB page; optionally rotates
    /// `BootId` to simulate OS reboot. Returns how many files received torn page writes.
    pub fn crash_power_loss(&self, rng: &mut SimRng, reboot: bool) -> usize {
        let mut inner = self.inner.lock();
        let mut next_dirty = inner.durable.clone();
        let mut torn_updates = Vec::new();

        for (path, dirty_bytes) in &inner.dirty {
            let durable_bytes = inner.durable.get(path);
            if durable_bytes == Some(dirty_bytes) {
                continue;
            }
            // Only files already linked durably can take torn overwrites; newly created files
            // whose directory wasn't synced may either disappear or appear torn.
            if rng.gen_bool(1, 2) {
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

        let torn_count = torn_updates.len();
        for (path, torn) in torn_updates {
            inner.durable.insert(path, torn);
        }

        inner.dirty = next_dirty;
        inner.synced_contents = inner.durable.clone();
        if reboot {
            rng.fill_bytes(&mut inner.boot_id);
        }
        torn_count
    }

    /// Directly corrupts a byte range in both dirty and durable buffers to test torn-slot recovery.
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
        if let Some(buf) = inner.synced_contents.get_mut(path)
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
        let start = usize::try_from(offset).map_err(|_| StorageError::BadValSize { len: data.len() })?;
        let end = start.checked_add(data.len()).ok_or(StorageError::BadValSize { len: data.len() })?;
        let mut inner = self.inner.lock();
        let entry = inner.dirty.entry(path.to_path_buf()).or_default();
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
        inner.synced_contents.insert(path.to_path_buf(), bytes.clone());
        // If already durably linked in its directory, update durable bytes immediately.
        if inner.durable.contains_key(path) {
            inner.durable.insert(path.to_path_buf(), bytes);
        }
        Ok(())
    }

    fn sync_dir(&self, dir: &Path) -> Result<(), StorageError> {
        if self.faults.fail_dir_sync.swap(false, Ordering::AcqRel) {
            return Err(StorageError::Io(io::Error::other("injected sim sync_dir fault")));
        }
        let mut inner = self.inner.lock();
        // Persist creations/renames/unlinks for files directly inside `dir`.
        let to_remove: Vec<PathBuf> = inner
            .durable
            .keys()
            .filter(|p| p.parent() == Some(dir) && !inner.dirty.contains_key(*p))
            .cloned()
            .collect();
        for path in to_remove {
            inner.durable.remove(&path);
        }
        let to_link: Vec<(PathBuf, Vec<u8>)> = inner
            .synced_contents
            .iter()
            .filter(|(p, _)| p.parent() == Some(dir) && inner.dirty.contains_key(*p))
            .map(|(p, b)| (p.clone(), b.clone()))
            .collect();
        for (path, bytes) in to_link {
            inner.durable.insert(path, bytes);
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
        inner.synced_contents.remove(path);
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), StorageError> {
        let mut inner = self.inner.lock();
        let bytes = inner
            .dirty
            .remove(from)
            .ok_or_else(|| StorageError::Io(io::Error::new(io::ErrorKind::NotFound, "sim file not found")))?;
        inner.dirty.insert(to.to_path_buf(), bytes);
        if let Some(synced) = inner.synced_contents.remove(from) {
            inner.synced_contents.insert(to.to_path_buf(), synced);
        }
        Ok(())
    }

    fn boot_id(&self) -> BootId {
        BootId(self.inner.lock().boot_id)
    }
}
