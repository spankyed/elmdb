//! Two-tier `ReadSet` implementing `elmdb_query::QueryReadRecorder`.
//!
//! - **Tier 1 (Structural Predicates):** `PointEntity`, `IndexedEq`, `EdgeAdj`, `TypeScan`, `AllEntities`.
//! - **Tier 2 (Sub-DB Byte Ranges):** `[low, high)` intervals per pooled `SubDbSlot`, sorted and coalesced
//!   (merging overlapping or adjacent intervals) before registration into `InvalidationIndex`'s flat byte slab.

use std::collections::BTreeMap;

use elmdb_engine::{EntityKey, FieldMask, IndexId, RelKindId, TypeId};
use elmdb_query::{EdgeDir, QueryReadRecorder};
use rustc_hash::{FxHashMap, FxHashSet};

/// Opaque handle identifying an active live watch or tracked query subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WatchId(pub u64);

/// Field-level dependency on a single primary entity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PointWatch {
    /// When `true`, ANY field change on the entity invalidates (e.g., unprojected `Rows { fields: None }`).
    pub all_fields: bool,
    /// Specific fields read on this entity. Liveness transitions (create/trash/restore/destroy) ALWAYS invalidate!
    pub fields: FieldMask,
}

impl PointWatch {
    pub fn merge_fields(&mut self, mask: &FieldMask) {
        if !self.all_fields {
            self.fields.union_with(mask);
        }
    }

    pub fn merge_all_fields(&mut self) {
        self.all_fields = true;
    }

    /// Returns `true` if a mutation with `(liveness_changed, changed_fields)` invalidates this dependency.
    #[inline]
    pub fn matches_mutation(&self, liveness_changed: bool, changed_fields: &FieldMask) -> bool {
        if liveness_changed {
            return true;
        }
        if self.all_fields {
            return !changed_fields.is_empty();
        }
        self.fields.intersects(changed_fields)
    }
}

/// Adjacency-list dependency on `(entity, rel_kinds, dir)`.
/// Empty `rel_kinds` means wildcard (any relation kind).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeAdjWatch {
    pub entity: EntityKey,
    /// Empty = any relation kind; otherwise sorted deduplicated `RelKindId`s.
    pub rel_kinds: Vec<RelKindId>,
    pub dir: EdgeDir,
}

impl EdgeAdjWatch {
    #[inline]
    pub fn matches_edge(&self, kind: RelKindId, edge_dir_from_entity: EdgeDir) -> bool {
        let dir_ok = match (self.dir, edge_dir_from_entity) {
            (EdgeDir::Both, _) | (_, EdgeDir::Both) => true,
            (a, b) => a == b,
        };
        dir_ok && (self.rel_kinds.is_empty() || self.rel_kinds.contains(&kind))
    }
}

/// Coalesced half-open `[low, high)` byte interval on a pooled `SubDbSlot`.
/// An empty `high` represents unbounded upper (`+inf`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoalescedRange {
    pub slot: u16,
    pub low: Vec<u8>,
    /// Exclusive upper bound; empty `Vec` means unbounded (`+inf`).
    pub high: Vec<u8>,
}

type RawRangeList = Vec<(Vec<u8>, Vec<u8>)>;

/// Mutable recorder passed into `QueryEngine::execute_in_txn` / `execute_tracked`.
/// Collects Tier 1 structural predicates and Tier 2 raw ranges, coalescing them on demand.
#[derive(Debug, Default, Clone)]
pub struct ReadSet {
    pub all_entities: bool,
    pub type_scans: FxHashSet<TypeId>,
    pub point_entities: FxHashMap<EntityKey, PointWatch>,
    pub indexed_eqs: FxHashSet<(IndexId, u64)>,
    pub edge_adjs: Vec<EdgeAdjWatch>,
    raw_ranges: BTreeMap<u16, RawRangeList>,
}

impl ReadSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        !self.all_entities
            && self.type_scans.is_empty()
            && self.point_entities.is_empty()
            && self.indexed_eqs.is_empty()
            && self.edge_adjs.is_empty()
            && self.raw_ranges.is_empty()
    }

    /// Sorts and merges overlapping or adjacent `[low, high)` ranges per `SubDbSlot`!
    pub fn coalesce_ranges(&self) -> Vec<CoalescedRange> {
        let mut out = Vec::new();
        for (&slot, intervals) in &self.raw_ranges {
            if intervals.is_empty() {
                continue;
            }
            let mut sorted: Vec<(&[u8], &[u8])> = intervals.iter().map(|(l, h)| (l.as_slice(), h.as_slice())).collect();
            sorted.sort_unstable_by(|a, b| a.0.cmp(b.0).then_with(|| cmp_upper(a.1, b.1)));

            let mut cur_low = sorted[0].0;
            let mut cur_high = sorted[0].1;
            for &(low, high) in &sorted[1..] {
                // Overlapping or adjacent: `cur_high` is unbounded (`[]`) OR `low <= cur_high`.
                if cur_high.is_empty() || low <= cur_high {
                    if cmp_upper(high, cur_high) == std::cmp::Ordering::Greater {
                        cur_high = high;
                    }
                } else {
                    out.push(CoalescedRange { slot, low: cur_low.to_vec(), high: cur_high.to_vec() });
                    cur_low = low;
                    cur_high = high;
                }
            }
            out.push(CoalescedRange { slot, low: cur_low.to_vec(), high: cur_high.to_vec() });
        }
        out
    }

    /// Deduplicates and merges `edge_adjs` by `(entity, dir)` to keep Tier-1 tables compact.
    pub fn coalesce_edge_adjs(&self) -> Vec<EdgeAdjWatch> {
        let mut by_entity: FxHashMap<EntityKey, (Option<EdgeDir>, bool, Vec<RelKindId>)> = FxHashMap::default();
        for ea in &self.edge_adjs {
            let entry = by_entity.entry(ea.entity).or_insert((None, false, Vec::new()));
            entry.0 = Some(match entry.0 {
                None => ea.dir,
                Some(prev) if prev == ea.dir => prev,
                Some(_) => EdgeDir::Both,
            });
            if ea.rel_kinds.is_empty() {
                entry.1 = true;
                entry.2.clear();
            } else if !entry.1 {
                entry.2.extend_from_slice(&ea.rel_kinds);
            }
        }
        let mut out = Vec::with_capacity(by_entity.len());
        for (entity, (dir, wildcard, mut kinds)) in by_entity {
            if wildcard {
                kinds.clear();
            } else {
                kinds.sort_unstable();
                kinds.dedup();
            }
            out.push(EdgeAdjWatch { entity, rel_kinds: kinds, dir: dir.unwrap_or(EdgeDir::Both) });
        }
        out
    }
}

/// Compares two exclusive upper bounds where empty slice `&[]` denotes `+infinity`.
#[inline]
pub(crate) fn cmp_upper(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    match (a.is_empty(), b.is_empty()) {
        (true, true) => std::cmp::Ordering::Equal,
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        (false, false) => a.cmp(b),
    }
}

impl QueryReadRecorder for ReadSet {
    fn record_point_entity(&mut self, key: EntityKey, fields: &FieldMask) {
        self.point_entities.entry(key).or_default().merge_fields(fields);
    }

    fn record_point_entity_all_fields(&mut self, key: EntityKey) {
        self.point_entities.entry(key).or_default().merge_all_fields();
    }

    fn record_indexed_eq(&mut self, index_id: IndexId, value_hash: u64) {
        self.indexed_eqs.insert((index_id, value_hash));
    }

    fn record_edge_adj(&mut self, entity: EntityKey, rel_kinds: &[RelKindId], dir: EdgeDir) {
        self.edge_adjs.push(EdgeAdjWatch { entity, rel_kinds: rel_kinds.to_vec(), dir });
    }

    fn record_type_scan(&mut self, type_id: TypeId) {
        self.type_scans.insert(type_id);
    }

    fn record_all_entities(&mut self) {
        self.all_entities = true;
    }

    fn record_subdb_range(&mut self, slot: u16, low: &[u8], high: &[u8]) {
        // Ignore empty inverted intervals `[low, high)` where non-empty `high <= low`.
        if !high.is_empty() && high <= low {
            return;
        }
        self.raw_ranges.entry(slot).or_default().push((low.to_vec(), high.to_vec()));
    }
}
