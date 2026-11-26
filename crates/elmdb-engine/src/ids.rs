use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Strictly monotonic, never-reused internal entity identifier (8-byte native LMDB `INTEGER_KEY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntityKey(pub u64);

impl EntityKey {
    pub const fn to_ne_bytes(self) -> [u8; 8] {
        self.0.to_ne_bytes()
    }

    pub const fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    pub const fn from_ne_bytes(bytes: [u8; 8]) -> Self {
        Self(u64::from_ne_bytes(bytes))
    }

    pub const fn from_be_bytes(bytes: [u8; 8]) -> Self {
        Self(u64::from_be_bytes(bytes))
    }
}

/// Strictly monotonic, never-reused internal relation identifier (8-byte native LMDB `INTEGER_KEY`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RelKey(pub u64);

impl RelKey {
    pub const fn to_ne_bytes(self) -> [u8; 8] {
        self.0.to_ne_bytes()
    }

    pub const fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    pub const fn from_ne_bytes(bytes: [u8; 8]) -> Self {
        Self(u64::from_ne_bytes(bytes))
    }

    pub const fn from_be_bytes(bytes: [u8; 8]) -> Self {
        Self(u64::from_be_bytes(bytes))
    }
}

/// Monotonic, never-reused entity type identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TypeId(pub u16);

/// Monotonic, never-reused field identifier scoped to its owning `TypeId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FieldId(pub u16);

/// Monotonic, never-reused relation kind identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RelKindId(pub u16);

/// Monotonic, never-reused role identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RoleId(pub u16);

/// Strictly monotonic, never-reused logical index identifier (mapped to a pooled `SubDbSlot(u16)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IndexId(pub u32);

/// Compact set of `TypeId`s with fast inline bitmask for `TypeId < 128`, overflow set beyond,
/// and an explicit `all` wildcard flag so `TypeMask::all()` matches every present and future `TypeId`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeMask {
    #[serde(default)]
    all: bool,
    low: u128,
    high: BTreeSet<u16>,
}

impl TypeMask {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn all() -> Self {
        Self { all: true, low: 0, high: BTreeSet::new() }
    }

    pub fn is_all(&self) -> bool {
        self.all
    }

    pub fn insert(&mut self, id: TypeId) {
        if self.all {
            return;
        }
        if id.0 < 128 {
            self.low |= 1u128 << id.0;
        } else {
            self.high.insert(id.0);
        }
    }

    pub fn contains(&self, id: TypeId) -> bool {
        if self.all {
            return true;
        }
        if id.0 < 128 { (self.low & (1u128 << id.0)) != 0 } else { self.high.contains(&id.0) }
    }

    pub fn intersects(&self, other: &Self) -> bool {
        if self.is_empty() || other.is_empty() {
            return false;
        }
        if self.all || other.all {
            return true;
        }
        if (self.low & other.low) != 0 {
            return true;
        }
        self.high.iter().any(|id| other.high.contains(id))
    }

    pub fn union_with(&mut self, other: &Self) {
        if other.all {
            self.all = true;
            self.low = 0;
            self.high.clear();
            return;
        }
        if self.all {
            return;
        }
        self.low |= other.low;
        self.high.extend(other.high.iter().copied());
    }

    pub fn is_empty(&self) -> bool {
        !self.all && self.low == 0 && self.high.is_empty()
    }

    pub fn iter(&self) -> Vec<TypeId> {
        let mut out = Vec::new();
        let mut bits = self.low;
        while bits != 0 {
            let tz = bits.trailing_zeros() as u16;
            out.push(TypeId(tz));
            bits &= bits - 1;
        }
        out.extend(self.high.iter().copied().map(TypeId));
        out
    }
}

/// Compact set of `FieldId`s changed on an entity (fast bitmask dispatch for `ProjectionRegistry`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldMask {
    low: u128,
    high: BTreeSet<u16>,
}

impl FieldMask {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, id: FieldId) {
        if id.0 < 128 {
            self.low |= 1u128 << id.0;
        } else {
            self.high.insert(id.0);
        }
    }

    pub fn contains(&self, id: FieldId) -> bool {
        if id.0 < 128 { (self.low & (1u128 << id.0)) != 0 } else { self.high.contains(&id.0) }
    }

    pub fn intersects(&self, other: &Self) -> bool {
        if (self.low & other.low) != 0 {
            return true;
        }
        self.high.iter().any(|id| other.high.contains(id))
    }

    pub fn union_with(&mut self, other: &Self) {
        self.low |= other.low;
        self.high.extend(other.high.iter().copied());
    }

    pub fn is_empty(&self) -> bool {
        self.low == 0 && self.high.is_empty()
    }

    pub fn iter(&self) -> Vec<FieldId> {
        let mut out = Vec::new();
        let mut bits = self.low;
        while bits != 0 {
            let tz = bits.trailing_zeros() as u16;
            out.push(FieldId(tz));
            bits &= bits - 1;
        }
        out.extend(self.high.iter().copied().map(FieldId));
        out
    }
}
