use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use sha2::{Digest, Sha256};

use crate::error::StorageError;
use crate::kv::{CommitReceipt, DbFlags, KvRead, KvStore, KvWrite, SubDbHandle};
use crate::locks::{MetaLock, OrderedMutexGuard, WriterLock};
use crate::vfs::{BootId, Vfs};

/// File name of the dual-slot 8 KiB durability sidecar inside the database directory.
pub const SYNC_SIDECAR_NAME: &str = "synced.v2";
/// Each durability slot occupies one full 4 KiB page so a torn page write on one slot
/// cannot corrupt its sibling slot.
pub const SYNC_SLOT_BYTES: usize = 4096;
pub const SYNC_FILE_BYTES: usize = SYNC_SLOT_BYTES * 2;

const SLOT_MAGIC: [u8; 8] = *b"ELMDBV2S";
const SLOT_FORMAT_VERSION: u32 = 2;
const SLOT_PAYLOAD_END: usize = 76;
const SLOT_DIGEST_END: usize = 108;

/// Commit durability policy enforced by `DurabilityGate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityMode {
    /// Commits return once in OS page cache; explicit `sync()` (or group-commit timer)
    /// flushes LMDB and writes the dual-slot verified sidecar.
    Grouped,
    /// Every commit synchronously flushes LMDB and writes + fsyncs the dual-slot sidecar.
    Strict,
    /// Ephemeral/test mode: skips sidecar and fsync.
    Unsafe,
}

/// One 4 KiB page-aligned durability record stored in `synced.v2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncSlot {
    pub seq: u64,
    pub txnid: u64,
    pub boot_id: BootId,
    pub roots_sha256: [u8; 32],
}

impl SyncSlot {
    pub fn encode(&self) -> [u8; SYNC_SLOT_BYTES] {
        let mut buf = [0u8; SYNC_SLOT_BYTES];
        buf[0..8].copy_from_slice(&SLOT_MAGIC);
        buf[8..12].copy_from_slice(&SLOT_FORMAT_VERSION.to_le_bytes());
        buf[12..20].copy_from_slice(&self.seq.to_le_bytes());
        buf[20..28].copy_from_slice(&self.txnid.to_le_bytes());
        buf[28..44].copy_from_slice(&self.boot_id.0);
        buf[44..76].copy_from_slice(&self.roots_sha256);
        let digest = Sha256::digest(&buf[..SLOT_PAYLOAD_END]);
        buf[SLOT_PAYLOAD_END..SLOT_DIGEST_END].copy_from_slice(&digest);
        buf
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        if buf.len() < SLOT_DIGEST_END {
            return None;
        }
        if buf[0..8] != SLOT_MAGIC {
            return None;
        }
        let version = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
        if version != SLOT_FORMAT_VERSION {
            return None;
        }
        let expected = Sha256::digest(&buf[..SLOT_PAYLOAD_END]);
        if buf[SLOT_PAYLOAD_END..SLOT_DIGEST_END] != expected[..] {
            return None;
        }
        let seq = u64::from_le_bytes(buf[12..20].try_into().ok()?);
        let txnid = u64::from_le_bytes(buf[20..28].try_into().ok()?);
        let mut boot = [0u8; 16];
        boot.copy_from_slice(&buf[28..44]);
        let mut roots = [0u8; 32];
        roots.copy_from_slice(&buf[44..76]);
        Some(Self { seq, txnid, boot_id: BootId(boot), roots_sha256: roots })
    }
}

/// Outcome of validating the on-disk LMDB state against the dual-slot durability sidecar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryStatus {
    /// Sidecar does not exist yet (fresh database).
    Fresh,
    /// LMDB tip matches the highest valid durable record cleanly.
    Clean,
    /// LMDB holds newer unsynced commits from the *same* OS boot session (grouped mode).
    SameBootUnsyncedTail,
    /// OS rebooted since the last durable record; LMDB matches the verified durable snapshot.
    RebootVerified,
    /// OS rebooted and LMDB holds unverified commits past the durable record, or root hashes diverged.
    UnsyncedAfterReboot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryReport {
    pub status: RecoveryStatus,
    pub durable: Option<SyncSlot>,
    pub observed_txnid: u64,
    pub observed_roots_sha256: [u8; 32],
    pub valid_slots: u8,
}

#[derive(Debug, Default)]
struct WriterState {
    next_seq: u64,
    last_durable: Option<SyncSlot>,
}

/// Sole choke point allowed to commit write transactions and update durability state.
///
/// Enforces:
/// 1. Serialization under `WriterLock` (L3).
/// 2. Dual-slot 4 KiB page-aligned durability records with SHA-256 verification.
/// 3. Fail-stop permanent poisoning if any `sync` fails (no further writes may proceed).
pub struct DurabilityGate<S: KvStore, V: Vfs> {
    store: S,
    vfs: V,
    dir: PathBuf,
    sidecar_path: PathBuf,
    mode: DurabilityMode,
    writer: WriterLock<WriterState>,
    poisoned: AtomicBool,
    poison_reason: MetaLock<Option<String>>,
}

impl<S: KvStore, V: Vfs> DurabilityGate<S, V> {
    pub fn new(store: S, vfs: V, dir: impl AsRef<Path>, mode: DurabilityMode) -> Self {
        let dir = dir.as_ref().to_path_buf();
        let sidecar_path = dir.join(SYNC_SIDECAR_NAME);
        Self {
            store,
            vfs,
            dir,
            sidecar_path,
            mode,
            writer: WriterLock::new(WriterState::default()),
            poisoned: AtomicBool::new(false),
            poison_reason: MetaLock::new(None),
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn vfs(&self) -> &V {
        &self.vfs
    }

    pub fn mode(&self) -> DurabilityMode {
        self.mode
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    /// Validates the dual-slot sidecar against the current `KvStore` root state and initializes
    /// the gate's sequence counter from the highest valid slot.
    pub fn recover(&self) -> Result<RecoveryReport, StorageError> {
        let mut state = self.writer.lock();
        let observed_txnid = self.store.last_txnid()?;
        let observed_roots = self.store.roots_sha256()?;

        let mut valid_slots = 0u8;
        let mut best: Option<SyncSlot> = None;

        if self.vfs.exists(&self.sidecar_path) {
            let bytes = self.vfs.read_file(&self.sidecar_path)?;
            for slot_idx in 0..2 {
                let start = slot_idx * SYNC_SLOT_BYTES;
                if bytes.len() >= start + SLOT_DIGEST_END
                    && let Some(slot) = SyncSlot::decode(&bytes[start..bytes.len().min(start + SYNC_SLOT_BYTES)])
                {
                    valid_slots += 1;
                    if best.is_none_or(|prev| slot.seq > prev.seq) {
                        best = Some(slot);
                    }
                }
            }
        }

        if let Some(slot) = best {
            state.next_seq = slot.seq.saturating_add(1);
            state.last_durable = Some(slot);
        }

        let current_boot = self.vfs.boot_id();
        let status = match best {
            None => RecoveryStatus::Fresh,
            Some(durable) => {
                let roots_match = durable.txnid == observed_txnid && durable.roots_sha256 == observed_roots;
                if roots_match {
                    if durable.boot_id == current_boot { RecoveryStatus::Clean } else { RecoveryStatus::RebootVerified }
                } else if durable.boot_id == current_boot && observed_txnid > durable.txnid {
                    RecoveryStatus::SameBootUnsyncedTail
                } else {
                    RecoveryStatus::UnsyncedAfterReboot
                }
            }
        };

        Ok(RecoveryReport { status, durable: best, observed_txnid, observed_roots_sha256: observed_roots, valid_slots })
    }

    /// Opens a read-only MVCC transaction without acquiring `WriterLock`.
    pub fn begin_read(&self) -> Result<S::RoTxn<'_>, StorageError> {
        self.store.begin_read()
    }

    /// Begins a write transaction guarded by `WriterLock` and fail-stop poison checks.
    pub fn begin_write(&self) -> Result<GatedWriteTxn<'_, S, V>, StorageError> {
        self.check_poisoned()?;
        let guard = self.writer.lock();
        self.check_poisoned()?;
        let txn = self.store.begin_write()?;
        Ok(GatedWriteTxn { gate: self, _writer_guard: guard, txn: Some(txn) })
    }

    /// Flushes LMDB to disk and writes + fsyncs the next alternating 4 KiB durability slot.
    ///
    /// Permanently poisons the gate on any I/O failure so no subsequent write can commit on
    /// an uncertain durability boundary.
    pub fn sync(&self) -> Result<Option<SyncSlot>, StorageError> {
        self.check_poisoned()?;
        let mut guard = self.writer.lock();
        self.check_poisoned()?;
        if self.mode == DurabilityMode::Unsafe {
            return Ok(None);
        }
        let receipt = CommitReceipt { txnid: self.store.last_txnid()?, roots_sha256: self.store.roots_sha256()? };
        let slot = self.persist_durable_locked(&mut guard, receipt)?;
        Ok(Some(slot))
    }

    fn check_poisoned(&self) -> Result<(), StorageError> {
        if self.poisoned.load(Ordering::Acquire) {
            let reason = self.poison_reason.lock().clone().unwrap_or_else(|| "prior durability failure".to_owned());
            return Err(StorageError::Poisoned(reason));
        }
        Ok(())
    }

    fn poison(&self, reason: String) -> StorageError {
        *self.poison_reason.lock() = Some(reason.clone());
        self.poisoned.store(true, Ordering::Release);
        StorageError::DurabilityFailed(reason)
    }

    fn persist_durable_locked(
        &self,
        state: &mut WriterState,
        receipt: CommitReceipt,
    ) -> Result<SyncSlot, StorageError> {
        if let Err(err) = self.store.sync() {
            return Err(self.poison(format!("store sync failed: {err}")));
        }

        let seq = state.next_seq;
        let slot =
            SyncSlot { seq, txnid: receipt.txnid, boot_id: self.vfs.boot_id(), roots_sha256: receipt.roots_sha256 };
        let encoded = slot.encode();
        let is_new = !self.vfs.exists(&self.sidecar_path);
        let offset = (seq % 2) * (SYNC_SLOT_BYTES as u64);

        if let Err(err) = self.vfs.write_at(&self.sidecar_path, offset, &encoded) {
            return Err(self.poison(format!("sidecar write failed: {err}")));
        }
        if let Err(err) = self.vfs.sync_file(&self.sidecar_path) {
            return Err(self.poison(format!("sidecar sync failed: {err}")));
        }
        if is_new && let Err(err) = self.vfs.sync_dir(&self.dir) {
            return Err(self.poison(format!("directory sync failed: {err}")));
        }

        state.next_seq = seq.saturating_add(1);
        state.last_durable = Some(slot);
        Ok(slot)
    }
}

/// Affine write transaction handle whose sole commit path is `DurabilityGate`.
///
/// Dropping without calling `commit()` aborts the transaction cleanly.
pub struct GatedWriteTxn<'a, S: KvStore, V: Vfs> {
    gate: &'a DurabilityGate<S, V>,
    _writer_guard: OrderedMutexGuard<'a, WriterState>,
    txn: Option<S::RwTxn<'a>>,
}

impl<'a, S: KvStore, V: Vfs> GatedWriteTxn<'a, S, V> {
    fn inner(&self) -> &S::RwTxn<'a> {
        self.txn.as_ref().expect("txn present until commit/abort")
    }

    fn inner_mut(&mut self) -> &mut S::RwTxn<'a> {
        self.txn.as_mut().expect("txn present until commit/abort")
    }

    pub fn open_subdb(&mut self, name: &str, flags: DbFlags) -> Result<SubDbHandle, StorageError> {
        self.inner_mut().open_subdb(name, flags)
    }

    pub fn put(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<(), StorageError> {
        self.inner_mut().put(db, key, val)
    }

    pub fn put_no_overwrite(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<bool, StorageError> {
        self.inner_mut().put_no_overwrite(db, key, val)
    }

    pub fn del(&mut self, db: SubDbHandle, key: &[u8]) -> Result<bool, StorageError> {
        self.inner_mut().del(db, key)
    }

    pub fn truncate(&mut self, db: SubDbHandle) -> Result<(), StorageError> {
        self.inner_mut().truncate(db)
    }

    pub fn begin_savepoint(&mut self) -> Result<u32, StorageError> {
        self.inner_mut().begin_savepoint()
    }

    pub fn commit_savepoint(&mut self, depth: u32) -> Result<(), StorageError> {
        self.inner_mut().commit_savepoint(depth)
    }

    pub fn abort_savepoint(&mut self, depth: u32) -> Result<(), StorageError> {
        self.inner_mut().abort_savepoint(depth)
    }

    pub fn savepoint_depth(&self) -> u32 {
        self.inner().savepoint_depth()
    }

    /// Executes `f` inside a native nested savepoint on this gated write transaction.
    pub fn with_savepoint<T, E, F>(&mut self, f: F) -> Result<T, E>
    where
        E: From<StorageError>,
        F: FnOnce(&mut Self) -> Result<T, E>,
    {
        let depth = self.begin_savepoint().map_err(E::from)?;
        match f(self) {
            Ok(value) => {
                self.commit_savepoint(depth).map_err(E::from)?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.abort_savepoint(depth);
                Err(err)
            }
        }
    }

    /// Consumes the write transaction and commits atomically through `DurabilityGate`.
    pub fn commit(mut self) -> Result<CommitReceipt, StorageError> {
        self.gate.check_poisoned()?;
        let txn = self.txn.take().expect("txn present");
        let receipt = txn.commit_raw()?;
        if self.gate.mode == DurabilityMode::Strict {
            self.gate.persist_durable_locked(&mut self._writer_guard, receipt)?;
        }
        Ok(receipt)
    }

    /// Explicitly aborts the transaction.
    pub fn abort(mut self) {
        if let Some(txn) = self.txn.take() {
            txn.abort();
        }
    }
}

impl<'a, S: KvStore, V: Vfs> KvRead for GatedWriteTxn<'a, S, V> {
    type Cursor<'b>
        = <S::RwTxn<'a> as KvRead>::Cursor<'b>
    where
        Self: 'b;

    fn get(&self, db: SubDbHandle, key: &[u8]) -> Result<Option<&[u8]>, StorageError> {
        self.inner().get(db, key)
    }

    fn cursor(&self, db: SubDbHandle) -> Result<Self::Cursor<'_>, StorageError> {
        self.inner().cursor(db)
    }

    fn entries(&self, db: SubDbHandle) -> Result<u64, StorageError> {
        self.inner().entries(db)
    }
}

impl<S: KvStore, V: Vfs> Drop for GatedWriteTxn<'_, S, V> {
    fn drop(&mut self) {
        if let Some(txn) = self.txn.take() {
            txn.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_slot_roundtrips_and_detects_bitflip() {
        let slot = SyncSlot { seq: 42, txnid: 99, boot_id: BootId([0xAB; 16]), roots_sha256: [0xCD; 32] };
        let mut encoded = slot.encode();
        assert_eq!(SyncSlot::decode(&encoded), Some(slot));

        // Flip 1 bit in payload -> digest rejects it.
        encoded[15] ^= 0x01;
        assert_eq!(SyncSlot::decode(&encoded), None);
    }
}
