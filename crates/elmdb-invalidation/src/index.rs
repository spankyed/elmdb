//! Two-tier `InvalidationIndex` with flat segmented-arena Tier-2 range slab + `touched_bitset`
//! short-circuit, lock-free MVCC `CommitRing` catch-up (1,024-commit window with fail-safe overflow),
//! and selective cross-process `commit_log` catch-up (4,096-commit window with fail-safe overflow).

use std::collections::VecDeque;

use elmdb_engine::{
    CommitDeltaSummary, CompiledSchema, CrossProcCommitSummary, EdgeDelta, Engine, EntityKey, IndexId, IndexKind,
    PackedRow, RelKindId, TypeId,
};
use elmdb_projections::value::{self, ValueKeyClass};
use elmdb_query::{EdgeDir, hash_canonical_value};
use elmdb_storage::{Clock, KvStore, Vfs};
use elmdb_wire::{CanonicalValue, Result};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::read_set::{CoalescedRange, EdgeAdjWatch, PointWatch, ReadSet, WatchId};

/// Capacity of the in-process `CommitRing` used to catch up lock-free MVCC readers after query completion.
pub const COMMIT_RING_CAP: usize = 1024;

// ---------------------------------------------------------------------------
// Dense Owner BitSet for O(1) `touched_bitset` short-circuiting in `stab()`
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    fn reset(&mut self, num_bits: usize) {
        let needed = num_bits.div_ceil(64);
        if self.words.len() < needed {
            self.words.resize(needed, 0);
        }
        for w in &mut self.words[..needed] {
            *w = 0;
        }
    }

    #[inline]
    fn get(&self, idx: u32) -> bool {
        let w = (idx as usize) >> 6;
        let b = (idx as usize) & 63;
        self.words.get(w).is_some_and(|word| (*word & (1u64 << b)) != 0)
    }

    #[inline]
    fn set(&mut self, idx: u32) -> bool {
        let w = (idx as usize) >> 6;
        let b = (idx as usize) & 63;
        if w >= self.words.len() {
            self.words.resize(w + 1, 0);
        }
        let mask = 1u64 << b;
        let was = (self.words[w] & mask) != 0;
        self.words[w] |= mask;
        !was
    }
}

// ---------------------------------------------------------------------------
// Tier 2: Flat Segmented-Arena Range Index per pooled SubDbSlot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct IntervalEntry {
    owner_idx: u32,
    watch_id: WatchId,
    low_off: u32,
    low_len: u32,
    high_off: u32,
    high_len: u32,
    alive: bool,
}

#[derive(Debug, Default)]
struct SlotRangeArena {
    slab: Vec<u8>,
    entries: Vec<IntervalEntry>,
    dead_count: usize,
}

impl SlotRangeArena {
    #[inline]
    fn slice_range(&self, off: u32, len: u32) -> &[u8] {
        let start = off as usize;
        let end = start + (len as usize);
        &self.slab[start..end]
    }

    fn insert(&mut self, owner_idx: u32, watch_id: WatchId, low: &[u8], high: &[u8]) {
        // Compact dead slab bytes first if appending would overflow 32-bit offsets.
        let needed = self.slab.len().saturating_add(low.len()).saturating_add(high.len());
        if needed > u32::MAX as usize && self.dead_count > 0 {
            self.compact();
        }
        let low_off = u32::try_from(self.slab.len()).expect("tier-2 range slab exceeds 4 GiB");
        let low_len = u32::try_from(low.len()).expect("range low key exceeds 4 GiB");
        self.slab.extend_from_slice(low);
        let high_off = u32::try_from(self.slab.len()).expect("tier-2 range slab exceeds 4 GiB");
        let high_len = u32::try_from(high.len()).expect("range high key exceeds 4 GiB");
        self.slab.extend_from_slice(high);
        self.entries.push(IntervalEntry { owner_idx, watch_id, low_off, low_len, high_off, high_len, alive: true });
    }

    fn remove_watch(&mut self, watch_id: WatchId) {
        for e in &mut self.entries {
            if e.alive && e.watch_id == watch_id {
                e.alive = false;
                self.dead_count += 1;
            }
        }
        // Compact slab when >= 50% of entries are dead and at least 64 entries exist.
        if self.dead_count >= 32 && self.dead_count * 2 >= self.entries.len() {
            self.compact();
        }
    }

    fn compact(&mut self) {
        let mut new_slab = Vec::with_capacity(self.slab.len() / 2);
        let mut new_entries = Vec::with_capacity(self.entries.len().saturating_sub(self.dead_count));
        for e in &self.entries {
            if !e.alive {
                continue;
            }
            let low = self.slice_range(e.low_off, e.low_len);
            let high = self.slice_range(e.high_off, e.high_len);
            let low_off = u32::try_from(new_slab.len()).expect("compacted slab exceeds 4 GiB");
            new_slab.extend_from_slice(low);
            let high_off = u32::try_from(new_slab.len()).expect("compacted slab exceeds 4 GiB");
            new_slab.extend_from_slice(high);
            new_entries.push(IntervalEntry {
                owner_idx: e.owner_idx,
                watch_id: e.watch_id,
                low_off,
                low_len: e.low_len,
                high_off,
                high_len: e.high_len,
                alive: true,
            });
        }
        self.slab = new_slab;
        self.entries = new_entries;
        self.dead_count = 0;
    }

    /// Exact point stab: marks every live owner whose `[low, high)` contains `key`,
    /// short-circuiting immediately when `touched.get(owner_idx)` is already true!
    fn stab_key(&self, key: &[u8], touched: &mut BitSet, out: &mut Vec<WatchId>) {
        for e in &self.entries {
            if !e.alive || touched.get(e.owner_idx) {
                continue;
            }
            let low = self.slice_range(e.low_off, e.low_len);
            if key < low {
                continue;
            }
            if e.high_len > 0 {
                let high = self.slice_range(e.high_off, e.high_len);
                if key >= high {
                    continue;
                }
            }
            if touched.set(e.owner_idx) {
                out.push(e.watch_id);
            }
        }
    }

    /// Coarse tag-range stab: marks every live owner whose `[low, high)` overlaps `[[tag], [tag + 1])`.
    fn stab_tag(&self, tag: u8, touched: &mut BitSet, out: &mut Vec<WatchId>) {
        let tag_lo = [tag];
        let tag_hi_byte = tag.checked_add(1);
        for e in &self.entries {
            if !e.alive || touched.get(e.owner_idx) {
                continue;
            }
            let low = self.slice_range(e.low_off, e.low_len);
            if let Some(hi_b) = tag_hi_byte
                && low >= [hi_b].as_slice()
            {
                continue;
            }
            if e.high_len > 0 {
                let high = self.slice_range(e.high_off, e.high_len);
                if high <= tag_lo.as_slice() {
                    continue;
                }
            }
            if touched.set(e.owner_idx) {
                out.push(e.watch_id);
            }
        }
    }

    /// Marks ALL live owners watching any range in this sub-DB slot (used for cross-process & multi-tag projections).
    fn stab_all(&self, touched: &mut BitSet, out: &mut Vec<WatchId>) {
        for e in &self.entries {
            if e.alive && touched.set(e.owner_idx) {
                out.push(e.watch_id);
            }
        }
    }

    fn is_empty(&self) -> bool {
        self.entries.len() == self.dead_count
    }

    fn live_count(&self) -> usize {
        self.entries.len().saturating_sub(self.dead_count)
    }

    fn dead_count(&self) -> usize {
        self.dead_count
    }

    fn slab_bytes(&self) -> usize {
        self.slab.len()
    }
}

// ---------------------------------------------------------------------------
// Per-watch reverse cleanup record & CommitRing record
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct WatchEntry {
    owner_idx: u32,
    all_entities: bool,
    type_scans: Vec<TypeId>,
    point_entities: Vec<(EntityKey, PointWatch)>,
    indexed_eqs: Vec<(IndexId, u64)>,
    edge_adjs: Vec<EdgeAdjWatch>,
    ranges: Vec<CoalescedRange>,
}

#[derive(Debug, Clone)]
enum CommitRingPayload {
    Local(CommitDeltaSummary),
    CrossProc(CrossProcCommitSummary),
    /// Direct sub-DB mutation outside `execute_batch` (e.g., `set_entity_embeddings` / background drain).
    SubDbWrite {
        slot: u16,
        key: Vec<u8>,
    },
    /// Schema epoch change or fail-safe full invalidation barrier.
    FullInvalidation,
}

#[derive(Debug, Clone)]
struct CommitRingEntry {
    seq: u64,
    maint_seq: u64,
    schema_epoch: u64,
    payload: CommitRingPayload,
}

/// Statistics for observability and DST assertions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InvalidationStats {
    pub active_watches: usize,
    pub tier2_live_ranges: usize,
    pub tier2_dead_ranges: usize,
    pub tier2_slab_bytes: usize,
    pub latest_seq: u64,
    pub latest_schema_epoch: u64,
    pub latest_maint_seq: u64,
    pub ring_len: usize,
}

/// Two-tier live-query invalidation index with lock-free MVCC `CommitRing` catch-up
/// and cross-process `commit_log` selective invalidation driven by three exact orthogonal
/// logical clocks: `(commit_seq, schema_epoch, maint_seq)`.
#[derive(Debug, Default)]
pub struct InvalidationIndex {
    // Dense owner allocation (`owner_idx` <-> `WatchId`) for fast bitset short-circuiting.
    watches: FxHashMap<WatchId, WatchEntry>,
    next_owner_idx: u32,
    free_owner_idxs: Vec<u32>,

    // Tier 1 structural inverted maps.
    all_entities_watchers: FxHashSet<WatchId>,
    by_type: FxHashMap<TypeId, FxHashSet<WatchId>>,
    by_point: FxHashMap<EntityKey, Vec<(WatchId, u32, PointWatch)>>,
    by_indexed_eq: FxHashMap<(IndexId, u64), FxHashSet<WatchId>>,
    by_index_id: FxHashMap<IndexId, FxHashSet<WatchId>>,
    by_edge_adj: FxHashMap<EntityKey, Vec<(WatchId, u32, EdgeAdjWatch)>>,
    by_rel_kind_adj: FxHashMap<RelKindId, FxHashSet<WatchId>>,
    wildcard_rel_adj: FxHashSet<WatchId>,

    // Tier 2 flat segmented-arena range slabs per `SubDbSlot`.
    slot_arenas: FxHashMap<u16, SlotRangeArena>,

    // Lock-free MVCC CommitRing (last `COMMIT_RING_CAP` entries) over three orthogonal logical clocks.
    latest_seq: u64,
    latest_schema_epoch: u64,
    latest_maint_seq: u64,
    ring_dropped_seq_up_to: u64,
    ring_dropped_maint_up_to: u64,
    ring: VecDeque<CommitRingEntry>,

    // Reusable scratch bitset for zero-alloc `stab` deduplication.
    scratch_touched: BitSet,
}

impl InvalidationIndex {
    pub fn new(initial_seq: u64, initial_schema_epoch: u64) -> Self {
        Self::new_with_maint_seq(initial_seq, initial_schema_epoch, 0)
    }

    pub fn new_with_maint_seq(initial_seq: u64, initial_schema_epoch: u64, initial_maint_seq: u64) -> Self {
        Self {
            latest_seq: initial_seq,
            latest_schema_epoch: initial_schema_epoch,
            latest_maint_seq: initial_maint_seq,
            ring_dropped_seq_up_to: initial_seq,
            ring_dropped_maint_up_to: initial_maint_seq,
            ..Default::default()
        }
    }

    pub fn stats(&self) -> InvalidationStats {
        let tier2_live_ranges = self.slot_arenas.values().map(SlotRangeArena::live_count).sum();
        let tier2_dead_ranges = self.slot_arenas.values().map(SlotRangeArena::dead_count).sum();
        let tier2_slab_bytes = self.slot_arenas.values().map(SlotRangeArena::slab_bytes).sum();
        InvalidationStats {
            active_watches: self.watches.len(),
            tier2_live_ranges,
            tier2_dead_ranges,
            tier2_slab_bytes,
            latest_seq: self.latest_seq,
            latest_schema_epoch: self.latest_schema_epoch,
            latest_maint_seq: self.latest_maint_seq,
            ring_len: self.ring.len(),
        }
    }

    pub fn latest_seq(&self) -> u64 {
        self.latest_seq
    }

    pub fn latest_schema_epoch(&self) -> u64 {
        self.latest_schema_epoch
    }

    pub fn latest_maint_seq(&self) -> u64 {
        self.latest_maint_seq
    }

    /// Removes `watch_id` from all Tier 1 and Tier 2 tables and recycles its dense `owner_idx`.
    pub fn unregister_watch(&mut self, watch_id: WatchId) -> bool {
        let Some(entry) = self.watches.remove(&watch_id) else {
            return false;
        };
        self.remove_entry_from_tables(watch_id, &entry);
        self.free_owner_idxs.push(entry.owner_idx);
        true
    }

    fn remove_entry_from_tables(&mut self, watch_id: WatchId, entry: &WatchEntry) {
        if entry.all_entities {
            self.all_entities_watchers.remove(&watch_id);
        }
        for &tid in &entry.type_scans {
            if let Some(set) = self.by_type.get_mut(&tid) {
                set.remove(&watch_id);
                if set.is_empty() {
                    self.by_type.remove(&tid);
                }
            }
        }
        for (ek, _) in &entry.point_entities {
            if let Some(list) = self.by_point.get_mut(ek) {
                list.retain(|(wid, _, _)| *wid != watch_id);
                if list.is_empty() {
                    self.by_point.remove(ek);
                }
            }
        }
        for &(idx_id, hash) in &entry.indexed_eqs {
            if let Some(set) = self.by_indexed_eq.get_mut(&(idx_id, hash)) {
                set.remove(&watch_id);
                if set.is_empty() {
                    self.by_indexed_eq.remove(&(idx_id, hash));
                }
            }
            if let Some(set) = self.by_index_id.get_mut(&idx_id) {
                set.remove(&watch_id);
                if set.is_empty() {
                    self.by_index_id.remove(&idx_id);
                }
            }
        }
        for ea in &entry.edge_adjs {
            if let Some(list) = self.by_edge_adj.get_mut(&ea.entity) {
                list.retain(|(wid, _, _)| *wid != watch_id);
                if list.is_empty() {
                    self.by_edge_adj.remove(&ea.entity);
                }
            }
            if ea.rel_kinds.is_empty() {
                self.wildcard_rel_adj.remove(&watch_id);
            } else {
                for &rk in &ea.rel_kinds {
                    if let Some(set) = self.by_rel_kind_adj.get_mut(&rk) {
                        set.remove(&watch_id);
                        if set.is_empty() {
                            self.by_rel_kind_adj.remove(&rk);
                        }
                    }
                }
            }
        }
        let mut touched_slots = FxHashSet::default();
        for r in &entry.ranges {
            touched_slots.insert(r.slot);
        }
        for slot in touched_slots {
            if let Some(arena) = self.slot_arenas.get_mut(&slot) {
                arena.remove_watch(watch_id);
                if arena.is_empty() {
                    self.slot_arenas.remove(&slot);
                }
            }
        }
    }

    /// Registers (or atomically replaces) `watch_id`'s `ReadSet` collected during an MVCC snapshot read
    /// at `(read_at_seq, read_schema_epoch)` and immediately checks `CommitRing(read_at_seq + 1 ..= latest_seq)`!
    ///
    /// Returns `true` if `watch_id` is ALREADY stale due to concurrent commits or `CommitRing` overflow
    /// (guaranteeing **zero** lost invalidations under lock-free concurrent MVCC reads!).
    pub fn register_after_read(
        &mut self,
        watch_id: WatchId,
        read_at_seq: u64,
        read_schema_epoch: u64,
        read_set: &ReadSet,
        current_schema: &CompiledSchema,
    ) -> bool {
        self.register_after_read_with_maint(watch_id, read_at_seq, 0, read_schema_epoch, read_set, current_schema)
    }

    /// Registers (or atomically replaces) `watch_id`'s `ReadSet` collected during an MVCC snapshot read
    /// at `(read_at_seq, read_at_maint_seq, read_schema_epoch)` and immediately checks `CommitRing`.
    ///
    /// Comparing maintenance writes strictly against `read_at_maint_seq` (`rec.maint_seq > read_at_maint_seq`)
    /// prevents post-maintenance watch livelock while catching every concurrent maintenance write!
    pub fn register_after_read_with_maint(
        &mut self,
        watch_id: WatchId,
        read_at_seq: u64,
        read_at_maint_seq: u64,
        read_schema_epoch: u64,
        read_set: &ReadSet,
        current_schema: &CompiledSchema,
    ) -> bool {
        // Reuse existing owner_idx on re-registration, or allocate a fresh one.
        let owner_idx = if let Some(prev) = self.watches.remove(&watch_id) {
            let oid = prev.owner_idx;
            self.remove_entry_from_tables(watch_id, &prev);
            oid
        } else if let Some(recycled) = self.free_owner_idxs.pop() {
            recycled
        } else {
            let oid = self.next_owner_idx;
            self.next_owner_idx = self.next_owner_idx.checked_add(1).expect("watch owner_idx overflow");
            oid
        };

        let type_scans: Vec<TypeId> = read_set.type_scans.iter().copied().collect();
        let point_entities: Vec<(EntityKey, PointWatch)> =
            read_set.point_entities.iter().map(|(&k, pw)| (k, pw.clone())).collect();
        let indexed_eqs: Vec<(IndexId, u64)> = read_set.indexed_eqs.iter().copied().collect();
        let edge_adjs = read_set.coalesce_edge_adjs();
        let ranges = read_set.coalesce_ranges();

        if read_set.all_entities {
            self.all_entities_watchers.insert(watch_id);
        }
        for &tid in &type_scans {
            self.by_type.entry(tid).or_default().insert(watch_id);
        }
        for (ek, pw) in &point_entities {
            self.by_point.entry(*ek).or_default().push((watch_id, owner_idx, pw.clone()));
        }
        for &(idx_id, h) in &indexed_eqs {
            self.by_indexed_eq.entry((idx_id, h)).or_default().insert(watch_id);
            self.by_index_id.entry(idx_id).or_default().insert(watch_id);
        }
        for ea in &edge_adjs {
            self.by_edge_adj.entry(ea.entity).or_default().push((watch_id, owner_idx, ea.clone()));
            if ea.rel_kinds.is_empty() {
                self.wildcard_rel_adj.insert(watch_id);
            } else {
                for &rk in &ea.rel_kinds {
                    self.by_rel_kind_adj.entry(rk).or_default().insert(watch_id);
                }
            }
        }
        for r in &ranges {
            self.slot_arenas.entry(r.slot).or_default().insert(owner_idx, watch_id, &r.low, &r.high);
        }

        let entry = WatchEntry {
            owner_idx,
            all_entities: read_set.all_entities,
            type_scans,
            point_entities,
            indexed_eqs,
            edge_adjs,
            ranges,
        };

        // Check whether any commits or maintenance writes happened AFTER `(read_at_seq, read_at_maint_seq)`
        // (or schema epoch changed).
        let stale =
            self.check_ring_staleness(&entry, read_at_seq, read_at_maint_seq, read_schema_epoch, current_schema);
        self.watches.insert(watch_id, entry);
        stale
    }

    fn check_ring_staleness(
        &self,
        entry: &WatchEntry,
        read_at_seq: u64,
        read_at_maint_seq: u64,
        read_schema_epoch: u64,
        schema: &CompiledSchema,
    ) -> bool {
        if read_schema_epoch != self.latest_schema_epoch || read_schema_epoch != schema.epoch {
            return true;
        }
        if read_at_seq >= self.latest_seq && read_at_maint_seq >= self.latest_maint_seq {
            return false;
        }
        // Fail strictly safe if `CommitRing` evicted commits or maintenance writes overlapping the reader's snapshot!
        if read_at_seq < self.ring_dropped_seq_up_to || read_at_maint_seq < self.ring_dropped_maint_up_to {
            return true;
        }
        let mut expected_seq = read_at_seq.saturating_add(1);
        let mut expected_maint = read_at_maint_seq.saturating_add(1);
        for rec in &self.ring {
            match &rec.payload {
                CommitRingPayload::SubDbWrite { slot, key } => {
                    if rec.maint_seq <= read_at_maint_seq {
                        continue;
                    }
                    if rec.schema_epoch != read_schema_epoch {
                        return true;
                    }
                    if rec.maint_seq > expected_maint {
                        return true;
                    }
                    if rec.maint_seq == expected_maint {
                        expected_maint = expected_maint.saturating_add(1);
                    }
                    if entry_matches_subdb_key(entry, *slot, key) {
                        return true;
                    }
                }
                CommitRingPayload::FullInvalidation => {
                    if rec.seq > read_at_seq || rec.maint_seq > read_at_maint_seq {
                        return true;
                    }
                }
                CommitRingPayload::Local(summary) => {
                    if rec.seq <= read_at_seq {
                        continue;
                    }
                    if rec.schema_epoch != read_schema_epoch {
                        return true;
                    }
                    if rec.seq > expected_seq {
                        return true;
                    }
                    if rec.seq == expected_seq {
                        expected_seq = expected_seq.saturating_add(1);
                    }
                    if entry_matches_local_commit(entry, schema, summary) {
                        return true;
                    }
                }
                CommitRingPayload::CrossProc(cp) => {
                    if rec.seq <= read_at_seq {
                        continue;
                    }
                    if rec.schema_epoch != read_schema_epoch {
                        return true;
                    }
                    if rec.seq > expected_seq {
                        return true;
                    }
                    if rec.seq == expected_seq {
                        expected_seq = expected_seq.saturating_add(1);
                    }
                    if entry_matches_cross_proc(entry, schema, cp) {
                        return true;
                    }
                }
            }
        }
        // Fail safe if ring did not contiguously cover all commits and maintenance writes up to the tip.
        (read_at_seq < self.latest_seq && expected_seq <= self.latest_seq)
            || (read_at_maint_seq < self.latest_maint_seq && expected_maint <= self.latest_maint_seq)
    }

    fn push_ring(&mut self, seq: u64, maint_seq: u64, schema_epoch: u64, payload: CommitRingPayload) {
        self.latest_seq = self.latest_seq.max(seq);
        self.latest_maint_seq = self.latest_maint_seq.max(maint_seq);
        self.latest_schema_epoch = self.latest_schema_epoch.max(schema_epoch);
        if self.ring.len() >= COMMIT_RING_CAP
            && let Some(evicted) = self.ring.pop_front()
        {
            match evicted.payload {
                CommitRingPayload::SubDbWrite { .. } => {
                    self.ring_dropped_maint_up_to = self.ring_dropped_maint_up_to.max(evicted.maint_seq);
                }
                CommitRingPayload::FullInvalidation => {
                    self.ring_dropped_seq_up_to = self.ring_dropped_seq_up_to.max(evicted.seq);
                    self.ring_dropped_maint_up_to = self.ring_dropped_maint_up_to.max(evicted.maint_seq);
                }
                CommitRingPayload::Local(_) | CommitRingPayload::CrossProc(_) => {
                    self.ring_dropped_seq_up_to = self.ring_dropped_seq_up_to.max(evicted.seq);
                }
            }
        }
        self.ring.push_back(CommitRingEntry { seq, maint_seq, schema_epoch, payload });
    }

    /// Processes an in-process committed `CommitDeltaSummary`, appends it to `CommitRing`,
    /// and returns the deduplicated list of invalidated `WatchId`s!
    ///
    /// Fail-safe guarantee:
    /// - If `summary.maint_seq > prev_maint` (an unseen cross-process maintenance write occurred),
    ///   `summary.seq > prev_seq + 1` (a commit gap), or `schema_epoch` changed, invalidates ALL watches!
    /// - Standalone non-query writes (like `next_counter`/`set_counter`) bump neither `commit_seq` nor
    ///   `maint_seq`, so they never trigger spurious false-positive invalidations!
    pub fn notify_commit(&mut self, schema: &CompiledSchema, summary: &CommitDeltaSummary) -> Vec<WatchId> {
        let prev_seq = self.latest_seq;
        let prev_maint = self.latest_maint_seq;
        let prev_epoch = self.latest_schema_epoch;
        let maint_gap = summary.maint_seq > prev_maint;

        if maint_gap {
            self.ring_dropped_seq_up_to = self.ring_dropped_seq_up_to.max(summary.seq);
            self.ring_dropped_maint_up_to = self.ring_dropped_maint_up_to.max(summary.maint_seq);
            self.push_ring(summary.seq, summary.maint_seq, summary.schema_epoch, CommitRingPayload::FullInvalidation);
            return self.all_watch_ids();
        }
        if summary.seq <= prev_seq {
            return Vec::new();
        }

        let schema_changed = summary.schema_epoch != prev_epoch;
        let seq_gap = summary.seq > prev_seq.saturating_add(1);
        if seq_gap {
            let missing_up_to = summary.seq.saturating_sub(1);
            self.ring_dropped_seq_up_to = self.ring_dropped_seq_up_to.max(missing_up_to);
            self.push_ring(missing_up_to, prev_maint, summary.schema_epoch, CommitRingPayload::FullInvalidation);
        }
        self.push_ring(summary.seq, summary.maint_seq, summary.schema_epoch, CommitRingPayload::Local(summary.clone()));

        if schema_changed || seq_gap {
            return self.all_watch_ids();
        }
        if self.watches.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::new();
        let num_owners = self.next_owner_idx as usize;
        self.scratch_touched.reset(num_owners);

        // 1. `AllEntities` watchers (any entity or edge delta invalidates).
        if !summary.entity_deltas.is_empty() || !summary.edge_deltas.is_empty() {
            for &wid in &self.all_entities_watchers {
                if let Some(we) = self.watches.get(&wid)
                    && self.scratch_touched.set(we.owner_idx)
                {
                    out.push(wid);
                }
            }
        }

        // 2. `TypeScan(type_id)` watchers.
        for tid in summary.touched_types.iter() {
            if let Some(set) = self.by_type.get(&tid) {
                for &wid in set {
                    if let Some(we) = self.watches.get(&wid)
                        && self.scratch_touched.set(we.owner_idx)
                    {
                        out.push(wid);
                    }
                }
            }
        }

        // 3. `PointEntity(ek, fields)` watchers — true field-level precision!
        for delta in &summary.entity_deltas {
            if let Some(list) = self.by_point.get(&delta.key) {
                for (wid, owner_idx, pw) in list {
                    if !self.scratch_touched.get(*owner_idx)
                        && pw.matches_mutation(delta.liveness_changed, &delta.changed_fields)
                        && self.scratch_touched.set(*owner_idx)
                    {
                        out.push(*wid);
                    }
                }
            }
        }

        // 4. `EdgeAdj(entity, rel_kinds, dir)` watchers.
        for ed in &summary.edge_deltas {
            let (src, dst) = edge_endpoints(ed);
            if let Some(src_ek) = src
                && let Some(list) = self.by_edge_adj.get(&src_ek)
            {
                for (wid, owner_idx, ea) in list {
                    if !self.scratch_touched.get(*owner_idx)
                        && ea.matches_edge(ed.kind, EdgeDir::Out)
                        && self.scratch_touched.set(*owner_idx)
                    {
                        out.push(*wid);
                    }
                }
            }
            if let Some(dst_ek) = dst
                && let Some(list) = self.by_edge_adj.get(&dst_ek)
            {
                for (wid, owner_idx, ea) in list {
                    if !self.scratch_touched.get(*owner_idx)
                        && ea.matches_edge(ed.kind, EdgeDir::In)
                        && self.scratch_touched.set(*owner_idx)
                    {
                        out.push(*wid);
                    }
                }
            }
        }

        // 5. `IndexedEq(index_id, hash)` + Tier 2 `SubDbRange(slot, [low, high))` per affected index!
        for def in &schema.indexes {
            match &def.kind {
                IndexKind::Value { fields, .. } => {
                    if !summary.touched_types.contains(def.type_id) {
                        continue;
                    }
                    let is_single = fields.len() == 1;
                    for delta in &summary.entity_deltas {
                        if delta.type_id != def.type_id {
                            continue;
                        }
                        if !delta.liveness_changed && !fields.iter().any(|f| delta.changed_fields.contains(*f)) {
                            continue;
                        }
                        for row_opt in [live_row(delta.before.as_ref()), live_row(delta.after.as_ref())] {
                            let Some(row) = row_opt else { continue };
                            if is_single {
                                let v = row.fields.get(&fields[0]).unwrap_or(&CanonicalValue::Null);
                                let h = hash_canonical_value(v);
                                if let Some(set) = self.by_indexed_eq.get(&(def.id, h)) {
                                    for &wid in set {
                                        if let Some(we) = self.watches.get(&wid)
                                            && self.scratch_touched.set(we.owner_idx)
                                        {
                                            out.push(wid);
                                        }
                                    }
                                }
                            }
                            if let Some(arena) = self.slot_arenas.get(&def.slot) {
                                let key_bytes = value_index_stab_key(row, fields, delta.key);
                                arena.stab_key(&key_bytes, &mut self.scratch_touched, &mut out);
                            }
                        }
                    }
                }
                IndexKind::Order { field, .. } => {
                    if !summary.touched_types.contains(def.type_id) {
                        continue;
                    }
                    let Some(arena) = self.slot_arenas.get(&def.slot) else { continue };
                    for delta in &summary.entity_deltas {
                        if delta.type_id != def.type_id {
                            continue;
                        }
                        if !delta.liveness_changed && !delta.changed_fields.contains(*field) {
                            continue;
                        }
                        for row_opt in [live_row(delta.before.as_ref()), live_row(delta.after.as_ref())] {
                            let Some(row) = row_opt else { continue };
                            let tag = if matches!(row.fields.get(field), Some(CanonicalValue::String(_))) {
                                0x01
                            } else {
                                0x02
                            };
                            arena.stab_tag(tag, &mut self.scratch_touched, &mut out);
                        }
                    }
                }
                IndexKind::Text { fields } => {
                    if !summary.touched_types.contains(def.type_id) {
                        continue;
                    }
                    let Some(arena) = self.slot_arenas.get(&def.slot) else { continue };
                    let touched = summary.entity_deltas.iter().any(|d| {
                        d.type_id == def.type_id
                            && (d.liveness_changed || fields.iter().any(|(f, _)| d.changed_fields.contains(*f)))
                    });
                    if touched {
                        // Cover ALL internal Text sub-DB tags (`TAG_STATS..=TAG_LENGTHS`).
                        arena.stab_all(&mut self.scratch_touched, &mut out);
                    }
                }
                IndexKind::Trigram { fields } | IndexKind::Vector { fields, .. } => {
                    if !summary.touched_types.contains(def.type_id) {
                        continue;
                    }
                    let Some(arena) = self.slot_arenas.get(&def.slot) else { continue };
                    let touched = summary.entity_deltas.iter().any(|d| {
                        d.type_id == def.type_id
                            && (d.liveness_changed || fields.iter().any(|f| d.changed_fields.contains(*f)))
                    });
                    if touched {
                        // Cover ALL internal Trigram (`0x01..=0x04`) and Vector (`0x00..=0x03`) tags!
                        arena.stab_all(&mut self.scratch_touched, &mut out);
                    }
                }
                IndexKind::Rank { relations, .. } => {
                    let Some(arena) = self.slot_arenas.get(&def.slot) else { continue };
                    let entity_touched =
                        summary.entity_deltas.iter().any(|d| d.type_id == def.type_id && d.liveness_changed);
                    let edge_touched =
                        summary.edge_deltas.iter().any(|e| relations.is_empty() || relations.contains(&e.kind));
                    if entity_touched || edge_touched {
                        // Cover ALL internal Rank tags (`TAG_META`, `TAG_RANK`, `TAG_ORDER`, `TAG_DIRTY`)!
                        arena.stab_all(&mut self.scratch_touched, &mut out);
                    }
                }
            }
        }

        out
    }

    /// Notifies a direct sub-DB write outside `execute_batch` (e.g., `set_entity_embeddings` or background vector/rank drain).
    pub fn notify_subdb_write(&mut self, seq: u64, schema_epoch: u64, slot: u16, key: &[u8]) -> Vec<WatchId> {
        self.notify_subdb_write_at_maint(seq, schema_epoch, 0, slot, key)
    }

    pub fn notify_subdb_write_at_maint(
        &mut self,
        seq: u64,
        schema_epoch: u64,
        maint_seq: u64,
        slot: u16,
        key: &[u8],
    ) -> Vec<WatchId> {
        let prev_seq = self.latest_seq;
        let prev_maint = self.latest_maint_seq;
        let prev_epoch = self.latest_schema_epoch;

        // Ignore stale delayed receipts already superseded by a newer catch-up, schema change, or barrier.
        if maint_seq > 0 && maint_seq <= prev_maint && seq <= prev_seq && schema_epoch <= prev_epoch {
            return Vec::new();
        }

        let schema_changed = schema_epoch != prev_epoch;
        let seq_gap = seq > prev_seq;
        let effective_maint = if maint_seq > 0 { maint_seq } else { prev_maint.saturating_add(1) };
        let maint_gap = effective_maint > prev_maint.saturating_add(1);

        if seq_gap || maint_gap {
            self.ring_dropped_seq_up_to = self.ring_dropped_seq_up_to.max(seq);
            self.ring_dropped_maint_up_to = self.ring_dropped_maint_up_to.max(effective_maint);
            self.push_ring(seq, effective_maint, schema_epoch, CommitRingPayload::FullInvalidation);
            return self.all_watch_ids();
        }
        self.push_ring(seq, effective_maint, schema_epoch, CommitRingPayload::SubDbWrite { slot, key: key.to_vec() });

        if schema_changed {
            return self.all_watch_ids();
        }
        let mut out = Vec::new();
        if let Some(arena) = self.slot_arenas.get(&slot) {
            self.scratch_touched.reset(self.next_owner_idx as usize);
            arena.stab_key(key, &mut self.scratch_touched, &mut out);
        }
        out
    }

    /// Notifies a schema change (`set_schema`), invalidating all active watches cleanly.
    pub fn notify_schema_change(&mut self, seq: u64, new_schema_epoch: u64) -> Vec<WatchId> {
        self.notify_schema_change_at_maint(seq, new_schema_epoch, self.latest_maint_seq)
    }

    pub fn notify_schema_change_at_maint(&mut self, seq: u64, new_schema_epoch: u64, maint_seq: u64) -> Vec<WatchId> {
        let effective_maint = maint_seq.max(self.latest_maint_seq);
        if seq > self.latest_seq.saturating_add(1) {
            self.ring_dropped_seq_up_to = self.ring_dropped_seq_up_to.max(seq.saturating_sub(1));
        }
        if effective_maint > self.latest_maint_seq {
            self.ring_dropped_maint_up_to = self.ring_dropped_maint_up_to.max(effective_maint);
        }
        self.push_ring(seq, effective_maint, new_schema_epoch, CommitRingPayload::FullInvalidation);
        self.all_watch_ids()
    }

    /// Notifies a full maintenance rebuild, recording a `maint_seq`-tagged barrier so concurrent
    /// readers older than `maint_seq` rerun once and settle cleanly.
    pub fn notify_full_rebuild(&mut self, seq: u64, schema_epoch: u64) -> Vec<WatchId> {
        self.notify_full_rebuild_at_maint(seq, schema_epoch, 0)
    }

    pub fn notify_full_rebuild_at_maint(&mut self, seq: u64, schema_epoch: u64, maint_seq: u64) -> Vec<WatchId> {
        if maint_seq > 0
            && maint_seq <= self.latest_maint_seq
            && seq <= self.latest_seq
            && schema_epoch <= self.latest_schema_epoch
        {
            return Vec::new();
        }
        let effective_maint = if maint_seq > 0 { maint_seq } else { self.latest_maint_seq.saturating_add(1) };
        if seq > self.latest_seq {
            self.ring_dropped_seq_up_to = self.ring_dropped_seq_up_to.max(seq);
        }
        self.ring_dropped_maint_up_to = self.ring_dropped_maint_up_to.max(effective_maint);
        self.push_ring(seq, effective_maint, schema_epoch, CommitRingPayload::FullInvalidation);
        self.all_watch_ids()
    }

    /// Selective cross-process catch-up via persisted `commit_log` (last 4,096 commits),
    /// executed atomically inside **ONE single pinned MVCC `RoTxn`** (zero TOCTOU race!)
    /// using the exact orthogonal `(commit_seq, schema_epoch, maint_seq)` clocks.
    pub fn catch_up_cross_process<S: KvStore, V: Vfs, C: Clock>(
        &mut self,
        engine: &Engine<S, V, C>,
    ) -> Result<Vec<WatchId>> {
        let (ro, schema) = engine.begin_read_pinned()?;
        let tip_maint = elmdb_engine::read_maint_seq(&ro, &engine.dbs())?;
        let (tip_seq, summaries_opt) = elmdb_engine::read_commit_summaries_in_txn(&ro, &engine.dbs(), self.latest_seq)?;
        let tip_epoch = schema.epoch;
        drop(ro);

        if tip_maint > self.latest_maint_seq {
            self.ring_dropped_seq_up_to = self.ring_dropped_seq_up_to.max(tip_seq);
            self.ring_dropped_maint_up_to = self.ring_dropped_maint_up_to.max(tip_maint);
            self.push_ring(tip_seq, tip_maint, tip_epoch, CommitRingPayload::FullInvalidation);
            return Ok(self.all_watch_ids());
        }

        if tip_seq <= self.latest_seq && tip_epoch == self.latest_schema_epoch {
            return Ok(Vec::new());
        }

        let Some(summaries) = summaries_opt else {
            // > 4,096 commits elapsed cross-process: fail strictly safe!
            self.ring_dropped_seq_up_to = self.ring_dropped_seq_up_to.max(tip_seq);
            self.push_ring(tip_seq, tip_maint, tip_epoch, CommitRingPayload::FullInvalidation);
            return Ok(self.all_watch_ids());
        };

        if tip_seq > self.latest_seq {
            // P0-2: Verify contiguous sequence coverage `self.latest_seq + 1 ..= tip_seq`!
            let mut expected = self.latest_seq.saturating_add(1);
            let mut contiguous = !summaries.is_empty();
            for cp in &summaries {
                if cp.seq != expected {
                    contiguous = false;
                    break;
                }
                expected = expected.saturating_add(1);
            }
            if !contiguous || expected != tip_seq.saturating_add(1) {
                self.ring_dropped_seq_up_to = self.ring_dropped_seq_up_to.max(tip_seq);
                self.push_ring(tip_seq, tip_maint, tip_epoch, CommitRingPayload::FullInvalidation);
                return Ok(self.all_watch_ids());
            }
        }

        if summaries.is_empty() {
            if tip_epoch != self.latest_schema_epoch {
                return Ok(self.notify_schema_change_at_maint(tip_seq, tip_epoch, tip_maint));
            }
            return Ok(Vec::new());
        }

        let mut all_dirty: FxHashSet<WatchId> = FxHashSet::default();
        for cp in summaries {
            let dirty = self.apply_cross_proc_summary_at_maint(&schema, &cp, tip_maint);
            all_dirty.extend(dirty);
        }
        // If `tip_epoch` advanced (e.g. schema change didn't emit a data commit summary), fail safe!
        if tip_epoch != self.latest_schema_epoch {
            all_dirty.extend(self.notify_schema_change_at_maint(tip_seq, tip_epoch, tip_maint));
        }
        Ok(all_dirty.into_iter().collect())
    }

    /// Applies one `CrossProcCommitSummary` selectively (public for deterministic unit testing).
    pub fn apply_cross_proc_summary(&mut self, schema: &CompiledSchema, cp: &CrossProcCommitSummary) -> Vec<WatchId> {
        self.apply_cross_proc_summary_at_maint(schema, cp, self.latest_maint_seq)
    }

    fn apply_cross_proc_summary_at_maint(
        &mut self,
        schema: &CompiledSchema,
        cp: &CrossProcCommitSummary,
        maint_seq: u64,
    ) -> Vec<WatchId> {
        let schema_changed = cp.schema_epoch != self.latest_schema_epoch;
        self.push_ring(cp.seq, maint_seq, cp.schema_epoch, CommitRingPayload::CrossProc(cp.clone()));
        if schema_changed {
            return self.all_watch_ids();
        }
        if self.watches.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::new();
        self.scratch_touched.reset(self.next_owner_idx as usize);

        // 1. AllEntities watchers.
        if !cp.touched_types.is_empty() || !cp.touched_rel_kinds.is_empty() {
            for &wid in &self.all_entities_watchers {
                if let Some(we) = self.watches.get(&wid)
                    && self.scratch_touched.set(we.owner_idx)
                {
                    out.push(wid);
                }
            }
        }

        // 2. Type-level invalidations: TypeScan, IndexedEq on type's indexes, and Tier 2 SubDbRange on type's indexes.
        for &tid in &cp.touched_types {
            if let Some(set) = self.by_type.get(&tid) {
                for &wid in set {
                    if let Some(we) = self.watches.get(&wid)
                        && self.scratch_touched.set(we.owner_idx)
                    {
                        out.push(wid);
                    }
                }
            }
            for def in &schema.indexes {
                if def.type_id != tid {
                    continue;
                }
                if let Some(set) = self.by_index_id.get(&def.id) {
                    for &wid in set {
                        if let Some(we) = self.watches.get(&wid)
                            && self.scratch_touched.set(we.owner_idx)
                        {
                            out.push(wid);
                        }
                    }
                }
                if let Some(arena) = self.slot_arenas.get(&def.slot) {
                    arena.stab_all(&mut self.scratch_touched, &mut out);
                }
            }
        }

        // 3. PointEntity invalidations: exact when `<= 64` entities touched, safe fallback when `None`!
        match &cp.touched_entities {
            Some(keys) => {
                for ek in keys {
                    if let Some(list) = self.by_point.get(ek) {
                        for (wid, owner_idx, _) in list {
                            if self.scratch_touched.set(*owner_idx) {
                                out.push(*wid);
                            }
                        }
                    }
                }
            }
            None => {
                // Overflowed 64-entity summary cap -> fail strictly safe for all PointEntity watches!
                for list in self.by_point.values() {
                    for (wid, owner_idx, _) in list {
                        if self.scratch_touched.set(*owner_idx) {
                            out.push(*wid);
                        }
                    }
                }
            }
        }

        // 4. Relation-kind invalidations: EdgeAdj watchers & Rank indexes!
        if !cp.touched_rel_kinds.is_empty() {
            for &wid in &self.wildcard_rel_adj {
                if let Some(we) = self.watches.get(&wid)
                    && self.scratch_touched.set(we.owner_idx)
                {
                    out.push(wid);
                }
            }
            for &rk in &cp.touched_rel_kinds {
                if let Some(set) = self.by_rel_kind_adj.get(&rk) {
                    for &wid in set {
                        if let Some(we) = self.watches.get(&wid)
                            && self.scratch_touched.set(we.owner_idx)
                        {
                            out.push(wid);
                        }
                    }
                }
            }
            for def in &schema.indexes {
                if let IndexKind::Rank { relations, .. } = &def.kind
                    && (relations.is_empty() || cp.touched_rel_kinds.iter().any(|rk| relations.contains(rk)))
                    && let Some(arena) = self.slot_arenas.get(&def.slot)
                {
                    arena.stab_all(&mut self.scratch_touched, &mut out);
                }
            }
        }

        out
    }

    pub fn clear_all_watches(&mut self) {
        self.watches.clear();
        self.all_entities_watchers.clear();
        self.by_type.clear();
        self.by_point.clear();
        self.by_indexed_eq.clear();
        self.by_index_id.clear();
        self.by_edge_adj.clear();
        self.by_rel_kind_adj.clear();
        self.wildcard_rel_adj.clear();
        self.slot_arenas.clear();
        self.free_owner_idxs.clear();
        self.next_owner_idx = 0;
    }

    fn all_watch_ids(&self) -> Vec<WatchId> {
        self.watches.keys().copied().collect()
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by live `notify_commit` and `CommitRing` catch-up replay
// ---------------------------------------------------------------------------

#[inline]
fn live_row(row: Option<&PackedRow>) -> Option<&PackedRow> {
    row.filter(|r| !r.header.is_trashed())
}

#[inline]
fn edge_endpoints(ed: &EdgeDelta) -> (Option<EntityKey>, Option<EntityKey>) {
    if let Some(ref e) = ed.after {
        (Some(e.src), Some(e.dst))
    } else if let Some(ref e) = ed.before {
        (Some(e.src), Some(e.dst))
    } else {
        (None, None)
    }
}

/// Computes the exact byte key written by `value::apply_delta` for `row` in a Value index sub-DB.
fn value_index_stab_key(row: &PackedRow, fields: &[elmdb_engine::FieldId], ek: EntityKey) -> Vec<u8> {
    let vals: Vec<&CanonicalValue> =
        fields.iter().map(|fid| row.fields.get(fid).unwrap_or(&CanonicalValue::Null)).collect();
    let mut k = Vec::new();
    match value::encode_compound_values(&vals) {
        ValueKeyClass::Indexed(encoded) => {
            k.push(0x01);
            k.extend_from_slice(&encoded);
        }
        ValueKeyClass::Without => {
            k.push(0x02);
            k.extend_from_slice(&ek.to_be_bytes());
        }
        ValueKeyClass::Unordered => {
            k.push(0x03);
            k.extend_from_slice(&ek.to_be_bytes());
        }
        ValueKeyClass::Long => {
            k.push(0x04);
            k.extend_from_slice(&ek.to_be_bytes());
        }
    }
    k
}

fn range_contains_key(r: &CoalescedRange, key: &[u8]) -> bool {
    key >= r.low.as_slice() && (r.high.is_empty() || key < r.high.as_slice())
}

fn range_overlaps_tag(r: &CoalescedRange, tag: u8) -> bool {
    if let Some(hi_b) = tag.checked_add(1)
        && r.low.as_slice() >= [hi_b].as_slice()
    {
        return false;
    }
    if !r.high.is_empty() && r.high.as_slice() <= [tag].as_slice() {
        return false;
    }
    true
}

fn entry_matches_subdb_key(entry: &WatchEntry, slot: u16, key: &[u8]) -> bool {
    entry.ranges.iter().any(|r| r.slot == slot && range_contains_key(r, key))
}

fn entry_matches_local_commit(entry: &WatchEntry, schema: &CompiledSchema, summary: &CommitDeltaSummary) -> bool {
    if entry.all_entities && (!summary.entity_deltas.is_empty() || !summary.edge_deltas.is_empty()) {
        return true;
    }
    if entry.type_scans.iter().any(|tid| summary.touched_types.contains(*tid)) {
        return true;
    }
    for delta in &summary.entity_deltas {
        if entry
            .point_entities
            .iter()
            .any(|(ek, pw)| *ek == delta.key && pw.matches_mutation(delta.liveness_changed, &delta.changed_fields))
        {
            return true;
        }
    }
    for ed in &summary.edge_deltas {
        let (src, dst) = edge_endpoints(ed);
        if entry.edge_adjs.iter().any(|ea| {
            (src == Some(ea.entity) && ea.matches_edge(ed.kind, EdgeDir::Out))
                || (dst == Some(ea.entity) && ea.matches_edge(ed.kind, EdgeDir::In))
        }) {
            return true;
        }
    }
    for def in &schema.indexes {
        match &def.kind {
            IndexKind::Value { fields, .. } => {
                if !summary.touched_types.contains(def.type_id) {
                    continue;
                }
                let is_single = fields.len() == 1;
                for delta in &summary.entity_deltas {
                    if delta.type_id != def.type_id {
                        continue;
                    }
                    if !delta.liveness_changed && !fields.iter().any(|f| delta.changed_fields.contains(*f)) {
                        continue;
                    }
                    for row_opt in [live_row(delta.before.as_ref()), live_row(delta.after.as_ref())] {
                        let Some(row) = row_opt else { continue };
                        if is_single {
                            let v = row.fields.get(&fields[0]).unwrap_or(&CanonicalValue::Null);
                            let h = hash_canonical_value(v);
                            if entry.indexed_eqs.contains(&(def.id, h)) {
                                return true;
                            }
                        }
                        let kb = value_index_stab_key(row, fields, delta.key);
                        if entry.ranges.iter().any(|r| r.slot == def.slot && range_contains_key(r, &kb)) {
                            return true;
                        }
                    }
                }
            }
            IndexKind::Order { field, .. } => {
                if !summary.touched_types.contains(def.type_id) {
                    continue;
                }
                for delta in &summary.entity_deltas {
                    if delta.type_id != def.type_id {
                        continue;
                    }
                    if !delta.liveness_changed && !delta.changed_fields.contains(*field) {
                        continue;
                    }
                    for row_opt in [live_row(delta.before.as_ref()), live_row(delta.after.as_ref())] {
                        let Some(row) = row_opt else { continue };
                        let tag =
                            if matches!(row.fields.get(field), Some(CanonicalValue::String(_))) { 0x01 } else { 0x02 };
                        if entry.ranges.iter().any(|r| r.slot == def.slot && range_overlaps_tag(r, tag)) {
                            return true;
                        }
                    }
                }
            }
            IndexKind::Text { fields } => {
                if summary.touched_types.contains(def.type_id)
                    && summary.entity_deltas.iter().any(|d| {
                        d.type_id == def.type_id
                            && (d.liveness_changed || fields.iter().any(|(f, _)| d.changed_fields.contains(*f)))
                    })
                    && entry.ranges.iter().any(|r| r.slot == def.slot)
                {
                    return true;
                }
            }
            IndexKind::Trigram { fields } | IndexKind::Vector { fields, .. } => {
                if summary.touched_types.contains(def.type_id)
                    && summary.entity_deltas.iter().any(|d| {
                        d.type_id == def.type_id
                            && (d.liveness_changed || fields.iter().any(|f| d.changed_fields.contains(*f)))
                    })
                    && entry.ranges.iter().any(|r| r.slot == def.slot)
                {
                    return true;
                }
            }
            IndexKind::Rank { relations, .. } => {
                let touched = summary.entity_deltas.iter().any(|d| d.type_id == def.type_id && d.liveness_changed)
                    || summary.edge_deltas.iter().any(|e| relations.is_empty() || relations.contains(&e.kind));
                if touched && entry.ranges.iter().any(|r| r.slot == def.slot) {
                    return true;
                }
            }
        }
    }
    false
}

fn entry_matches_cross_proc(entry: &WatchEntry, schema: &CompiledSchema, cp: &CrossProcCommitSummary) -> bool {
    if entry.all_entities && (!cp.touched_types.is_empty() || !cp.touched_rel_kinds.is_empty()) {
        return true;
    }
    if entry.type_scans.iter().any(|tid| cp.touched_types.contains(tid)) {
        return true;
    }
    match &cp.touched_entities {
        Some(keys) => {
            if entry.point_entities.iter().any(|(ek, _)| keys.contains(ek)) {
                return true;
            }
        }
        None => {
            if !entry.point_entities.is_empty() {
                return true;
            }
        }
    }
    if !cp.touched_rel_kinds.is_empty()
        && entry
            .edge_adjs
            .iter()
            .any(|ea| ea.rel_kinds.is_empty() || cp.touched_rel_kinds.iter().any(|rk| ea.rel_kinds.contains(rk)))
    {
        return true;
    }
    for def in &schema.indexes {
        let type_hit = cp.touched_types.contains(&def.type_id);
        let rank_hit = matches!(&def.kind, IndexKind::Rank { relations, .. } if !cp.touched_rel_kinds.is_empty() && (relations.is_empty() || cp.touched_rel_kinds.iter().any(|rk| relations.contains(rk))));
        if type_hit || rank_hit {
            if entry.indexed_eqs.iter().any(|(id, _)| *id == def.id) {
                return true;
            }
            if entry.ranges.iter().any(|r| r.slot == def.slot) {
                return true;
            }
        }
    }
    false
}
