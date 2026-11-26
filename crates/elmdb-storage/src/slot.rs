use std::collections::BTreeSet;

use crate::error::StorageError;

/// Maximum number of pooled projection sub-DB slots (leaving 32 fixed sub-DBs within `max_dbs = 512`).
pub const MAX_POOLED_SLOTS: u16 = 480;

/// Physical LMDB sub-database slot drawn from a bounded reusable pool.
///
/// Separating logical `IndexId(u32)` (monotonic, never reused) from physical `SubDbSlot(u16)`
/// ensures LMDB `MDB_dbi` handles never leak and `mdb_dbi_close()` is never needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubDbSlot(u16);

impl SubDbSlot {
    pub const fn new(raw: u16) -> Option<Self> {
        if raw < MAX_POOLED_SLOTS { Some(Self(raw)) } else { None }
    }

    pub const fn raw(self) -> u16 {
        self.0
    }

    /// Canonical LMDB sub-database name (`p_000` .. `p_479`).
    pub fn name(self) -> String {
        format!("p_{:03}", self.0)
    }
}

/// Bounded allocator for reusable projection LMDB sub-database slots.
///
/// Rebuilds and HNSW compactions allocate a staging slot, flip the active slot in `meta`,
/// truncate the retired slot with `mdb_drop(txn, dbi, 0)` (O(1), MVCC-safe for active readers),
/// and return the retired slot to `free`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubDbPool {
    max_slots: u16,
    high_water: u16,
    free: BTreeSet<u16>,
}

impl SubDbPool {
    pub fn new(max_slots: u16) -> Self {
        assert!(max_slots <= MAX_POOLED_SLOTS, "max_slots exceeds MAX_POOLED_SLOTS");
        Self { max_slots, high_water: 0, free: BTreeSet::new() }
    }

    /// Allocates a free recycled slot if available, or opens the next sequential slot.
    pub fn allocate(&mut self) -> Result<SubDbSlot, StorageError> {
        if let Some(reused) = self.free.pop_first() {
            return Ok(SubDbSlot(reused));
        }
        if self.high_water >= self.max_slots {
            return Err(StorageError::DbsFull { max_dbs: u32::from(self.max_slots) });
        }
        let slot = SubDbSlot(self.high_water);
        self.high_water += 1;
        Ok(slot)
    }

    /// Returns a retired (already truncated) slot back to the reusable pool.
    pub fn recycle(&mut self, slot: SubDbSlot) -> Result<(), StorageError> {
        if slot.0 >= self.high_water {
            return Err(StorageError::InvalidState(format!("cannot recycle unallocated sub-DB slot {}", slot.0)));
        }
        if !self.free.insert(slot.0) {
            return Err(StorageError::InvalidState(format!("sub-DB slot {} was recycled twice", slot.0)));
        }
        Ok(())
    }

    pub fn active_count(&self) -> u16 {
        self.high_water - u16::try_from(self.free.len()).unwrap_or(0)
    }

    pub fn high_water(&self) -> u16 {
        self.high_water
    }

    pub fn free_count(&self) -> usize {
        self.free.len()
    }

    /// Canonical binary encoding for persistence inside `meta`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(6 + self.free.len() * 2);
        out.extend_from_slice(&self.max_slots.to_le_bytes());
        out.extend_from_slice(&self.high_water.to_le_bytes());
        out.extend_from_slice(&u16::try_from(self.free.len()).expect("free fits in u16").to_le_bytes());
        for &slot in &self.free {
            out.extend_from_slice(&slot.to_le_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, StorageError> {
        if bytes.len() < 6 {
            return Err(StorageError::Corrupted("truncated SubDbPool metadata".into()));
        }
        let max_slots = u16::from_le_bytes([bytes[0], bytes[1]]);
        let high_water = u16::from_le_bytes([bytes[2], bytes[3]]);
        let free_len = usize::from(u16::from_le_bytes([bytes[4], bytes[5]]));
        if max_slots > MAX_POOLED_SLOTS || high_water > max_slots {
            return Err(StorageError::Corrupted("invalid SubDbPool bounds".into()));
        }
        if bytes.len() != 6 + free_len * 2 {
            return Err(StorageError::Corrupted("SubDbPool length mismatch".into()));
        }
        let mut free = BTreeSet::new();
        for &chunk in bytes[6..].as_chunks::<2>().0 {
            let slot = u16::from_le_bytes(chunk);
            if slot >= high_water || !free.insert(slot) {
                return Err(StorageError::Corrupted("invalid or duplicate free slot in SubDbPool".into()));
            }
        }
        Ok(Self { max_slots, high_water, free })
    }
}

impl Default for SubDbPool {
    fn default() -> Self {
        Self::new(MAX_POOLED_SLOTS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_pong_and_churn_stay_bounded() {
        let mut pool = SubDbPool::new(4);
        let mut active = pool.allocate().unwrap();
        assert_eq!(active.name(), "p_000");

        // Simulate 1,000 rebuild/compaction ping-pongs on a 4-slot pool.
        for _ in 0..1_000 {
            let staging = pool.allocate().unwrap();
            assert_ne!(staging, active);
            pool.recycle(active).unwrap();
            active = staging;
        }
        assert_eq!(pool.high_water(), 2);
        assert_eq!(pool.active_count(), 1);

        let encoded = pool.encode();
        assert_eq!(SubDbPool::decode(&encoded).unwrap(), pool);
    }

    #[test]
    fn double_recycle_and_exhaustion_are_rejected() {
        let mut pool = SubDbPool::new(2);
        let a = pool.allocate().unwrap();
        let b = pool.allocate().unwrap();
        assert!(matches!(pool.allocate(), Err(StorageError::DbsFull { max_dbs: 2 })));
        pool.recycle(a).unwrap();
        assert!(matches!(pool.recycle(a), Err(StorageError::InvalidState(_))));
        let c = pool.allocate().unwrap();
        assert_eq!(c, a);
        pool.recycle(b).unwrap();
        pool.recycle(c).unwrap();
        assert_eq!(pool.active_count(), 0);
    }
}
