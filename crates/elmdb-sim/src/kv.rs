use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

use elmdb_storage::{
    CommitReceipt, DbFlags, KvCursor, KvEntry, KvRead, KvStore, KvWrite, LockScopeId, StorageError, SubDbHandle,
};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

/// Maximum key size matching LMDB's 511-byte default key ceiling.
pub const MAX_KEY_BYTES: usize = 511;

/// Configuration for `SimKvStore` limits.
#[derive(Debug, Clone, Copy)]
pub struct SimKvConfig {
    pub max_dbs: u32,
    pub map_size: usize,
    pub page_size: usize,
}

impl Default for SimKvConfig {
    fn default() -> Self {
        Self { max_dbs: 512, map_size: 64 * 1024 * 1024, page_size: 4096 }
    }
}

/// Ordered key wrapper implementing exact LMDB byte vs. fixed 8-byte native `u64` integer comparison.
#[derive(Debug, Clone)]
enum SimKey {
    Bytes(Vec<u8>),
    Int(u64, [u8; 8]),
}

impl SimKey {
    fn from_slice(flags: DbFlags, bytes: &[u8]) -> Result<Self, StorageError> {
        if flags.integer_key {
            if bytes.len() != 8 {
                return Err(StorageError::BadValSize { len: bytes.len() });
            }
            let arr: [u8; 8] = bytes.try_into().expect("8 bytes");
            Ok(Self::Int(u64::from_ne_bytes(arr), arr))
        } else {
            if bytes.is_empty() || bytes.len() > MAX_KEY_BYTES {
                return Err(StorageError::BadValSize { len: bytes.len() });
            }
            Ok(Self::Bytes(bytes.to_vec()))
        }
    }

    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Bytes(b) => b.as_slice(),
            Self::Int(_, arr) => arr.as_slice(),
        }
    }
}

impl PartialEq for SimKey {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for SimKey {}

impl Ord for SimKey {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Bytes(a), Self::Bytes(b)) => a.cmp(b),
            (Self::Int(a, _), Self::Int(b, _)) => a.cmp(b),
            (Self::Bytes(_), Self::Int(_, _)) => Ordering::Less,
            (Self::Int(_, _), Self::Bytes(_)) => Ordering::Greater,
        }
    }
}

impl PartialOrd for SimKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone)]
struct SimSubDb {
    flags: DbFlags,
    entries: BTreeMap<SimKey, Vec<u8>>,
}

#[derive(Debug, Clone)]
struct KvSnapshot {
    txnid: u64,
    next_handle: u32,
    by_name: BTreeMap<String, SubDbHandle>,
    dbs: BTreeMap<SubDbHandle, SimSubDb>,
    approx_bytes: usize,
    /// Tracks whether this write snapshot contains any mutation since txn start (LMDB parity:
    /// committing a no-op write txn does not bump `txnid`).
    dirty: bool,
}

impl KvSnapshot {
    fn empty() -> Self {
        Self { txnid: 0, next_handle: 1, by_name: BTreeMap::new(), dbs: BTreeMap::new(), approx_bytes: 0, dirty: false }
    }

    fn roots_sha256(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(self.txnid.to_le_bytes());
        for (name, handle) in &self.by_name {
            hasher.update((name.len() as u32).to_le_bytes());
            hasher.update(name.as_bytes());
            if let Some(db) = self.dbs.get(handle) {
                hasher.update([u8::from(db.flags.integer_key)]);
                hasher.update((db.entries.len() as u64).to_le_bytes());
                for (k, v) in &db.entries {
                    let kb = k.as_bytes();
                    hasher.update((kb.len() as u32).to_le_bytes());
                    hasher.update(kb);
                    hasher.update((v.len() as u32).to_le_bytes());
                    hasher.update(v);
                }
            }
        }
        hasher.finalize().into()
    }
}

struct StoreInner {
    config: SimKvConfig,
    lock_scope: LockScopeId,
    committed: Arc<KvSnapshot>,
    durable: Arc<KvSnapshot>,
    writer_active: bool,
    /// Active `SimRoTxn` reader counts keyed by snapshot `txnid` (models LMDB `lock.mdb` reader slots).
    active_readers: BTreeMap<u64, usize>,
    /// Active `Grouped`-mode durable snapshot pin `txnid` (set by `refresh_durable_pin`, cleared on crash).
    pinned_txnid: Option<u64>,
    /// First mutating `txnid > durable.txnid` that freed pages belonging to `durable` (when `durable.txnid > 0`).
    durable_freed_at: Option<u64>,
    /// Set to `true` when an unsynced commit >= `durable_freed_at + 2` runs with no active reader or pin
    /// at `<= durable.txnid` (exact LMDB `mdb_find_oldest` freelist page reuse semantics).
    durable_tree_corrupted: bool,
    /// Set on `crash_power_loss()` if `durable_tree_corrupted` was `true`: meta page still reports
    /// `durable`'s `txnid`/`roots_sha256`, while reading tree pages fails with `StorageError::Corrupted`.
    tree_corrupted_after_crash: bool,
}

impl StoreInner {
    fn oldest_reader_or_pin(&self) -> Option<u64> {
        [self.active_readers.keys().next().copied(), self.pinned_txnid].into_iter().flatten().min()
    }
}

/// Pure-Rust CoW snapshot KV store mirroring LMDB MVCC, nested savepoints, `MDB_INTEGERKEY`,
/// O(1) sub-DB truncation, freelist page reuse vs active reader/pin txnid, and unsynced-vs-durable crash semantics.
#[derive(Clone)]
pub struct SimKvStore {
    inner: Arc<Mutex<StoreInner>>,
    fail_next_sync: Arc<AtomicU32>,
    fail_next_pin: Arc<AtomicU32>,
}

impl SimKvStore {
    pub fn new(config: SimKvConfig) -> Self {
        let empty = Arc::new(KvSnapshot::empty());
        Self {
            inner: Arc::new(Mutex::new(StoreInner {
                config,
                lock_scope: LockScopeId::fresh(),
                committed: Arc::clone(&empty),
                durable: empty,
                writer_active: false,
                active_readers: BTreeMap::new(),
                pinned_txnid: None,
                durable_freed_at: None,
                durable_tree_corrupted: false,
                tree_corrupted_after_crash: false,
            })),
            fail_next_sync: Arc::new(AtomicU32::new(0)),
            fail_next_pin: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Injects `count` upcoming `sync()` failures to verify `DurabilityGate` poisoning.
    pub fn inject_sync_failures(&self, count: u32) {
        self.fail_next_sync.store(count, AtomicOrdering::Release);
    }

    /// Injects `count` upcoming `refresh_durable_pin` / post-remap re-pin failures (e.g. `ReadersFull`).
    pub fn inject_pin_failures(&self, count: u32) {
        self.fail_next_pin.store(count, AtomicOrdering::Release);
    }

    /// Simulates power loss: drops all uncommitted and unsynced commits, reverting the store
    /// back to the last `sync()`ed snapshot. If LMDB's freelist recycled pages of the durable
    /// snapshot because no reader/pin held `<= durable.txnid`, post-crash reads fail with `Corrupted`
    /// even though the meta page still reports `durable`'s `txnid` and `roots_sha256`.
    pub fn crash_power_loss(&self) {
        let mut inner = self.inner.lock();
        inner.committed = Arc::clone(&inner.durable);
        inner.writer_active = false;
        inner.active_readers.clear();
        inner.pinned_txnid = None;
        inner.durable_freed_at = None;
        if inner.durable_tree_corrupted {
            inner.tree_corrupted_after_crash = true;
            inner.durable_tree_corrupted = false;
        }
    }

    /// Simulates a process crash *without* an OS reboot: page cache (`committed`) survives,
    /// while all in-process locks, readers, and durable pins are lost.
    pub fn crash_process_same_boot(&self) {
        let mut inner = self.inner.lock();
        inner.writer_active = false;
        inner.active_readers.clear();
        inner.pinned_txnid = None;
    }

    /// Returns the currently pinned durable snapshot `txnid`, if any.
    pub fn pinned_txnid(&self) -> Option<u64> {
        self.inner.lock().pinned_txnid
    }

    /// Drops the durable snapshot pin without syncing (for testing unpinned freelist corruption).
    pub fn clear_durable_pin_for_test(&self) {
        self.inner.lock().pinned_txnid = None;
    }

    /// Simulates silent storage rollback to an earlier durable snapshot (for corruption tests).
    pub fn simulate_corrupt_rollback(&self) {
        let mut inner = self.inner.lock();
        let empty = Arc::new(KvSnapshot::empty());
        inner.committed = Arc::clone(&empty);
        inner.durable = empty;
    }

    /// Corrupts an entry in-place without incrementing `txnid` (for same-txnid hash mismatch tests).
    pub fn simulate_silent_bitflip(&self, subdb_name: &str, key: &[u8]) {
        let mut inner = self.inner.lock();
        let mut snap = (*inner.committed).clone();
        if let Some(&handle) = snap.by_name.get(subdb_name)
            && let Some(db) = snap.dbs.get_mut(&handle)
            && let Ok(parsed) = SimKey::from_slice(db.flags, key)
            && let Some(val) = db.entries.get_mut(&parsed)
        {
            if let Some(first) = val.first_mut() {
                *first ^= 0xFF;
            } else {
                val.push(0xFF);
            }
        }
        inner.committed = Arc::new(snap);
    }

    /// Returns the `txnid` currently persisted in the durable snapshot.
    pub fn durable_txnid(&self) -> u64 {
        self.inner.lock().durable.txnid
    }
}

impl Default for SimKvStore {
    fn default() -> Self {
        Self::new(SimKvConfig::default())
    }
}

pub struct SimRoTxn {
    inner: Arc<Mutex<StoreInner>>,
    snapshot: Arc<KvSnapshot>,
    tree_corrupted: bool,
}

impl Drop for SimRoTxn {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        if let Some(count) = inner.active_readers.get_mut(&self.snapshot.txnid) {
            if *count <= 1 {
                inner.active_readers.remove(&self.snapshot.txnid);
            } else {
                *count -= 1;
            }
        }
    }
}

pub struct SimRwTxn {
    inner: Arc<Mutex<StoreInner>>,
    /// Stack of CoW working snapshots; index 0 is the root write txn, indices 1.. are nested savepoints.
    stack: Vec<KvSnapshot>,
    finished: bool,
}

#[derive(Debug, Clone, Copy)]
enum CursorPos<'txn> {
    Unpositioned,
    At(&'txn SimKey),
    Bof,
    Eof,
}

pub struct SimCursor<'txn> {
    db: &'txn SimSubDb,
    pos: CursorPos<'txn>,
}

impl<'txn> KvCursor<'txn> for SimCursor<'txn> {
    fn first(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        match self.db.entries.first_key_value() {
            Some((k, v)) => {
                self.pos = CursorPos::At(k);
                Ok(Some((k.as_bytes(), v.as_slice())))
            }
            None => {
                self.pos = CursorPos::Eof;
                Ok(None)
            }
        }
    }

    fn last(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        match self.db.entries.last_key_value() {
            Some((k, v)) => {
                self.pos = CursorPos::At(k);
                Ok(Some((k.as_bytes(), v.as_slice())))
            }
            None => {
                self.pos = CursorPos::Bof;
                Ok(None)
            }
        }
    }

    fn seek_ge(&mut self, key: &[u8]) -> Result<Option<KvEntry<'txn>>, StorageError> {
        let parsed = SimKey::from_slice(self.db.flags, key)?;
        match self.db.entries.range((Bound::Included(parsed), Bound::Unbounded)).next() {
            Some((k, v)) => {
                self.pos = CursorPos::At(k);
                Ok(Some((k.as_bytes(), v.as_slice())))
            }
            None => {
                self.pos = CursorPos::Eof;
                Ok(None)
            }
        }
    }

    fn seek_exact(&mut self, key: &[u8]) -> Result<Option<&'txn [u8]>, StorageError> {
        let parsed = SimKey::from_slice(self.db.flags, key)?;
        match self.db.entries.get_key_value(&parsed) {
            Some((k, v)) => {
                self.pos = CursorPos::At(k);
                Ok(Some(v.as_slice()))
            }
            None => {
                self.pos = CursorPos::Unpositioned;
                Ok(None)
            }
        }
    }

    fn next(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        match self.pos {
            CursorPos::Unpositioned | CursorPos::Bof => self.first(),
            CursorPos::Eof => Ok(None),
            CursorPos::At(cur) => match self.db.entries.range((Bound::Excluded(cur), Bound::Unbounded)).next() {
                Some((k, v)) => {
                    self.pos = CursorPos::At(k);
                    Ok(Some((k.as_bytes(), v.as_slice())))
                }
                None => {
                    self.pos = CursorPos::Eof;
                    Ok(None)
                }
            },
        }
    }

    fn prev(&mut self) -> Result<Option<KvEntry<'txn>>, StorageError> {
        match self.pos {
            CursorPos::Unpositioned | CursorPos::Eof => self.last(),
            CursorPos::Bof => Ok(None),
            CursorPos::At(cur) => match self.db.entries.range((Bound::Unbounded, Bound::Excluded(cur))).next_back() {
                Some((k, v)) => {
                    self.pos = CursorPos::At(k);
                    Ok(Some((k.as_bytes(), v.as_slice())))
                }
                None => {
                    self.pos = CursorPos::Bof;
                    Ok(None)
                }
            },
        }
    }
}

fn validate_subdb_name(name: &str) -> Result<(), StorageError> {
    if name.is_empty() || name.len() > MAX_KEY_BYTES {
        return Err(StorageError::BadValSize { len: name.len() });
    }
    if name.as_bytes().contains(&0) {
        return Err(StorageError::InvalidState("sub-DB name contains interior NUL".into()));
    }
    Ok(())
}

fn read_get<'a>(snap: &'a KvSnapshot, db: SubDbHandle, key: &[u8]) -> Result<Option<&'a [u8]>, StorageError> {
    let sub = snap.dbs.get(&db).ok_or(StorageError::InvalidSubDb { handle: db.raw() })?;
    let parsed = SimKey::from_slice(sub.flags, key)?;
    Ok(sub.entries.get(&parsed).map(Vec::as_slice))
}

fn read_cursor<'a>(snap: &'a KvSnapshot, db: SubDbHandle) -> Result<SimCursor<'a>, StorageError> {
    let sub = snap.dbs.get(&db).ok_or(StorageError::InvalidSubDb { handle: db.raw() })?;
    Ok(SimCursor { db: sub, pos: CursorPos::Unpositioned })
}

fn read_entries(snap: &KvSnapshot, db: SubDbHandle) -> Result<u64, StorageError> {
    let sub = snap.dbs.get(&db).ok_or(StorageError::InvalidSubDb { handle: db.raw() })?;
    Ok(sub.entries.len() as u64)
}

fn read_open_subdb_existing(
    snap: &KvSnapshot,
    name: &str,
    flags: DbFlags,
) -> Result<Option<SubDbHandle>, StorageError> {
    validate_subdb_name(name)?;
    let Some(&handle) = snap.by_name.get(name) else {
        return Ok(None);
    };
    let existing = snap.dbs.get(&handle).expect("valid handle");
    if existing.flags != flags {
        return Err(StorageError::InvalidState(format!("sub-DB '{name}' opened read-only with mismatched flags")));
    }
    Ok(Some(handle))
}

impl KvRead for SimRoTxn {
    type Cursor<'a>
        = SimCursor<'a>
    where
        Self: 'a;

    fn get(&self, db: SubDbHandle, key: &[u8]) -> Result<Option<&[u8]>, StorageError> {
        if self.tree_corrupted {
            return Err(StorageError::Corrupted("freelist reused pages of durable snapshot before power loss".into()));
        }
        read_get(&self.snapshot, db, key)
    }

    fn cursor(&self, db: SubDbHandle) -> Result<Self::Cursor<'_>, StorageError> {
        if self.tree_corrupted {
            return Err(StorageError::Corrupted("freelist reused pages of durable snapshot before power loss".into()));
        }
        read_cursor(&self.snapshot, db)
    }

    fn entries(&self, db: SubDbHandle) -> Result<u64, StorageError> {
        if self.tree_corrupted {
            return Err(StorageError::Corrupted("freelist reused pages of durable snapshot before power loss".into()));
        }
        read_entries(&self.snapshot, db)
    }

    fn txnid(&self) -> u64 {
        self.snapshot.txnid
    }

    fn open_subdb_existing(&self, name: &str, flags: DbFlags) -> Result<Option<SubDbHandle>, StorageError> {
        if self.tree_corrupted {
            return Err(StorageError::Corrupted("freelist reused pages of durable snapshot before power loss".into()));
        }
        read_open_subdb_existing(&self.snapshot, name, flags)
    }
}

impl SimRwTxn {
    fn top(&self) -> &KvSnapshot {
        self.stack.last().expect("write txn always has root snapshot")
    }

    fn top_mut(&mut self) -> &mut KvSnapshot {
        self.stack.last_mut().expect("write txn always has root snapshot")
    }
}

impl KvRead for SimRwTxn {
    type Cursor<'a>
        = SimCursor<'a>
    where
        Self: 'a;

    fn get(&self, db: SubDbHandle, key: &[u8]) -> Result<Option<&[u8]>, StorageError> {
        read_get(self.top(), db, key)
    }

    fn cursor(&self, db: SubDbHandle) -> Result<Self::Cursor<'_>, StorageError> {
        read_cursor(self.top(), db)
    }

    fn entries(&self, db: SubDbHandle) -> Result<u64, StorageError> {
        read_entries(self.top(), db)
    }

    fn txnid(&self) -> u64 {
        self.top().txnid
    }

    fn open_subdb_existing(&self, name: &str, flags: DbFlags) -> Result<Option<SubDbHandle>, StorageError> {
        read_open_subdb_existing(self.top(), name, flags)
    }
}

impl KvWrite for SimRwTxn {
    type MapReadGuard = ();
    fn open_subdb(&mut self, name: &str, flags: DbFlags) -> Result<SubDbHandle, StorageError> {
        validate_subdb_name(name)?;
        let max_dbs = self.inner.lock().config.max_dbs;
        let depth = self.savepoint_depth();
        let snap = self.top_mut();
        if let Some(&handle) = snap.by_name.get(name) {
            let existing = snap.dbs.get(&handle).expect("by_name points to valid db");
            if existing.flags != flags {
                return Err(StorageError::InvalidState(format!("sub-DB '{name}' reopened with mismatched flags")));
            }
            return Ok(handle);
        }
        if depth != 0 {
            return Err(StorageError::InvalidState(
                "creating a new sub-DB inside a nested savepoint is forbidden; open at root write txn".into(),
            ));
        }
        if snap.dbs.len() >= max_dbs as usize {
            return Err(StorageError::DbsFull { max_dbs });
        }
        let handle = SubDbHandle::from_raw(snap.next_handle);
        snap.next_handle += 1;
        snap.by_name.insert(name.to_owned(), handle);
        snap.dbs.insert(handle, SimSubDb { flags, entries: BTreeMap::new() });
        snap.dirty = true;
        Ok(handle)
    }

    fn put(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<(), StorageError> {
        let map_size = self.inner.lock().config.map_size;
        let snap = self.top_mut();
        let sub = snap.dbs.get_mut(&db).ok_or(StorageError::InvalidSubDb { handle: db.raw() })?;
        let parsed = SimKey::from_slice(sub.flags, key)?;
        let old_bytes = sub.entries.get(&parsed).map_or(0, |old| key.len() + old.len());
        let new_bytes = key.len() + val.len();
        let projected = snap.approx_bytes.saturating_sub(old_bytes).saturating_add(new_bytes);
        if projected > map_size {
            return Err(StorageError::MapFull { map_size });
        }
        sub.entries.insert(parsed, val.to_vec());
        snap.approx_bytes = projected;
        snap.dirty = true;
        Ok(())
    }

    fn put_no_overwrite(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> Result<bool, StorageError> {
        let map_size = self.inner.lock().config.map_size;
        let snap = self.top_mut();
        let sub = snap.dbs.get_mut(&db).ok_or(StorageError::InvalidSubDb { handle: db.raw() })?;
        let parsed = SimKey::from_slice(sub.flags, key)?;
        if sub.entries.contains_key(&parsed) {
            return Ok(false);
        }
        let projected = snap.approx_bytes.saturating_add(key.len() + val.len());
        if projected > map_size {
            return Err(StorageError::MapFull { map_size });
        }
        sub.entries.insert(parsed, val.to_vec());
        snap.approx_bytes = projected;
        snap.dirty = true;
        Ok(true)
    }

    fn del(&mut self, db: SubDbHandle, key: &[u8]) -> Result<bool, StorageError> {
        let snap = self.top_mut();
        let sub = snap.dbs.get_mut(&db).ok_or(StorageError::InvalidSubDb { handle: db.raw() })?;
        let parsed = SimKey::from_slice(sub.flags, key)?;
        match sub.entries.remove(&parsed) {
            Some(old) => {
                snap.approx_bytes = snap.approx_bytes.saturating_sub(key.len() + old.len());
                snap.dirty = true;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn truncate(&mut self, db: SubDbHandle) -> Result<(), StorageError> {
        let snap = self.top_mut();
        let sub = snap.dbs.get_mut(&db).ok_or(StorageError::InvalidSubDb { handle: db.raw() })?;
        if !sub.entries.is_empty() {
            let freed: usize = sub.entries.iter().map(|(k, v)| k.as_bytes().len() + v.len()).sum();
            sub.entries.clear();
            snap.approx_bytes = snap.approx_bytes.saturating_sub(freed);
            snap.dirty = true;
        }
        Ok(())
    }

    fn begin_savepoint(&mut self) -> Result<u32, StorageError> {
        let cloned = self.top().clone();
        self.stack.push(cloned);
        Ok(u32::try_from(self.stack.len() - 1).expect("savepoint depth fits u32"))
    }

    fn commit_savepoint(&mut self, depth: u32) -> Result<(), StorageError> {
        let actual = self.savepoint_depth();
        if actual == 0 || depth != actual {
            return Err(StorageError::SavepointMismatch { expected: depth, actual });
        }
        let child = self.stack.pop().expect("child present");
        *self.top_mut() = child;
        Ok(())
    }

    fn abort_savepoint(&mut self, depth: u32) -> Result<(), StorageError> {
        let actual = self.savepoint_depth();
        if actual == 0 || depth != actual {
            return Err(StorageError::SavepointMismatch { expected: depth, actual });
        }
        self.stack.pop();
        Ok(())
    }

    fn savepoint_depth(&self) -> u32 {
        u32::try_from(self.stack.len().saturating_sub(1)).expect("depth fits u32")
    }

    fn commit_raw(mut self) -> Result<(CommitReceipt, Self::MapReadGuard), StorageError> {
        let depth = self.savepoint_depth();
        if depth != 0 {
            return Err(StorageError::SavepointMismatch { expected: 0, actual: depth });
        }
        let mut final_snap = self.stack.pop().expect("root snapshot");
        let was_dirty = final_snap.dirty;
        if was_dirty {
            final_snap.txnid = final_snap.txnid.saturating_add(1);
            final_snap.dirty = false;
        }
        let receipt = CommitReceipt { txnid: final_snap.txnid, roots_sha256: final_snap.roots_sha256() };
        let mut inner = self.inner.lock();
        if was_dirty && inner.durable.txnid > 0 && final_snap.txnid > inner.durable.txnid {
            let freed_at = *inner.durable_freed_at.get_or_insert(final_snap.txnid);
            // LMDB keeps 2 meta slots, so pages freed at `freed_at` become eligible for freelist
            // reuse on commit `freed_at + 2` unless an active reader or durable pin holds `<= durable.txnid`.
            if final_snap.txnid >= freed_at.saturating_add(2)
                && inner.oldest_reader_or_pin().is_none_or(|r| r > inner.durable.txnid)
            {
                inner.durable_tree_corrupted = true;
            }
        }
        inner.committed = Arc::new(final_snap);
        inner.writer_active = false;
        self.finished = true;
        Ok((receipt, ()))
    }

    fn abort(mut self) {
        if !self.finished {
            self.inner.lock().writer_active = false;
            self.finished = true;
        }
    }
}

impl Drop for SimRwTxn {
    fn drop(&mut self) {
        if !self.finished {
            self.inner.lock().writer_active = false;
            self.finished = true;
        }
    }
}

impl KvStore for SimKvStore {
    type RoTxn<'a> = SimRoTxn;
    type RwTxn<'a> = SimRwTxn;
    type OwnedRwTxn = SimRwTxn;
    type MapReadGuard = ();
    type MapWriteGuard<'a> = ();

    fn lock_map_read(&self) -> Self::MapReadGuard {}

    fn lock_map_write(&self) -> Self::MapWriteGuard<'_> {}

    fn begin_read(&self) -> Result<Self::RoTxn<'_>, StorageError> {
        let mut inner = self.inner.lock();
        let snapshot = Arc::clone(&inner.committed);
        *inner.active_readers.entry(snapshot.txnid).or_insert(0) += 1;
        let tree_corrupted = inner.tree_corrupted_after_crash;
        Ok(SimRoTxn { inner: Arc::clone(&self.inner), snapshot, tree_corrupted })
    }

    fn begin_write(&self, map_guard: Self::MapReadGuard) -> Result<Self::RwTxn<'_>, StorageError> {
        self.begin_write_owned(map_guard)
    }

    fn begin_write_owned(&self, _map_guard: Self::MapReadGuard) -> Result<Self::OwnedRwTxn, StorageError> {
        let mut inner = self.inner.lock();
        if inner.writer_active {
            return Err(StorageError::InvalidState("concurrent write transaction already active".into()));
        }
        inner.writer_active = true;
        let mut root = (*inner.committed).clone();
        root.dirty = false;
        Ok(SimRwTxn { inner: Arc::clone(&self.inner), stack: vec![root], finished: false })
    }

    fn open_subdb_ro(
        &self,
        txn: &Self::RoTxn<'_>,
        name: &str,
        flags: DbFlags,
    ) -> Result<Option<SubDbHandle>, StorageError> {
        txn.open_subdb_existing(name, flags)
    }

    fn sync(&self, _map_guard: &Self::MapReadGuard) -> Result<(), StorageError> {
        self.sync_under_write(&())
    }

    fn sync_under_write(&self, _map_guard: &Self::MapWriteGuard<'_>) -> Result<(), StorageError> {
        if self.fail_next_sync.try_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |n| n.checked_sub(1)).is_ok()
        {
            return Err(StorageError::DurabilityFailed("injected sim store sync failure".into()));
        }
        let mut inner = self.inner.lock();
        inner.durable = Arc::clone(&inner.committed);
        inner.durable_freed_at = None;
        inner.durable_tree_corrupted = false;
        Ok(())
    }

    fn refresh_durable_pin(&self, _map_guard: &Self::MapReadGuard) -> Result<(), StorageError> {
        self.refresh_durable_pin_under_write(&())
    }

    fn refresh_durable_pin_under_write(&self, _map_guard: &Self::MapWriteGuard<'_>) -> Result<(), StorageError> {
        if self.fail_next_pin.try_update(AtomicOrdering::AcqRel, AtomicOrdering::Acquire, |n| n.checked_sub(1)).is_ok()
        {
            return Err(StorageError::ReadersFull);
        }
        let mut inner = self.inner.lock();
        inner.pinned_txnid = Some(inner.committed.txnid);
        Ok(())
    }

    fn lock_scope_id(&self) -> LockScopeId {
        self.inner.lock().lock_scope
    }

    fn last_txnid(&self) -> Result<u64, StorageError> {
        Ok(self.inner.lock().committed.txnid)
    }

    fn roots_sha256(&self) -> Result<[u8; 32], StorageError> {
        Ok(self.inner.lock().committed.roots_sha256())
    }

    fn map_size(&self) -> usize {
        self.inner.lock().config.map_size
    }

    fn grow_map_locked(
        &self,
        _map_guard: &Self::MapWriteGuard<'_>,
        min_size: Option<usize>,
        pin_after: bool,
    ) -> Result<usize, StorageError> {
        let new_size = {
            let mut inner = self.inner.lock();
            inner.pinned_txnid = None;
            let target = min_size.unwrap_or_else(|| inner.config.map_size.saturating_mul(2));
            inner.config.map_size = inner.config.map_size.max(target);
            inner.config.map_size
        };
        if pin_after {
            self.refresh_durable_pin_under_write(&())?;
        }
        Ok(new_size)
    }

    fn adopt_resized_map_locked(
        &self,
        _map_guard: &Self::MapWriteGuard<'_>,
        pin_after: bool,
    ) -> Result<usize, StorageError> {
        let size = {
            let mut inner = self.inner.lock();
            inner.pinned_txnid = None;
            inner.config.map_size
        };
        if pin_after {
            self.refresh_durable_pin_under_write(&())?;
        }
        Ok(size)
    }

    fn page_size(&self) -> usize {
        self.inner.lock().config.page_size
    }
}
