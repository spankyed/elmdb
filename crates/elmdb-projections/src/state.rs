//! Watermark-bounded `BuildState`, `IndexStatus`, `CheckReport`, `ProjectionStats`,
//! and an in-memory single-sub-DB `KvRead`/`KvWrite` scratchpad used by `check()` to prove
//! exact `derived == rebuild(primary)` equivalence via the single authoritative projection code.

use std::collections::BTreeMap;

use elmdb_engine::{EntityKey, IndexId, SystemDbs};
use elmdb_storage::{DbFlags, GatedWriteTxn, KvCursor, KvEntry, KvRead, KvStore, StorageError, SubDbHandle, Vfs};
use elmdb_wire::{ElmdbError, ErrorCode, Result};
use serde::{Deserialize, Serialize};

/// Narrow write-only mutation capability without exposing raw `commit_raw()` (so `GatedWriteTxn`'s
/// durability-gate bypass protection remains enforced at compile time while `check()` can share
/// the exact same projection apply code over an in-memory overlay!).
pub trait KvMut: KvRead {
    fn open_subdb(&mut self, name: &str, flags: DbFlags) -> std::result::Result<SubDbHandle, StorageError>;
    fn put(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> std::result::Result<(), StorageError>;
    fn put_no_overwrite(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> std::result::Result<bool, StorageError>;
    fn del(&mut self, db: SubDbHandle, key: &[u8]) -> std::result::Result<bool, StorageError>;
    fn truncate(&mut self, db: SubDbHandle) -> std::result::Result<(), StorageError>;
}

impl<S: KvStore, V: Vfs> KvMut for GatedWriteTxn<'_, S, V> {
    fn open_subdb(&mut self, name: &str, flags: DbFlags) -> std::result::Result<SubDbHandle, StorageError> {
        GatedWriteTxn::open_subdb(self, name, flags)
    }

    fn put(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> std::result::Result<(), StorageError> {
        GatedWriteTxn::put(self, db, key, val)
    }

    fn put_no_overwrite(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> std::result::Result<bool, StorageError> {
        GatedWriteTxn::put_no_overwrite(self, db, key, val)
    }

    fn del(&mut self, db: SubDbHandle, key: &[u8]) -> std::result::Result<bool, StorageError> {
        GatedWriteTxn::del(self, db, key)
    }

    fn truncate(&mut self, db: SubDbHandle) -> std::result::Result<(), StorageError> {
        GatedWriteTxn::truncate(self, db)
    }
}

const IDX_STATE_PREFIX: &[u8] = b"idx_state:";

/// Default chunk size for watermark-bounded background index builds.
pub const DEFAULT_BUILD_CHUNK: usize = 1000;

/// Watermark-bounded lifecycle state of a single projection, persisted in `meta`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildState {
    /// Background build is walking `(built_up_to, high_water]` in chunks.
    /// Foreground writes apply ONLY for `key <= built_up_to || key > high_water` (zero double-work!).
    Building { built_up_to: EntityKey, high_water: EntityKey },
    /// Projection is fully caught up and maintained synchronously on every commit.
    Ready,
    /// Build encountered a fatal constraint violation (e.g., `UniqueViolation` on existing data).
    Failed { code: ErrorCode, message: String },
}

impl BuildState {
    /// Returns `true` iff a foreground write to `key` must be applied immediately to this index.
    #[inline]
    pub fn should_apply_foreground(&self, key: EntityKey) -> bool {
        match *self {
            Self::Ready => true,
            Self::Building { built_up_to, high_water } => key <= built_up_to || key > high_water,
            Self::Failed { .. } => false,
        }
    }

    #[inline]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Public status snapshot of one index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStatus {
    pub index_id: IndexId,
    pub name: String,
    pub slot: u16,
    pub state: BuildState,
}

/// Storage statistics for one projection sub-DB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionStats {
    pub index_id: IndexId,
    pub name: String,
    pub slot: u16,
    pub entry_count: u64,
    pub key_bytes: u64,
    pub value_bytes: u64,
}

/// A single discrepancy detected by `ProjectionRegistry::check`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexMismatch {
    pub index_id: IndexId,
    pub index_name: String,
    pub detail: String,
}

/// Result of verifying `derived == rebuild(primary)` across one or all indexes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    pub checked_indexes: usize,
    pub mismatches: Vec<IndexMismatch>,
}

impl CheckReport {
    pub fn is_ok(&self) -> bool {
        self.mismatches.is_empty()
    }
}

pub fn idx_state_key(id: IndexId) -> [u8; 14] {
    let mut k = [0u8; 14];
    k[0..10].copy_from_slice(IDX_STATE_PREFIX);
    k[10..14].copy_from_slice(&id.0.to_be_bytes());
    k
}

pub fn read_build_state<R: KvRead>(txn: &R, dbs: &SystemDbs, id: IndexId) -> Result<Option<BuildState>> {
    let Some(raw) = txn.get(dbs.meta, &idx_state_key(id))? else {
        return Ok(None);
    };
    let state: BuildState =
        serde_json::from_slice(raw).map_err(|e| ElmdbError::new(ErrorCode::Corrupted, e.to_string()))?;
    Ok(Some(state))
}

pub fn write_build_state<W: KvMut>(txn: &mut W, dbs: &SystemDbs, id: IndexId, state: &BuildState) -> Result<()> {
    let encoded = serde_json::to_vec(state).map_err(|e| ElmdbError::new(ErrorCode::Internal, e.to_string()))?;
    txn.put(dbs.meta, &idx_state_key(id), &encoded)?;
    Ok(())
}

pub fn delete_build_state<W: KvMut>(txn: &mut W, dbs: &SystemDbs, id: IndexId) -> Result<()> {
    let _ = txn.del(dbs.meta, &idx_state_key(id))?;
    Ok(())
}

pub fn list_persisted_state_ids<R: KvRead>(txn: &R, dbs: &SystemDbs) -> Result<Vec<IndexId>> {
    let mut cur = txn.cursor(dbs.meta)?;
    let mut out = Vec::new();
    let mut step = cur.seek_ge(IDX_STATE_PREFIX)?;
    while let Some((k, _)) = step {
        if !k.starts_with(IDX_STATE_PREFIX) || k.len() != 14 {
            break;
        }
        let id = u32::from_be_bytes(k[10..14].try_into().expect("4"));
        out.push(IndexId(id));
        step = cur.next()?;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// In-memory single-sub-DB overlay (`MemCheckTxn`) for exact `check()` diffing
// ---------------------------------------------------------------------------

/// Read-write overlay redirecting writes for `target_db` into an in-memory `BTreeMap` while
/// forwarding reads for all other sub-DBs (e.g., `entities`, `blobs`) to the live RO snapshot.
/// Allows `check()` to execute the EXACT same projection `apply_delta` logic without mutating disk!
pub(crate) struct MemCheckTxn<'a, R: KvRead> {
    backing: &'a R,
    target_db: SubDbHandle,
    map: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl<'a, R: KvRead> MemCheckTxn<'a, R> {
    pub fn new(backing: &'a R, target_db: SubDbHandle) -> Self {
        Self { backing, target_db, map: BTreeMap::new() }
    }

    pub fn into_map(self) -> BTreeMap<Vec<u8>, Vec<u8>> {
        self.map
    }
}

pub(crate) struct MemCheckCursor<'txn> {
    entries: Vec<KvEntry<'txn>>,
    pos: Option<usize>,
}

impl<'txn> KvCursor<'txn> for MemCheckCursor<'txn> {
    fn first(&mut self) -> std::result::Result<Option<KvEntry<'txn>>, StorageError> {
        if self.entries.is_empty() {
            self.pos = None;
            Ok(None)
        } else {
            self.pos = Some(0);
            Ok(Some(self.entries[0]))
        }
    }

    fn last(&mut self) -> std::result::Result<Option<KvEntry<'txn>>, StorageError> {
        if self.entries.is_empty() {
            self.pos = None;
            Ok(None)
        } else {
            let last = self.entries.len() - 1;
            self.pos = Some(last);
            Ok(Some(self.entries[last]))
        }
    }

    fn seek_ge(&mut self, key: &[u8]) -> std::result::Result<Option<KvEntry<'txn>>, StorageError> {
        let idx = self.entries.partition_point(|(k, _)| *k < key);
        if idx < self.entries.len() {
            self.pos = Some(idx);
            Ok(Some(self.entries[idx]))
        } else {
            self.pos = None;
            Ok(None)
        }
    }

    fn seek_exact(&mut self, key: &[u8]) -> std::result::Result<Option<&'txn [u8]>, StorageError> {
        match self.entries.binary_search_by_key(&key, |(k, _)| *k) {
            Ok(idx) => {
                self.pos = Some(idx);
                Ok(Some(self.entries[idx].1))
            }
            Err(_) => {
                self.pos = None;
                Ok(None)
            }
        }
    }

    fn next(&mut self) -> std::result::Result<Option<KvEntry<'txn>>, StorageError> {
        let Some(p) = self.pos else { return Ok(None) };
        let next = p + 1;
        if next < self.entries.len() {
            self.pos = Some(next);
            Ok(Some(self.entries[next]))
        } else {
            self.pos = None;
            Ok(None)
        }
    }

    fn prev(&mut self) -> std::result::Result<Option<KvEntry<'txn>>, StorageError> {
        let Some(p) = self.pos else { return Ok(None) };
        if p > 0 {
            let prev = p - 1;
            self.pos = Some(prev);
            Ok(Some(self.entries[prev]))
        } else {
            self.pos = None;
            Ok(None)
        }
    }
}

pub(crate) enum OverlayCursor<'txn, C> {
    Mem(MemCheckCursor<'txn>),
    Backing(C),
}

impl<'txn, C: KvCursor<'txn>> KvCursor<'txn> for OverlayCursor<'txn, C> {
    fn first(&mut self) -> std::result::Result<Option<KvEntry<'txn>>, StorageError> {
        match self {
            Self::Mem(c) => c.first(),
            Self::Backing(c) => c.first(),
        }
    }

    fn last(&mut self) -> std::result::Result<Option<KvEntry<'txn>>, StorageError> {
        match self {
            Self::Mem(c) => c.last(),
            Self::Backing(c) => c.last(),
        }
    }

    fn seek_ge(&mut self, key: &[u8]) -> std::result::Result<Option<KvEntry<'txn>>, StorageError> {
        match self {
            Self::Mem(c) => c.seek_ge(key),
            Self::Backing(c) => c.seek_ge(key),
        }
    }

    fn seek_exact(&mut self, key: &[u8]) -> std::result::Result<Option<&'txn [u8]>, StorageError> {
        match self {
            Self::Mem(c) => c.seek_exact(key),
            Self::Backing(c) => c.seek_exact(key),
        }
    }

    fn next(&mut self) -> std::result::Result<Option<KvEntry<'txn>>, StorageError> {
        match self {
            Self::Mem(c) => c.next(),
            Self::Backing(c) => c.next(),
        }
    }

    fn prev(&mut self) -> std::result::Result<Option<KvEntry<'txn>>, StorageError> {
        match self {
            Self::Mem(c) => c.prev(),
            Self::Backing(c) => c.prev(),
        }
    }
}

impl<'a, R: KvRead> KvRead for MemCheckTxn<'a, R> {
    type Cursor<'txn>
        = OverlayCursor<'txn, R::Cursor<'txn>>
    where
        Self: 'txn;

    fn get(&self, db: SubDbHandle, key: &[u8]) -> std::result::Result<Option<&[u8]>, StorageError> {
        if db == self.target_db { Ok(self.map.get(key).map(Vec::as_slice)) } else { self.backing.get(db, key) }
    }

    fn cursor(&self, db: SubDbHandle) -> std::result::Result<Self::Cursor<'_>, StorageError> {
        if db == self.target_db {
            let entries: Vec<KvEntry<'_>> = self.map.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
            Ok(OverlayCursor::Mem(MemCheckCursor { entries, pos: None }))
        } else {
            Ok(OverlayCursor::Backing(self.backing.cursor(db)?))
        }
    }

    fn entries(&self, db: SubDbHandle) -> std::result::Result<u64, StorageError> {
        if db == self.target_db { Ok(self.map.len() as u64) } else { self.backing.entries(db) }
    }

    fn txnid(&self) -> u64 {
        self.backing.txnid()
    }

    fn open_subdb_existing(
        &self,
        name: &str,
        flags: DbFlags,
    ) -> std::result::Result<Option<SubDbHandle>, StorageError> {
        self.backing.open_subdb_existing(name, flags)
    }
}

impl<'a, R: KvRead> KvMut for MemCheckTxn<'a, R> {
    fn open_subdb(&mut self, _name: &str, _flags: DbFlags) -> std::result::Result<SubDbHandle, StorageError> {
        Ok(self.target_db)
    }

    fn put(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> std::result::Result<(), StorageError> {
        debug_assert_eq!(db, self.target_db);
        self.map.insert(key.to_vec(), val.to_vec());
        Ok(())
    }

    fn put_no_overwrite(&mut self, db: SubDbHandle, key: &[u8], val: &[u8]) -> std::result::Result<bool, StorageError> {
        debug_assert_eq!(db, self.target_db);
        if self.map.contains_key(key) {
            return Ok(false);
        }
        self.map.insert(key.to_vec(), val.to_vec());
        Ok(true)
    }

    fn del(&mut self, db: SubDbHandle, key: &[u8]) -> std::result::Result<bool, StorageError> {
        debug_assert_eq!(db, self.target_db);
        Ok(self.map.remove(key).is_some())
    }

    fn truncate(&mut self, db: SubDbHandle) -> std::result::Result<(), StorageError> {
        debug_assert_eq!(db, self.target_db);
        self.map.clear();
        Ok(())
    }
}

/// Compares a live sub-DB cursor byte-for-byte against an expected rebuilt `BTreeMap`.
pub(crate) fn diff_subdb_against_expected<R: KvRead>(
    txn: &R,
    db: Option<SubDbHandle>,
    expected: &BTreeMap<Vec<u8>, Vec<u8>>,
    index_id: IndexId,
    index_name: &str,
    max_mismatches: usize,
) -> Result<Vec<IndexMismatch>> {
    let mut mismatches = Vec::new();
    let mut actual_entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    if let Some(handle) = db {
        let mut cur = txn.cursor(handle)?;
        let mut step = cur.first()?;
        while let Some((k, v)) = step {
            actual_entries.push((k.to_vec(), v.to_vec()));
            step = cur.next()?;
        }
    }

    let mut exp_iter = expected.iter().peekable();
    let mut act_iter = actual_entries.iter().peekable();

    while mismatches.len() < max_mismatches {
        match (exp_iter.peek(), act_iter.peek()) {
            (None, None) => break,
            (Some(&(ek, _)), None) => {
                mismatches.push(IndexMismatch {
                    index_id,
                    index_name: index_name.to_owned(),
                    detail: format!(
                        "missing derived key (len={}, tag=0x{:02x})",
                        ek.len(),
                        ek.first().copied().unwrap_or(0)
                    ),
                });
                exp_iter.next();
            }
            (None, Some(&(ak, _))) => {
                mismatches.push(IndexMismatch {
                    index_id,
                    index_name: index_name.to_owned(),
                    detail: format!(
                        "unexpected extra derived key (len={}, tag=0x{:02x})",
                        ak.len(),
                        ak.first().copied().unwrap_or(0)
                    ),
                });
                act_iter.next();
            }
            (Some(&(ek, ev)), Some(&(ak, av))) => match ek.cmp(ak) {
                std::cmp::Ordering::Equal => {
                    if ev != av {
                        mismatches.push(IndexMismatch {
                            index_id,
                            index_name: index_name.to_owned(),
                            detail: format!(
                                "value mismatch on key (tag=0x{:02x}, expected {}B, actual {}B)",
                                ek.first().copied().unwrap_or(0),
                                ev.len(),
                                av.len()
                            ),
                        });
                    }
                    exp_iter.next();
                    act_iter.next();
                }
                std::cmp::Ordering::Less => {
                    mismatches.push(IndexMismatch {
                        index_id,
                        index_name: index_name.to_owned(),
                        detail: format!(
                            "missing derived key (len={}, tag=0x{:02x})",
                            ek.len(),
                            ek.first().copied().unwrap_or(0)
                        ),
                    });
                    exp_iter.next();
                }
                std::cmp::Ordering::Greater => {
                    mismatches.push(IndexMismatch {
                        index_id,
                        index_name: index_name.to_owned(),
                        detail: format!(
                            "unexpected extra derived key (len={}, tag=0x{:02x})",
                            ak.len(),
                            ak.first().copied().unwrap_or(0)
                        ),
                    });
                    act_iter.next();
                }
            },
        }
    }
    Ok(mismatches)
}
