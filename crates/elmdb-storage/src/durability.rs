use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::error::StorageError;
use crate::kv::{CommitReceipt, DbFlags, KvCursor, KvEntry, KvRead, KvStore, KvWrite, SubDbHandle};
use crate::locks::sync::{AtomicBool, Ordering};
use crate::locks::{MetaLock, OrderedArcMutexGuard, OrderedMutexGuard, WriterLock};
use crate::vfs::{BootId, Vfs};

/// File name of the dual-slot 8 KiB durability sidecar inside the database directory.
pub const SYNC_SIDECAR_NAME: &str = "synced.v2";
const SYNC_LOCK_NAME: &str = "synced.v2.lock";
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
    /// Sidecar does not exist yet and LMDB has zero committed transactions.
    Fresh,
    /// LMDB tip matches the highest valid durable record cleanly in the same boot session.
    Clean,
    /// LMDB holds newer unsynced commits from the *same* OS boot session (grouped mode).
    SameBootUnsyncedTail,
    /// OS rebooted since the last durable record; LMDB matches the verified durable snapshot.
    RebootVerified,
    /// OS rebooted with unverified commits past the durable record, or non-empty DB lacks sidecar.
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
    recovered: bool,
    dir_synced: bool,
    next_seq: u64,
    last_durable: Option<SyncSlot>,
}

/// Sole choke point allowed to commit write transactions and update durability state.
///
/// Enforces:
/// 1. Mandatory `recover()` before any write or sync (via `DurabilityGate::open` or explicit call).
/// 2. Serialization under `WriterLock` (L3).
/// 3. Dual-slot 4 KiB page-aligned durability records with SHA-256 verification.
/// 4. Fail-stop permanent poisoning if any durability sync fails (blocking all further reads & writes).
pub struct DurabilityGate<S: KvStore, V: Vfs> {
    store: S,
    vfs: V,
    dir: PathBuf,
    sidecar_path: PathBuf,
    sidecar_lock_path: PathBuf,
    mode: DurabilityMode,
    read_only: bool,
    writer: WriterLock<WriterState>,
    unacked_reboot: AtomicBool,
    poisoned: AtomicBool,
    poison_reason: MetaLock<Option<String>>,
}

impl<S: KvStore, V: Vfs> DurabilityGate<S, V> {
    /// Constructs a writable `DurabilityGate` and immediately runs `recover()` so slot sequence
    /// numbers and unacknowledged-reboot status are initialized before any write or sync can occur.
    pub fn open(
        store: S,
        vfs: V,
        dir: impl AsRef<Path>,
        mode: DurabilityMode,
    ) -> Result<(Self, RecoveryReport), StorageError> {
        Self::open_with_options(store, vfs, dir, mode, false)
    }

    /// Constructs a `DurabilityGate` with explicit `read_only` enforcement and immediately runs `recover()`.
    pub fn open_with_options(
        store: S,
        vfs: V,
        dir: impl AsRef<Path>,
        mode: DurabilityMode,
        read_only: bool,
    ) -> Result<(Self, RecoveryReport), StorageError> {
        let gate = Self::new_unrecovered_with_options(store, vfs, dir, mode, read_only);
        let report = gate.recover()?;
        Ok((gate, report))
    }

    /// Constructs an unrecovered writable gate; callers must invoke `recover()` before `begin_write()` or `sync()`.
    pub fn new_unrecovered(store: S, vfs: V, dir: impl AsRef<Path>, mode: DurabilityMode) -> Self {
        Self::new_unrecovered_with_options(store, vfs, dir, mode, false)
    }

    /// Constructs an unrecovered gate with explicit `read_only` enforcement.
    pub fn new_unrecovered_with_options(
        store: S,
        vfs: V,
        dir: impl AsRef<Path>,
        mode: DurabilityMode,
        read_only: bool,
    ) -> Self {
        let dir = dir.as_ref().to_path_buf();
        let sidecar_path = dir.join(SYNC_SIDECAR_NAME);
        let sidecar_lock_path = dir.join(SYNC_LOCK_NAME);
        let scope = store.lock_scope_id();
        Self {
            store,
            vfs,
            dir,
            sidecar_path,
            sidecar_lock_path,
            mode,
            read_only,
            writer: WriterLock::new_in_scope(scope, WriterState::default()),
            unacked_reboot: AtomicBool::new(false),
            poisoned: AtomicBool::new(false),
            poison_reason: MetaLock::new_in_scope(scope, None),
        }
    }

    pub fn mode(&self) -> DurabilityMode {
        self.mode
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn has_unacked_reboot(&self) -> bool {
        self.unacked_reboot.load(Ordering::Acquire)
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned.load(Ordering::Acquire)
    }

    pub fn map_size(&self) -> usize {
        self.store.map_size()
    }

    pub fn page_size(&self) -> usize {
        self.store.page_size()
    }

    /// Creates a consistent compacted copy of the store in `target_dir` and writes + fsyncs
    /// a verified `synced.v2` sidecar matching the copied `data.mdb` active meta page.
    /// Automatically adopts cross-process `MDB_MAP_RESIZED` and retries if needed.
    ///
    /// Fails closed if `unacked_reboot` is set so an unacknowledged `UnsyncedAfterReboot`
    /// database cannot be silently laundered into a `Clean` backup.
    pub fn copy_compacted(&self, target_dir: &Path) -> Result<(), StorageError> {
        let receipt = loop {
            self.check_poisoned()?;
            if self.unacked_reboot.load(Ordering::Acquire) {
                return Err(StorageError::DurabilityFailed(
                    "cannot create backup while database has an unacknowledged reboot; call acknowledge_restore() first"
                        .into(),
                ));
            }
            match self.store.copy_compacted(target_dir) {
                Ok(r) => break r,
                Err(StorageError::MapResized) => {
                    let _ = self.vfs.remove_file(&target_dir.join("data.mdb"));
                    self.adopt_resized_map_checked()?;
                    continue;
                }
                Err(err) => return Err(err),
            }
        };
        let slot =
            SyncSlot { seq: 1, txnid: receipt.txnid, boot_id: self.vfs.boot_id(), roots_sha256: receipt.roots_sha256 };
        let encoded = slot.encode();
        let mut sidecar = vec![0u8; SYNC_FILE_BYTES];
        sidecar[0..SYNC_SLOT_BYTES].copy_from_slice(&encoded);
        sidecar[SYNC_SLOT_BYTES..SYNC_FILE_BYTES].copy_from_slice(&encoded);
        let sidecar_path = target_dir.join(SYNC_SIDECAR_NAME);
        self.vfs.write_at(&sidecar_path, 0, &sidecar)?;
        self.vfs.sync_file(&sidecar_path)?;
        self.vfs.sync_dir(target_dir)?;
        Ok(())
    }

    pub fn last_txnid(&self) -> Result<u64, StorageError> {
        self.check_poisoned()?;
        self.store.last_txnid()
    }

    pub fn open_subdb_ro(
        &self,
        txn: &S::RoTxn<'_>,
        name: &str,
        flags: DbFlags,
    ) -> Result<Option<SubDbHandle>, StorageError> {
        self.check_poisoned()?;
        self.store.open_subdb_ro(txn, name, flags)
    }

    /// Pre-opens committed named sub-DBs into the environment handle cache while holding L2+L3
    /// so `mdb_dbi_open` never races with an active write transaction's `open_subdb`.
    pub fn ensure_subdbs_open(&self, names: &[(&str, DbFlags)]) -> Result<(), StorageError> {
        if names.is_empty() {
            return Ok(());
        }
        loop {
            self.check_poisoned()?;
            let map_guard = self.store.lock_map_read();
            let writer_guard = self.writer.lock();
            self.check_poisoned()?;
            match self.store.ensure_subdbs_open(&map_guard, names) {
                Ok(()) => return Ok(()),
                Err(StorageError::MapResized) => {
                    drop(writer_guard);
                    drop(map_guard);
                    self.adopt_resized_map_checked()?;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Grows the underlying storage map (doubling or growing to `min_size`) under L2 `MapGate::write()`.
    /// Caller MUST NOT hold any open read or write transaction on the calling thread.
    pub fn grow_map(&self, min_size: Option<usize>) -> Result<usize, StorageError> {
        self.check_poisoned()?;
        if self.read_only {
            return Err(StorageError::ReadOnly);
        }
        if self.mode == DurabilityMode::Grouped && self.unacked_reboot.load(Ordering::Acquire) {
            return Err(StorageError::DurabilityFailed(
                "cannot grow map while Grouped database has an unacknowledged reboot; call acknowledge_restore() first"
                    .into(),
            ));
        }
        // Hold L2 `MapGate::write()` -> L3 `WriterLock` across the entire seal -> unpin -> remap -> re-pin
        // -> verify sequence so no local writer can slip between sealing and re-pinning.
        let map_write = self.store.lock_map_write();
        let mut guard = self.writer.lock();
        self.check_poisoned()?;
        let pin_after = self.mode == DurabilityMode::Grouped && guard.recovered;
        if pin_after {
            let _ = self.persist_durable_locked(&mut guard, MapWitness::Write(&map_write), None, false)?;
        }
        let size = match self.store.grow_map_locked(&map_write, min_size, pin_after) {
            Ok(size) => size,
            Err(StorageError::Corrupted(msg)) => return Err(self.poison_corrupted(msg)),
            Err(err) if pin_after => return Err(self.poison(format!("Grouped map grow/re-pin failed: {err}"))),
            Err(err) => return Err(err),
        };
        if pin_after {
            self.verify_and_seal_after_remap_locked(&mut guard, &map_write)?;
        }
        drop(guard);
        drop(map_write);
        Ok(size)
    }

    fn adopt_resized_map_checked(&self) -> Result<usize, StorageError> {
        Ok(self.adopt_resized_map_internal(false)?.0)
    }

    fn adopt_and_pin_grouped(&self, force_pin: bool) -> Result<Option<SyncSlot>, StorageError> {
        Ok(self.adopt_resized_map_internal(force_pin)?.1)
    }

    fn adopt_resized_map_internal(&self, force_pin: bool) -> Result<(usize, Option<SyncSlot>), StorageError> {
        // Hold L2 `MapGate::write()` -> L3 `WriterLock` across seal -> unpin -> adopt -> re-pin -> verify
        // so neither local writers nor cross-process commits can leave the pin ahead of `synced.v2`.
        let map_write = self.store.lock_map_write();
        let mut guard = self.writer.lock();
        self.check_poisoned()?;
        let pin_after = !self.read_only
            && self.mode == DurabilityMode::Grouped
            && guard.recovered
            && (force_pin || !self.unacked_reboot.load(Ordering::Acquire));
        if pin_after {
            let _ = self.persist_durable_locked(&mut guard, MapWitness::Write(&map_write), None, false)?;
        }
        let size = match self.store.adopt_resized_map_locked(&map_write, pin_after) {
            Ok(size) => size,
            Err(StorageError::Corrupted(msg)) => return Err(self.poison_corrupted(msg)),
            Err(err) if pin_after => return Err(self.poison(format!("Grouped adopt_resized_map/re-pin failed: {err}"))),
            Err(err) => return Err(err),
        };
        if pin_after {
            self.verify_and_seal_after_remap_locked(&mut guard, &map_write)?;
        }
        let sealed = guard.last_durable;
        drop(guard);
        drop(map_write);
        Ok((size, sealed))
    }

    /// Runs while holding L2 `MapGate::write()` + L3 `WriterLock` immediately after a Grouped remap:
    /// `grow_map_locked` / `adopt_resized_map_locked` already acquired a fresh durable pin `P` right
    /// after `mdb_env_set_mapsize`. If a cross-process peer committed during the remap (`receipt.txnid > sealed_txnid`),
    /// seal the newer tip `S >= P` (`refresh_pin = false` because `P <= S` already protects every page of `S`).
    fn verify_and_seal_after_remap_locked(
        &self,
        guard: &mut WriterState,
        map_write: &S::MapWriteGuard<'_>,
    ) -> Result<(), StorageError> {
        let receipt = match self.store.active_receipt() {
            Ok(r) => r,
            Err(err) => return Err(self.poison(format!("reading active meta after Grouped remap failed: {err}"))),
        };
        let sealed_txnid = guard.last_durable.map_or(0, |s| s.txnid);
        if receipt.txnid > sealed_txnid {
            let _ = self.persist_durable_locked(guard, MapWitness::Write(map_write), Some(receipt.txnid), false)?;
        }
        Ok(())
    }

    fn read_sidecar_best(&self) -> Result<(bool, u8, Option<SyncSlot>, bool), StorageError> {
        if !self.vfs.exists(&self.sidecar_path) {
            return Ok((false, 0, None, false));
        }
        let bytes = self.vfs.read_file(&self.sidecar_path)?;
        if bytes.is_empty() {
            // A 0-byte sidecar can only come from a crash before the first slot was written/renamed.
            return Ok((true, 0, None, true));
        }
        let mut valid_slots = 0u8;
        let mut best: Option<SyncSlot> = None;
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
        Ok((true, valid_slots, best, false))
    }

    /// Validates the dual-slot sidecar against the current `KvStore` root state and initializes
    /// the gate's sequence counter from the highest valid slot.
    pub fn recover(&self) -> Result<RecoveryReport, StorageError> {
        self.check_poisoned()?;
        let map_guard = self.store.lock_map_read();
        let mut state = self.writer.lock();
        self.check_poisoned()?;

        // Serialize sidecar inspection against concurrent writers in other processes.
        // Read-only opens NEVER create `synced.v2.lock` (they take a shared lock only if it exists).
        // Tolerate read-only mounts (`PermissionDenied` / `ReadOnlyFilesystem`) gracefully.
        let sidecar_lock = if self.mode != DurabilityMode::Unsafe {
            let lock_res = if self.read_only {
                self.vfs.lock_file_shared_if_exists(&self.sidecar_lock_path)
            } else {
                self.vfs.lock_file(&self.sidecar_lock_path).map(Some)
            };
            match lock_res {
                Ok(guard) => guard,
                Err(StorageError::Io(ref e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
                    ) =>
                {
                    None
                }
                Err(err) => return Err(err),
            }
        } else {
            None
        };

        // Read the sidecar strictly BEFORE `active_receipt()`! Because writers always commit `data.mdb`
        // before updating `synced.v2`, reading the sidecar first guarantees `durable.txnid <= observed_txnid`
        // even for read-only opens when `synced.v2.lock` does not yet exist (`sidecar_lock.is_none()`).
        let (sidecar_exists, valid_slots, best, empty_file) = self.read_sidecar_best()?;
        if sidecar_exists && !empty_file && valid_slots == 0 {
            return Err(StorageError::Corrupted(
                "durability sidecar exists but neither 4 KiB slot has a valid checksum".into(),
            ));
        }

        let receipt = self.store.active_receipt()?;
        let observed_txnid = receipt.txnid;
        let observed_roots = receipt.roots_sha256;

        let current_boot = self.vfs.boot_id();
        let status = match best {
            None => {
                if observed_txnid == 0 && !empty_file {
                    RecoveryStatus::Fresh
                } else {
                    RecoveryStatus::UnsyncedAfterReboot
                }
            }
            Some(durable) => {
                if observed_txnid < durable.txnid {
                    return Err(StorageError::Corrupted(format!(
                        "storage rollback detected: observed txnid {observed_txnid} < durable txnid {}",
                        durable.txnid
                    )));
                }
                if observed_txnid == durable.txnid {
                    if observed_roots != durable.roots_sha256 {
                        return Err(StorageError::Corrupted(format!(
                            "root checksum mismatch at durable txnid {observed_txnid}"
                        )));
                    }
                    if durable.boot_id == current_boot { RecoveryStatus::Clean } else { RecoveryStatus::RebootVerified }
                } else if durable.boot_id == current_boot {
                    RecoveryStatus::SameBootUnsyncedTail
                } else {
                    RecoveryStatus::UnsyncedAfterReboot
                }
            }
        };

        if let Some(slot) = best {
            state.next_seq = slot.seq.saturating_add(1);
            state.last_durable = Some(slot);
        }
        self.unacked_reboot.store(status == RecoveryStatus::UnsyncedAfterReboot, Ordering::Release);
        state.recovered = true;

        // Release `_sidecar_lock` BEFORE `persist_durable_locked` (which re-acquires it).
        drop(sidecar_lock);

        let mut need_adopt = false;
        if self.mode == DurabilityMode::Grouped && !self.read_only {
            match status {
                RecoveryStatus::SameBootUnsyncedTail => {
                    // Seal the same-boot unsynced tail FIRST so the pinned snapshot is genuinely durable
                    // (otherwise pinning `D+k` while `synced.v2` still says `D` leaves `D`'s pages unpinned).
                    let (_, adopt) = self.persist_durable_locked(
                        &mut state,
                        MapWitness::Read(&map_guard),
                        Some(observed_txnid),
                        true,
                    )?;
                    need_adopt = adopt;
                }
                RecoveryStatus::Clean | RecoveryStatus::RebootVerified => {
                    match self.store.prepare_durable_pin(&map_guard) {
                        Ok(tok) => {
                            let pin_txnid = S::pin_txnid(&tok);
                            self.store.install_durable_pin(tok);
                            if pin_txnid > observed_txnid {
                                // A peer process committed between `active_receipt()` and `prepare_durable_pin()`;
                                // `tok` (at `pin_txnid`) already protects every snapshot >= `pin_txnid`, so seal
                                // the current tip (`>= pin_txnid`) without leaving `observed_txnid` unpinned.
                                let _ = self.persist_durable_locked(
                                    &mut state,
                                    MapWitness::Read(&map_guard),
                                    Some(pin_txnid),
                                    false,
                                )?;
                            }
                        }
                        Err(StorageError::MapResized) => need_adopt = true,
                        Err(err) => {
                            return Err(self.poison(format!("failed to acquire Grouped durable pin: {err}")));
                        }
                    }
                }
                RecoveryStatus::Fresh | RecoveryStatus::UnsyncedAfterReboot => {}
            }
        }
        drop(state);
        drop(map_guard);
        if need_adopt {
            self.adopt_and_pin_grouped(false)?;
        }

        Ok(RecoveryReport { status, durable: best, observed_txnid, observed_roots_sha256: observed_roots, valid_slots })
    }

    /// Opens a read-only MVCC transaction without acquiring `WriterLock`, automatically adopting
    /// external cross-process map resizes (`MDB_MAP_RESIZED`).
    /// Fails closed if the durability gate is poisoned.
    pub fn begin_read(&self) -> Result<S::RoTxn<'_>, StorageError> {
        for _ in 0..8 {
            self.check_poisoned()?;
            match self.store.begin_read() {
                Ok(ro) => return Ok(ro),
                Err(StorageError::MapResized) => {
                    self.adopt_resized_map_checked()?;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        Err(StorageError::MapResized)
    }

    /// Begins a write transaction guarded by L2 `MapGate` (acquired before L3 `WriterLock`),
    /// proactive map growth, recovery check, and fail-stop poison checks.
    pub fn begin_write(&self) -> Result<GatedWriteTxn<'_, S, V>, StorageError> {
        if self.read_only {
            return Err(StorageError::ReadOnly);
        }
        for _ in 0..8 {
            self.check_poisoned()?;
            if let Some(target) = self.store.needs_map_growth()? {
                self.grow_map(Some(target))?;
            }
            let map_guard = self.store.lock_map_read();
            let guard = self.writer.lock();
            self.check_poisoned()?;
            if !guard.recovered {
                return Err(StorageError::InvalidState(
                    "DurabilityGate::recover() must succeed before beginning a write transaction".into(),
                ));
            }
            match self.store.begin_write(map_guard) {
                Ok(txn) => {
                    return Ok(GatedWriteTxn {
                        txn: Some(RwTxnRef::Borrowed(txn)),
                        writer_guard: WriterGuardRef::Borrowed(guard),
                        gate: GateRef::Borrowed(self),
                    });
                }
                Err(StorageError::MapResized) => {
                    drop(guard);
                    self.adopt_resized_map_checked()?;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        Err(StorageError::MapResized)
    }

    /// Begins an owned `'static` (but `!Send`) write transaction guarded by L2 `MapGate` + L3 `WriterLock`,
    /// allowing callback-scoped synchronous `transact(fn)` without any `unsafe` lifetime erasure.
    pub fn begin_write_owned(self: &Arc<Self>) -> Result<GatedWriteTxn<'static, S, V>, StorageError>
    where
        S: 'static,
        V: 'static,
    {
        if self.read_only {
            return Err(StorageError::ReadOnly);
        }
        for _ in 0..8 {
            self.check_poisoned()?;
            if let Some(target) = self.store.needs_map_growth()? {
                self.grow_map(Some(target))?;
            }
            let map_guard = self.store.lock_map_read();
            let guard = self.writer.lock_arc();
            self.check_poisoned()?;
            if !guard.recovered {
                return Err(StorageError::InvalidState(
                    "DurabilityGate::recover() must succeed before beginning a write transaction".into(),
                ));
            }
            match self.store.begin_write_owned(map_guard) {
                Ok(txn) => {
                    return Ok(GatedWriteTxn {
                        txn: Some(RwTxnRef::Owned(txn)),
                        writer_guard: WriterGuardRef::Owned(guard),
                        gate: GateRef::Owned(Arc::clone(self)),
                    });
                }
                Err(StorageError::MapResized) => {
                    drop(guard);
                    self.adopt_resized_map_checked()?;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        Err(StorageError::MapResized)
    }

    /// Flushes LMDB to disk and writes + fsyncs the next alternating 4 KiB durability slot.
    ///
    /// Refuses to overwrite `synced.v2` while `unacked_reboot` is set (returning `Ok(None)`),
    /// so routine `close()` / `flush()` cannot silently erase an `UnsyncedAfterReboot` recovery
    /// state before the caller explicitly invokes `acknowledge_restore()`.
    ///
    /// Permanently poisons the gate on any failure so no subsequent read or write can proceed on
    /// an uncertain durability boundary.
    pub fn sync(&self) -> Result<Option<SyncSlot>, StorageError> {
        self.check_poisoned()?;
        if self.read_only {
            return Err(StorageError::ReadOnly);
        }
        let map_guard = self.store.lock_map_read();
        let mut guard = self.writer.lock();
        self.check_poisoned()?;
        if !guard.recovered {
            return Err(StorageError::InvalidState(
                "DurabilityGate::recover() must succeed before calling sync()".into(),
            ));
        }
        if self.mode == DurabilityMode::Unsafe {
            return Ok(None);
        }
        if self.unacked_reboot.load(Ordering::Acquire) {
            // Flush LMDB pages so Grouped-mode writes are not lost, without sealing `synced.v2`
            // until the caller explicitly invokes `acknowledge_restore()`.
            if let Err(err) = self.store.sync(&map_guard) {
                return Err(self.poison(format!("store sync failed: {err}")));
            }
            drop(guard);
            drop(map_guard);
            return Ok(None);
        }
        let (slot, need_adopt) = self.persist_durable_locked(&mut guard, MapWitness::Read(&map_guard), None, true)?;
        drop(guard);
        drop(map_guard);
        if need_adopt {
            return self.adopt_and_pin_grouped(false);
        }
        Ok(slot)
    }

    /// Explicitly acknowledges an `UnsyncedAfterReboot` recovery state, flushing LMDB and sealing
    /// the current root state into `synced.v2`.
    ///
    /// Only after the sidecar write and fsync succeed is `unacked_reboot` cleared.
    pub fn acknowledge_restore(&self) -> Result<Option<SyncSlot>, StorageError> {
        self.check_poisoned()?;
        if self.read_only {
            return Err(StorageError::ReadOnly);
        }
        let map_guard = self.store.lock_map_read();
        let mut guard = self.writer.lock();
        self.check_poisoned()?;
        if !guard.recovered {
            return Err(StorageError::InvalidState(
                "DurabilityGate::recover() must succeed before calling acknowledge_restore()".into(),
            ));
        }
        if self.mode == DurabilityMode::Unsafe {
            self.unacked_reboot.store(false, Ordering::Release);
            return Ok(None);
        }
        let (slot, need_adopt) = self.persist_durable_locked(&mut guard, MapWitness::Read(&map_guard), None, true)?;
        if !need_adopt {
            self.unacked_reboot.store(false, Ordering::Release);
        }
        drop(guard);
        drop(map_guard);
        if need_adopt {
            // Pass `force_pin = true` so `adopt_resized_map_internal` seals and pins atomically under
            // L2 write + L3 even while `unacked_reboot` is still true, clearing it only on success.
            let adopted_slot = self.adopt_and_pin_grouped(true)?;
            self.unacked_reboot.store(false, Ordering::Release);
            return Ok(adopted_slot);
        }
        Ok(slot)
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

    fn poison_corrupted(&self, reason: String) -> StorageError {
        *self.poison_reason.lock() = Some(reason.clone());
        self.poisoned.store(true, Ordering::Release);
        StorageError::Corrupted(reason)
    }

    fn persist_durable_locked<'a, 'b>(
        &self,
        state: &mut WriterState,
        map_witness: MapWitness<'a, 'b, S>,
        hint_txnid: Option<u64>,
        refresh_pin: bool,
    ) -> Result<(Option<SyncSlot>, bool), StorageError> {
        // 1. Acquire cross-process sidecar lock FIRST so no peer can interleave slot updates.
        let _sidecar_lock = match self.vfs.lock_file(&self.sidecar_lock_path) {
            Ok(guard) => guard,
            Err(err) => return Err(self.poison(format!("sidecar lock failed: {err}"))),
        };

        // 2. For Grouped mode, stage a fresh `MDB_RDONLY` durable pin BEFORE reading `active_receipt()`
        //    and syncing, while keeping any previous pin alive!
        //    Because LMDB's freelist never reuses pages freed at `T >= oldest_reader_txnid`, opening
        //    `staged_pin` at `P` before reading `receipt.txnid = S` guarantees `P <= S` by construction
        //    (even if another process commits concurrently during `sync()`), so every page of `S` is
        //    protected from freelist reuse, and any failure leaves the prior pin intact.
        let staged_pin = if refresh_pin && self.mode == DurabilityMode::Grouped && !self.read_only {
            match map_witness.prepare_durable_pin(&self.store) {
                Ok(tok) => Some(tok),
                Err(StorageError::MapResized) => return Ok((None, true)),
                Err(err) => return Err(self.poison(format!("preparing Grouped durable pin failed: {err}"))),
            }
        } else {
            None
        };

        // 3. Read `(txnid, roots_sha256)` from the active meta page BEFORE `store.sync(...)`!
        //    Because LMDB writes all data pages before the meta page, calling `fdatasync(data.mdb)`
        //    strictly AFTER reading `receipt` guarantees every byte of `receipt.txnid` (and every
        //    concurrent peer commit included in `receipt`) is durable on disk before `synced.v2`
        //    records `receipt.txnid`. (Prevents multi-process durability inversion on power loss.)
        let receipt = match self.store.active_receipt() {
            Ok(r) => r,
            Err(err) => return Err(self.poison(format!("reading active meta page during sync failed: {err}"))),
        };
        if let Some(ref tok) = staged_pin
            && S::pin_txnid(tok) > receipt.txnid
        {
            return Err(self.poison_corrupted(format!(
                "durable pin txnid {} exceeded active meta txnid {}",
                S::pin_txnid(tok),
                receipt.txnid
            )));
        }
        if let Some(expected_min) = hint_txnid
            && receipt.txnid < expected_min
        {
            return Err(self.poison_corrupted(format!(
                "active meta txnid {} regressed below committed txnid {expected_min}",
                receipt.txnid
            )));
        }

        // 4. Flush `data.mdb` to stable storage while holding L2 `map_witness` (prevents `munmap` race).
        if let Err(err) = map_witness.sync(&self.store) {
            return Err(self.poison(format!("store sync failed: {err}")));
        }

        // 5. Inspect existing sidecar under `_sidecar_lock`: fail closed if corrupted or rolled back!
        let (sidecar_exists, valid_slots, disk_best, empty_file) = match self.read_sidecar_best() {
            Ok(res) => res,
            Err(err) => return Err(self.poison(format!("reading sidecar during sync failed: {err}"))),
        };
        if sidecar_exists && !empty_file && valid_slots == 0 {
            return Err(
                self.poison_corrupted("durability sidecar exists but neither 4 KiB slot has a valid checksum".into())
            );
        }
        if let Some(best) = disk_best
            && best.txnid > receipt.txnid
        {
            return Err(self.poison_corrupted(format!(
                "storage rollback detected during sync: active txnid {} < durable sidecar txnid {}",
                receipt.txnid, best.txnid
            )));
        }

        let seq = match disk_best {
            Some(best) => best.seq.saturating_add(1).max(state.next_seq),
            None => state.next_seq,
        };
        let slot =
            SyncSlot { seq, txnid: receipt.txnid, boot_id: self.vfs.boot_id(), roots_sha256: receipt.roots_sha256 };
        let encoded = slot.encode();

        if !sidecar_exists || empty_file {
            // Atomic initial sidecar creation via sibling temp file + fsync + rename + dir sync
            // so a crash can never leave a 0-byte or half-written initial `synced.v2`.
            let tmp_path = self.dir.join(format!(".{SYNC_SIDECAR_NAME}.tmp.{}", std::process::id()));
            let mut full = vec![0u8; SYNC_FILE_BYTES];
            let start = ((seq % 2) as usize) * SYNC_SLOT_BYTES;
            full[start..start + SYNC_SLOT_BYTES].copy_from_slice(&encoded);
            if let Err(err) = self.vfs.write_all(&tmp_path, &full) {
                return Err(self.poison(format!("initial sidecar write failed: {err}")));
            }
            if let Err(err) = self.vfs.sync_file(&tmp_path) {
                let _ = self.vfs.remove_file(&tmp_path);
                return Err(self.poison(format!("initial sidecar sync failed: {err}")));
            }
            if let Err(err) = self.vfs.rename(&tmp_path, &self.sidecar_path) {
                let _ = self.vfs.remove_file(&tmp_path);
                return Err(self.poison(format!("initial sidecar rename failed: {err}")));
            }
            if let Err(err) = self.vfs.sync_dir(&self.dir) {
                return Err(self.poison(format!("directory sync failed: {err}")));
            }
            state.dir_synced = true;
        } else {
            let need_dir_sync = !state.dir_synced;
            let offset = (seq % 2) * (SYNC_SLOT_BYTES as u64);
            if let Err(err) = self.vfs.write_at(&self.sidecar_path, offset, &encoded) {
                return Err(self.poison(format!("sidecar write failed: {err}")));
            }
            if let Err(err) = self.vfs.sync_file(&self.sidecar_path) {
                return Err(self.poison(format!("sidecar sync failed: {err}")));
            }
            if need_dir_sync {
                if let Err(err) = self.vfs.sync_dir(&self.dir) {
                    return Err(self.poison(format!("directory sync failed: {err}")));
                }
                state.dir_synced = true;
            }
        }

        state.next_seq = seq.saturating_add(1);
        state.last_durable = Some(slot);
        // 6. Install the staged durable pin (at `P <= receipt.txnid`) and drop any prior pin.
        if let Some(tok) = staged_pin {
            self.store.install_durable_pin(tok);
        }
        Ok((Some(slot), false))
    }
}

enum MapWitness<'a, 'b, S: KvStore + 'b> {
    Read(&'a S::MapReadGuard),
    Write(&'a S::MapWriteGuard<'b>),
}

impl<'a, 'b, S: KvStore + 'b> MapWitness<'a, 'b, S> {
    fn sync(&self, store: &S) -> Result<(), StorageError> {
        match self {
            Self::Read(g) => store.sync(g),
            Self::Write(g) => store.sync_under_write(g),
        }
    }

    fn prepare_durable_pin<'c>(&'c self, store: &'c S) -> Result<S::DurablePinToken<'c>, StorageError>
    where
        'a: 'c,
    {
        match self {
            Self::Read(g) => store.prepare_durable_pin(g),
            Self::Write(g) => store.prepare_durable_pin_under_write(g),
        }
    }
}

enum GateRef<'a, S: KvStore, V: Vfs> {
    Borrowed(&'a DurabilityGate<S, V>),
    Owned(Arc<DurabilityGate<S, V>>),
}

impl<S: KvStore, V: Vfs> GateRef<'_, S, V> {
    fn as_gate(&self) -> &DurabilityGate<S, V> {
        match self {
            Self::Borrowed(g) => g,
            Self::Owned(g) => g,
        }
    }
}

enum WriterGuardRef<'a> {
    Borrowed(OrderedMutexGuard<'a, WriterState>),
    Owned(OrderedArcMutexGuard<WriterState>),
}

impl WriterGuardRef<'_> {
    fn state_mut(&mut self) -> &mut WriterState {
        match self {
            Self::Borrowed(g) => g,
            Self::Owned(g) => g,
        }
    }
}

enum RwTxnRef<'a, S: KvStore + 'a> {
    Borrowed(S::RwTxn<'a>),
    Owned(S::OwnedRwTxn),
}

pub enum GatedCursor<C1, C2> {
    Borrowed(C1),
    Owned(C2),
}

impl<'txn, C1: KvCursor<'txn>, C2: KvCursor<'txn>> KvCursor<'txn> for GatedCursor<C1, C2> {
    fn first(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        match self {
            Self::Borrowed(c) => c.first(),
            Self::Owned(c) => c.first(),
        }
    }

    fn last(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        match self {
            Self::Borrowed(c) => c.last(),
            Self::Owned(c) => c.last(),
        }
    }

    fn seek_ge(&mut self, key: &[u8]) -> Result<Option<KvEntry<'txn>>, StorageError> {
        match self {
            Self::Borrowed(c) => c.seek_ge(key),
            Self::Owned(c) => c.seek_ge(key),
        }
    }

    fn seek_exact(&mut self, key: &[u8]) -> Result<Option<&'txn [u8]>, StorageError> {
        match self {
            Self::Borrowed(c) => c.seek_exact(key),
            Self::Owned(c) => c.seek_exact(key),
        }
    }

    fn next(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        match self {
            Self::Borrowed(c) => c.next(),
            Self::Owned(c) => c.next(),
        }
    }

    fn prev(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        match self {
            Self::Borrowed(c) => c.prev(),
            Self::Owned(c) => c.prev(),
        }
    }
}

/// Affine write transaction handle whose sole commit path is `DurabilityGate`.
///
/// Field drop order ensures:
/// 1. `txn` aborts any live `MDB_txn` first (and releases its owned L2 `MapReadGuard`),
/// 2. `writer_guard` releases L3 `WriterLock`,
/// 3. `gate` drops the `DurabilityGate` reference.
pub struct GatedWriteTxn<'a, S: KvStore + 'a, V: Vfs> {
    txn: Option<RwTxnRef<'a, S>>,
    writer_guard: WriterGuardRef<'a>,
    gate: GateRef<'a, S, V>,
}

impl<'a, S: KvStore + 'a, V: Vfs> GatedWriteTxn<'a, S, V> {
    fn inner(&self) -> &RwTxnRef<'a, S> {
        self.txn.as_ref().expect("txn present until commit/abort")
    }

    fn inner_mut(&mut self) -> &mut RwTxnRef<'a, S> {
        self.txn.as_mut().expect("txn present until commit/abort")
    }

    pub fn open_subdb(&mut self, name: &str, flags: DbFlags) -> Result<SubDbHandle, StorageError> {
        match self.inner_mut() {
            RwTxnRef::Borrowed(t) => t.open_subdb(name, flags),
            RwTxnRef::Owned(t) => t.open_subdb(name, flags),
        }
    }

    pub fn put(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<(), StorageError> {
        match self.inner_mut() {
            RwTxnRef::Borrowed(t) => t.put(db, key, val),
            RwTxnRef::Owned(t) => t.put(db, key, val),
        }
    }

    pub fn put_no_overwrite(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<bool, StorageError> {
        match self.inner_mut() {
            RwTxnRef::Borrowed(t) => t.put_no_overwrite(db, key, val),
            RwTxnRef::Owned(t) => t.put_no_overwrite(db, key, val),
        }
    }

    pub fn del(&mut self, db: SubDbHandle, key: &[u8]) -> Result<bool, StorageError> {
        match self.inner_mut() {
            RwTxnRef::Borrowed(t) => t.del(db, key),
            RwTxnRef::Owned(t) => t.del(db, key),
        }
    }

    pub fn truncate(&mut self, db: SubDbHandle) -> Result<(), StorageError> {
        match self.inner_mut() {
            RwTxnRef::Borrowed(t) => t.truncate(db),
            RwTxnRef::Owned(t) => t.truncate(db),
        }
    }

    pub fn begin_savepoint(&mut self) -> Result<u32, StorageError> {
        match self.inner_mut() {
            RwTxnRef::Borrowed(t) => t.begin_savepoint(),
            RwTxnRef::Owned(t) => t.begin_savepoint(),
        }
    }

    pub fn commit_savepoint(&mut self, depth: u32) -> Result<(), StorageError> {
        match self.inner_mut() {
            RwTxnRef::Borrowed(t) => t.commit_savepoint(depth),
            RwTxnRef::Owned(t) => t.commit_savepoint(depth),
        }
    }

    pub fn abort_savepoint(&mut self, depth: u32) -> Result<(), StorageError> {
        match self.inner_mut() {
            RwTxnRef::Borrowed(t) => t.abort_savepoint(depth),
            RwTxnRef::Owned(t) => t.abort_savepoint(depth),
        }
    }

    pub fn savepoint_depth(&self) -> u32 {
        match self.inner() {
            RwTxnRef::Borrowed(t) => t.savepoint_depth(),
            RwTxnRef::Owned(t) => t.savepoint_depth(),
        }
    }

    /// Executes `f` inside a native nested savepoint on this gated write transaction,
    /// unwinding any nested savepoints opened inside `f` on error OR panic.
    pub fn with_savepoint<T, E, F>(&mut self, f: F) -> Result<T, E>
    where
        E: From<StorageError>,
        F: FnOnce(&mut Self) -> Result<T, E>,
    {
        let depth = self.begin_savepoint().map_err(E::from)?;
        struct Guard<'b, 'a, S: KvStore + 'a, V: Vfs> {
            txn: &'b mut GatedWriteTxn<'a, S, V>,
            depth: u32,
            armed: bool,
        }
        impl<S: KvStore, V: Vfs> Drop for Guard<'_, '_, S, V> {
            fn drop(&mut self) {
                if self.armed {
                    while self.txn.savepoint_depth() >= self.depth {
                        let d = self.txn.savepoint_depth();
                        let _ = self.txn.abort_savepoint(d);
                    }
                }
            }
        }
        let mut guard = Guard { txn: self, depth, armed: true };
        match f(guard.txn) {
            Ok(value) => {
                guard.txn.commit_savepoint(depth).map_err(E::from)?;
                guard.armed = false;
                Ok(value)
            }
            Err(err) => {
                // `guard` drops with `armed == true` and aborts all savepoints >= `depth`.
                Err(err)
            }
        }
    }

    /// Consumes the write transaction and commits atomically through `DurabilityGate`.
    pub fn commit(mut self) -> Result<CommitReceipt, StorageError> {
        let gate = self.gate.as_gate();
        gate.check_poisoned()?;
        let txn = self.txn.take().expect("txn present");
        let commit_res = match txn {
            RwTxnRef::Borrowed(t) => t.commit_raw(),
            RwTxnRef::Owned(t) => t.commit_raw(),
        };
        let (receipt, map_guard) = match commit_res {
            Ok(r) => r,
            Err(err) => {
                // If an error occurred on a fatal/IO boundary after commit may have landed,
                // fail closed by poisoning the gate (for Strict & Grouped modes).
                if gate.mode != DurabilityMode::Unsafe
                    && matches!(err, StorageError::Io(_) | StorageError::Corrupted(_))
                {
                    return Err(gate.poison(format!("commit failed with uncertain durability: {err}")));
                }
                return Err(err);
            }
        };
        let out = if gate.mode == DurabilityMode::Strict {
            if gate.unacked_reboot.load(Ordering::Acquire) {
                // Flush LMDB pages so writes aren't lost, but NEVER overwrite `synced.v2`
                // until the caller explicitly invokes `acknowledge_restore()`.
                if let Err(err) = gate.store.sync(&map_guard) {
                    return Err(gate.poison(format!(
                        "LMDB commit (txnid={}) may have landed before store sync failed: {err}",
                        receipt.txnid
                    )));
                }
                receipt
            } else {
                match gate.persist_durable_locked(
                    self.writer_guard.state_mut(),
                    MapWitness::Read(&map_guard),
                    Some(receipt.txnid),
                    false,
                ) {
                    Ok((Some(slot), _)) => CommitReceipt { txnid: slot.txnid, roots_sha256: slot.roots_sha256 },
                    Ok((None, _)) => unreachable!("refresh_pin=false never returns need_adopt"),
                    Err(StorageError::Corrupted(msg)) => {
                        return Err(gate.poison_corrupted(format!(
                            "LMDB commit (txnid={}) may have landed before sidecar sync failed: {msg}",
                            receipt.txnid
                        )));
                    }
                    Err(err) => {
                        return Err(gate.poison(format!(
                            "LMDB commit (txnid={}) may have landed before sidecar sync failed: {err}",
                            receipt.txnid
                        )));
                    }
                }
            }
        } else {
            receipt
        };
        // Release L3 `writer_guard` (`self`) strictly BEFORE releasing L2 `map_guard`.
        drop(self);
        drop(map_guard);
        Ok(out)
    }

    /// Explicitly aborts the transaction.
    pub fn abort(mut self) {
        if let Some(txn) = self.txn.take() {
            match txn {
                RwTxnRef::Borrowed(t) => t.abort(),
                RwTxnRef::Owned(t) => t.abort(),
            }
        }
    }
}

impl<'a, S: KvStore + 'a, V: Vfs> KvRead for GatedWriteTxn<'a, S, V> {
    type Cursor<'b>
        = GatedCursor<<S::RwTxn<'a> as KvRead>::Cursor<'b>, <S::OwnedRwTxn as KvRead>::Cursor<'b>>
    where
        Self: 'b;

    fn get(&self, db: SubDbHandle, key: &[u8]) -> Result<Option<&[u8]>, StorageError> {
        match self.inner() {
            RwTxnRef::Borrowed(t) => t.get(db, key),
            RwTxnRef::Owned(t) => t.get(db, key),
        }
    }

    fn cursor(&self, db: SubDbHandle) -> Result<Self::Cursor<'_>, StorageError> {
        match self.inner() {
            RwTxnRef::Borrowed(t) => Ok(GatedCursor::Borrowed(t.cursor(db)?)),
            RwTxnRef::Owned(t) => Ok(GatedCursor::Owned(t.cursor(db)?)),
        }
    }

    fn entries(&self, db: SubDbHandle) -> Result<u64, StorageError> {
        match self.inner() {
            RwTxnRef::Borrowed(t) => t.entries(db),
            RwTxnRef::Owned(t) => t.entries(db),
        }
    }

    fn txnid(&self) -> u64 {
        match self.inner() {
            RwTxnRef::Borrowed(t) => t.txnid(),
            RwTxnRef::Owned(t) => t.txnid(),
        }
    }

    fn open_subdb_existing(&self, name: &str, flags: DbFlags) -> Result<Option<SubDbHandle>, StorageError> {
        match self.inner() {
            RwTxnRef::Borrowed(t) => t.open_subdb_existing(name, flags),
            RwTxnRef::Owned(t) => t.open_subdb_existing(name, flags),
        }
    }
}

impl<'a, S: KvStore + 'a, V: Vfs> Drop for GatedWriteTxn<'a, S, V> {
    fn drop(&mut self) {
        if let Some(txn) = self.txn.take() {
            match txn {
                RwTxnRef::Borrowed(t) => t.abort(),
                RwTxnRef::Owned(t) => t.abort(),
            }
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
