//! Owned LMDB-backed HNSW vector engine (`#![forbid(unsafe_code)]`).
//!
//! Single sub-DB layout per vector index (`SubDbSlot`):
//! - `0x00` -> `HnswMeta`
//! - `0x01 || NodeId(4B BE)` -> `PackedHnswNode` (per-vector SQ8 default: 1536-dim `< 2 KiB`!)
//! - `0x02 || EntityKey(8B BE) || chunk(4B BE)` -> `PendingOp { Upsert(vec_gen) | Delete }`
//! - `0x03 || EntityKey(8B BE) || chunk(4B BE)` -> `LiveMapping { node_id: Option<u32>, vec_gen: u64 }`
//!
//! Primary exact F32 vectors live in `dbs.embeddings`:
//! - `IndexId(4B BE) || EntityKey(8B BE) || chunk(4B BE)` -> `vec_gen(8B LE) || dims(4B LE) || f32_le * dims`

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use elmdb_engine::{
    CompiledSchema, EdgeDelta, Engine, EntityDelta, EntityKey, IndexDef, IndexId, IndexKind, SystemDbs, read_packed_row,
};
use elmdb_projections::{BuildState, ExternalProjection, IndexMismatch};
use elmdb_storage::{Clock, DbFlags, GatedWriteTxn, KvCursor, KvRead, KvStore, SubDbHandle, Vfs};
use elmdb_wire::{ElmdbError, ErrorCode, Result};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::quant::{
    QuantVec, Quantization, cosine_distance_f32, cosine_distance_quant, cosine_distance_query_vs_quant, l2_norm,
};

const TAG_META: u8 = 0x00;
const TAG_NODES: u8 = 0x01;
const TAG_PENDING: u8 = 0x02;
const TAG_LIVE_MAP: u8 = 0x03;

const PENDING_UPSERT: u8 = 0x01;
const PENDING_DELETE: u8 = 0x02;

/// HNSW graph structural constants.
pub const HNSW_M: usize = 16;
pub const HNSW_M0: usize = 32;
pub const HNSW_EF_CONSTRUCTION: usize = 64;
pub const HNSW_MAX_LEVEL: u8 = 16;

/// Bounded per-commit pending drain batch size (spec budget: 32–64 vectors/commit).
pub const DEFAULT_DRAIN_BATCH: usize = 48;
/// Pending-queue length above which callers receive adaptive backpressure signaling.
pub const PENDING_BACKPRESSURE_THRESHOLD: u64 = 1024;
/// Tight filter candidate cardinality threshold below which exact F32 scan beats HNSW.
pub const EXACT_SCAN_THRESHOLD: usize = 2048;
/// Lazy ping-pong compaction trigger: > 1,024 total nodes and >= 25% tombstones.
pub const COMPACT_MIN_NODES: u64 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswMeta {
    pub dims: u32,
    pub quantization: Quantization,
    pub seed: u64,
    pub next_node_id: u32,
    pub next_vec_gen: u64,
    pub entry_point: Option<u32>,
    pub max_level: u8,
    pub live_nodes: u64,
    pub dead_nodes: u64,
    pub pending_count: u64,
}

impl HnswMeta {
    pub fn new(dims: u32, quantization: Quantization, seed: u64) -> Self {
        Self {
            dims,
            quantization,
            seed,
            next_node_id: 0,
            next_vec_gen: 1,
            entry_point: None,
            max_level: 0,
            live_nodes: 0,
            dead_nodes: 0,
            pending_count: 0,
        }
    }

    pub fn should_compact(&self) -> bool {
        let total = self.live_nodes.saturating_add(self.dead_nodes);
        total > COMPACT_MIN_NODES && self.dead_nodes.saturating_mul(4) >= total
    }

    fn encode(self) -> [u8; 55] {
        let mut buf = [0u8; 55];
        buf[0..4].copy_from_slice(&self.dims.to_le_bytes());
        buf[4] = self.quantization.tag();
        buf[5..13].copy_from_slice(&self.seed.to_le_bytes());
        buf[13..17].copy_from_slice(&self.next_node_id.to_le_bytes());
        buf[17..25].copy_from_slice(&self.next_vec_gen.to_le_bytes());
        match self.entry_point {
            Some(ep) => {
                buf[25] = 1;
                buf[26..30].copy_from_slice(&ep.to_le_bytes());
            }
            None => {
                buf[25] = 0;
            }
        }
        buf[30] = self.max_level;
        buf[31..39].copy_from_slice(&self.live_nodes.to_le_bytes());
        buf[39..47].copy_from_slice(&self.dead_nodes.to_le_bytes());
        buf[47..55].copy_from_slice(&self.pending_count.to_le_bytes());
        buf
    }

    fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() != 55 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt hnsw meta"));
        }
        let dims = u32::from_le_bytes(raw[0..4].try_into().expect("4"));
        let quantization = Quantization::from_tag(raw[4])?;
        let seed = u64::from_le_bytes(raw[5..13].try_into().expect("8"));
        let next_node_id = u32::from_le_bytes(raw[13..17].try_into().expect("4"));
        let next_vec_gen = u64::from_le_bytes(raw[17..25].try_into().expect("8"));
        let entry_point = match raw[25] {
            0 => None,
            1 => Some(u32::from_le_bytes(raw[26..30].try_into().expect("4"))),
            other => return Err(ElmdbError::new(ErrorCode::Corrupted, format!("corrupt ep flag {other}"))),
        };
        let max_level = raw[30];
        let live_nodes = u64::from_le_bytes(raw[31..39].try_into().expect("8"));
        let dead_nodes = u64::from_le_bytes(raw[39..47].try_into().expect("8"));
        let pending_count = u64::from_le_bytes(raw[47..55].try_into().expect("8"));
        if dims == 0 || dims > 16_384 || max_level > HNSW_MAX_LEVEL || next_vec_gen == 0 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "hnsw meta bounds check failed"));
        }
        Ok(Self {
            dims,
            quantization,
            seed,
            next_node_id,
            next_vec_gen,
            entry_point,
            max_level,
            live_nodes,
            dead_nodes,
            pending_count,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PackedHnswNode {
    entity: EntityKey,
    chunk: u32,
    vec_gen: u64,
    level: u8,
    vec: QuantVec,
    neighbors: Vec<Vec<u32>>,
}

impl PackedHnswNode {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.vec.dims());
        out.extend_from_slice(&self.entity.0.to_le_bytes());
        out.extend_from_slice(&self.chunk.to_le_bytes());
        out.extend_from_slice(&self.vec_gen.to_le_bytes());
        out.push(self.level);
        self.vec.encode(&mut out);
        out.push(self.neighbors.len() as u8);
        for layer in &self.neighbors {
            out.extend_from_slice(&(layer.len() as u16).to_le_bytes());
            for &nid in layer {
                out.extend_from_slice(&nid.to_le_bytes());
            }
        }
        out
    }

    fn decode(raw: &[u8], expected_dims: usize) -> Result<Self> {
        if raw.len() < 21 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated hnsw node"));
        }
        let entity = EntityKey(u64::from_le_bytes(raw[0..8].try_into().expect("8")));
        let chunk = u32::from_le_bytes(raw[8..12].try_into().expect("4"));
        let vec_gen = u64::from_le_bytes(raw[12..20].try_into().expect("8"));
        let level = raw[20];
        if level > HNSW_MAX_LEVEL {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "hnsw node level exceeds max"));
        }
        let mut pos = 21usize;
        let vec = QuantVec::decode(raw, &mut pos, expected_dims)?;
        if pos >= raw.len() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated hnsw layer count"));
        }
        let layer_count = raw[pos] as usize;
        pos += 1;
        if layer_count != (level as usize) + 1 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "hnsw layer count does not match node level"));
        }
        let mut neighbors = Vec::with_capacity(layer_count);
        for _ in 0..layer_count {
            if pos + 2 > raw.len() {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated hnsw neighbor count"));
            }
            let n = u16::from_le_bytes([raw[pos], raw[pos + 1]]) as usize;
            pos += 2;
            let bytes =
                n.checked_mul(4).ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "neighbor len overflow"))?;
            let end =
                pos.checked_add(bytes).ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "neighbor pos overflow"))?;
            if end > raw.len() {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated hnsw neighbors"));
            }
            let mut layer = Vec::with_capacity(n);
            for &c in raw[pos..end].as_chunks::<4>().0 {
                layer.push(u32::from_le_bytes(c));
            }
            pos = end;
            neighbors.push(layer);
        }
        if pos != raw.len() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "trailing bytes in hnsw node"));
        }
        Ok(Self { entity, chunk, vec_gen, level, vec, neighbors })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LiveMapping {
    node_id: Option<u32>,
    vec_gen: u64,
}

impl LiveMapping {
    fn encode(self) -> [u8; 13] {
        let mut buf = [0u8; 13];
        match self.node_id {
            Some(id) => {
                buf[0] = 1;
                buf[1..5].copy_from_slice(&id.to_le_bytes());
            }
            None => {
                buf[0] = 0;
            }
        }
        buf[5..13].copy_from_slice(&self.vec_gen.to_le_bytes());
        buf
    }

    fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() != 13 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt live_map entry"));
        }
        let node_id = match raw[0] {
            0 => None,
            1 => Some(u32::from_le_bytes(raw[1..5].try_into().expect("4"))),
            other => return Err(ElmdbError::new(ErrorCode::Corrupted, format!("corrupt live_map flag {other}"))),
        };
        let vec_gen = u64::from_le_bytes(raw[5..13].try_into().expect("8"));
        Ok(Self { node_id, vec_gen })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingOp {
    Upsert(u64),
    Delete,
}

impl PendingOp {
    fn encode(self) -> [u8; 9] {
        let mut buf = [0u8; 9];
        match self {
            Self::Upsert(g) => {
                buf[0] = PENDING_UPSERT;
                buf[1..9].copy_from_slice(&g.to_le_bytes());
            }
            Self::Delete => {
                buf[0] = PENDING_DELETE;
            }
        }
        buf
    }

    fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() != 9 {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt vec_pending op"));
        }
        match raw[0] {
            PENDING_UPSERT => Ok(Self::Upsert(u64::from_le_bytes(raw[1..9].try_into().expect("8")))),
            PENDING_DELETE => Ok(Self::Delete),
            other => Err(ElmdbError::new(ErrorCode::Corrupted, format!("unknown pending op {other}"))),
        }
    }
}

/// Deterministic level assignment seeded by `(seed, entity_key, chunk)` using SplitMix64 mixing.
pub fn deterministic_level(seed: u64, entity: EntityKey, chunk: u32) -> u8 {
    let mut z =
        seed ^ entity.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ ((chunk as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Map to open interval (0, 1] using top 53 bits.
    let u = ((z >> 11) as f64 + 1.0) / ((1u64 << 53) as f64);
    let m_l = 1.0 / (HNSW_M as f64).ln();
    let lvl = (-u.ln() * m_l).floor() as u32;
    lvl.min(HNSW_MAX_LEVEL as u32) as u8
}

// Key builders
fn node_key(id: u32) -> [u8; 5] {
    let mut k = [0u8; 5];
    k[0] = TAG_NODES;
    k[1..5].copy_from_slice(&id.to_be_bytes());
    k
}

fn pending_key(entity: EntityKey, chunk: u32) -> [u8; 13] {
    let mut k = [0u8; 13];
    k[0] = TAG_PENDING;
    k[1..9].copy_from_slice(&entity.to_be_bytes());
    k[9..13].copy_from_slice(&chunk.to_be_bytes());
    k
}

fn live_map_key(entity: EntityKey, chunk: u32) -> [u8; 13] {
    let mut k = [0u8; 13];
    k[0] = TAG_LIVE_MAP;
    k[1..9].copy_from_slice(&entity.to_be_bytes());
    k[9..13].copy_from_slice(&chunk.to_be_bytes());
    k
}

fn primary_emb_key(index_id: IndexId, entity: EntityKey, chunk: u32) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[0..4].copy_from_slice(&index_id.0.to_be_bytes());
    k[4..12].copy_from_slice(&entity.to_be_bytes());
    k[12..16].copy_from_slice(&chunk.to_be_bytes());
    k
}

fn encode_primary_emb(vec_gen: u64, vec: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + vec.len() * 4);
    out.extend_from_slice(&vec_gen.to_le_bytes());
    out.extend_from_slice(&(vec.len() as u32).to_le_bytes());
    for &f in vec {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

fn decode_primary_emb(raw: &[u8], expected_dims: usize) -> Result<(u64, Vec<f32>)> {
    if raw.len() < 12 {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "truncated primary embedding"));
    }
    let vec_gen = u64::from_le_bytes(raw[0..8].try_into().expect("8"));
    let dims = u32::from_le_bytes(raw[8..12].try_into().expect("4")) as usize;
    let expected_bytes = dims
        .checked_mul(4)
        .and_then(|b| b.checked_add(12))
        .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "primary embedding byte size overflow"))?;
    if dims != expected_dims || raw.len() != expected_bytes {
        return Err(ElmdbError::new(ErrorCode::Corrupted, "primary embedding length/dims mismatch"));
    }
    let mut vec = Vec::with_capacity(dims);
    for &c in raw[12..].as_chunks::<4>().0 {
        let f = f32::from_le_bytes(c);
        if !f.is_finite() {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "non-finite primary embedding"));
        }
        vec.push(f);
    }
    Ok((vec_gen, vec))
}

fn validate_vector(vec: &[f32], expected_dims: u32) -> Result<()> {
    if vec.len() != expected_dims as usize {
        return Err(ElmdbError::new(
            ErrorCode::ValidationFailed,
            format!("vector dimension mismatch: expected {expected_dims}, got {}", vec.len()),
        ));
    }
    if vec.iter().any(|f| !f.is_finite()) {
        return Err(ElmdbError::new(ErrorCode::ValidationFailed, "vector contains non-finite float"));
    }
    Ok(())
}

fn load_meta_or_init<R: KvRead>(txn: &R, db: SubDbHandle, def: &IndexDef) -> Result<HnswMeta> {
    let IndexKind::Vector { dims, quantization, .. } = &def.kind else {
        return Err(ElmdbError::new(ErrorCode::InvalidArgument, "index is not a Vector index"));
    };
    let expected_q = Quantization::parse(quantization)?;
    if let Some(raw) = txn.get(db, &[TAG_META])? {
        let meta = HnswMeta::decode(raw)?;
        if meta.dims != *dims || meta.quantization != expected_q {
            return Err(ElmdbError::new(ErrorCode::Corrupted, "hnsw meta does not match index definition"));
        }
        return Ok(meta);
    }
    Ok(HnswMeta::new(*dims, expected_q, 0xE1ED_B200_0000_0000 ^ (def.id.0 as u64)))
}

/// Returned by embedding writes so callers can apply adaptive backpressure when `pending_count > 1024`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackpressureHint {
    pub pending_count: u64,
    pub backpressure: bool,
}

/// Scored vector search hit (`score` is exact F32 cosine similarity in `[-1.0, 1.0]`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VectorHit {
    pub key: EntityKey,
    pub chunk: u32,
    pub score: f32,
}

/// Diagnostic stats for a vector index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorStats {
    pub dims: u32,
    pub quantization: Quantization,
    pub live_nodes: u64,
    pub dead_nodes: u64,
    pub pending_count: u64,
    pub needs_compaction: bool,
}

// ---------------------------------------------------------------------------
// O(1) Commit-Time Embedding Writes (Zero Commit-Time HNSW Graph Stall!)
// ---------------------------------------------------------------------------

/// Reads all primary exact F32 embedding chunks stored for `(index_id, entity)` inside `ro`.
pub fn read_entity_embeddings<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    index_id: IndexId,
    entity: EntityKey,
    dims: usize,
) -> Result<Vec<Vec<f32>>> {
    let mut prefix = [0u8; 12];
    prefix[0..4].copy_from_slice(&index_id.0.to_be_bytes());
    prefix[4..12].copy_from_slice(&entity.to_be_bytes());
    let mut cur = txn.cursor(dbs.embeddings)?;
    let mut item = cur.seek_ge(&prefix)?;
    let mut out = Vec::new();
    while let Some((k, v)) = item {
        if k.len() != 16 || !k.starts_with(&prefix) {
            break;
        }
        let (_, vec) = decode_primary_emb(v, dims)?;
        out.push(vec);
        item = cur.next()?;
    }
    Ok(out)
}

/// Atomically stores exact F32 embedding chunk(s) for `entity` in `dbs.embeddings` and enqueues them
/// into `TAG_PENDING` in O(chunks) — zero HNSW graph mutation on the commit critical path!
pub fn set_entity_embeddings<S: KvStore, V: Vfs, C: Clock>(
    engine: &Engine<S, V, C>,
    index_id: IndexId,
    entity: EntityKey,
    chunks: &[Vec<f32>],
) -> Result<BackpressureHint> {
    let (hint, _) = engine.run_maintenance_write(|txn, schema, dbs| {
        let def = schema
            .index_by_id(index_id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", index_id.0)))?;
        let IndexKind::Vector { dims, .. } = &def.kind else {
            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "index is not a Vector index"));
        };
        for c in chunks {
            validate_vector(c, *dims)?;
        }
        // Verify entity exists, matches index type, and is live (not trashed).
        let row = read_packed_row(txn, dbs, entity)?
            .ok_or_else(|| ElmdbError::new(ErrorCode::EntityNotFound, format!("entity {entity:?} not found")))?;
        if row.header.type_id != def.type_id {
            return Err(ElmdbError::new(ErrorCode::EndpointMismatch, "entity type does not match vector index"));
        }
        if row.header.is_trashed() {
            return Err(ElmdbError::new(ErrorCode::EntityTrashed, "cannot embed trashed entity"));
        }

        let db = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
        let mut meta = load_meta_or_init(txn, db, def)?;

        // Collect existing chunks for this entity so removed trailing chunks are cleaned up.
        let old_chunks = list_live_chunks_for_entity(txn, db, entity)?;

        for (chunk_idx, vec) in chunks.iter().enumerate() {
            let chunk = chunk_idx as u32;
            let vec_gen = meta.next_vec_gen;
            meta.next_vec_gen = meta
                .next_vec_gen
                .checked_add(1)
                .ok_or_else(|| ElmdbError::new(ErrorCode::StorageFull, "vector generation counter exhausted"))?;

            // Persist exact F32 vector in primary `dbs.embeddings`.
            txn.put(dbs.embeddings, &primary_emb_key(index_id, entity, chunk), &encode_primary_emb(vec_gen, vec))?;

            // Update `TAG_LIVE_MAP`: if an HNSW node already existed for `(entity, chunk)`, retire it to `dead_nodes`!
            let lmk = live_map_key(entity, chunk);
            if let Some(old_raw) = txn.get(db, &lmk)? {
                let old_map = LiveMapping::decode(old_raw)?;
                if old_map.node_id.is_some() {
                    meta.live_nodes = meta.live_nodes.saturating_sub(1);
                    meta.dead_nodes = meta.dead_nodes.saturating_add(1);
                }
            }
            txn.put(db, &lmk, &LiveMapping { node_id: None, vec_gen }.encode())?;

            // Enqueue into persisted `TAG_PENDING`.
            let pk = pending_key(entity, chunk);
            let existed_pending = txn.get(db, &pk)?.is_some();
            txn.put(db, &pk, &PendingOp::Upsert(vec_gen).encode())?;
            if !existed_pending {
                meta.pending_count = meta.pending_count.saturating_add(1);
            }
        }

        // Remove any trailing chunks that no longer exist.
        let new_len = chunks.len() as u32;
        for (old_chunk, old_map) in old_chunks {
            if old_chunk >= new_len {
                remove_single_chunk(txn, dbs, db, (index_id, entity, old_chunk), old_map, &mut meta, true)?;
            }
        }

        txn.put(db, &[TAG_META], &meta.encode())?;
        Ok(BackpressureHint {
            pending_count: meta.pending_count,
            backpressure: meta.pending_count > PENDING_BACKPRESSURE_THRESHOLD,
        })
    })?;
    Ok(hint)
}

/// Atomically resolves `ext_id` and its type-scoped vector `index_name` (supporting both
/// unqualified `"emb"` scoped to the entity's own `TypeId` and `"TypeName.emb"`) inside ONE
/// write transaction so there is zero TOCTOU race and zero cross-type index ambiguity!
pub fn set_entity_embeddings_by_ext_id<S: KvStore, V: Vfs, C: Clock>(
    engine: &Engine<S, V, C>,
    index_name: &str,
    ext_id: &str,
    chunks: &[Vec<f32>],
) -> Result<(IndexId, BackpressureHint)> {
    let ((index_id, hint), _) = engine.run_maintenance_write(|txn, schema, dbs| {
        let entity = elmdb_engine::lookup_ext_id_in_txn(txn, dbs, ext_id)?
            .ok_or_else(|| ElmdbError::new(ErrorCode::EntityNotFound, format!("entity '{ext_id}' not found")))?;
        let row = read_packed_row(txn, dbs, entity)?
            .ok_or_else(|| ElmdbError::new(ErrorCode::EntityNotFound, format!("entity '{ext_id}' not found")))?;
        if row.header.is_trashed() {
            return Err(ElmdbError::new(ErrorCode::EntityTrashed, "cannot embed trashed entity"));
        }

        // Check exact unqualified index name on this entity's type first (supports dotted index names like `v1.emb`),
        // then fall back to `TypeName.indexName` qualification.
        let def = if let Some(exact) =
            schema.indexes.iter().find(|i| i.type_id == row.header.type_id && i.name == index_name)
        {
            exact
        } else {
            let (req_type_name, short_name) = match index_name.split_once('.') {
                Some((t, n)) => (Some(t), n),
                None => (None, index_name),
            };
            if let Some(tn) = req_type_name {
                let td = schema
                    .type_by_id(row.header.type_id)
                    .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, "row references unknown TypeId"))?;
                if td.name != tn {
                    return Err(ElmdbError::new(
                        ErrorCode::EndpointMismatch,
                        format!("entity '{ext_id}' has type '{}', not '{tn}'", td.name),
                    ));
                }
            }
            schema.indexes.iter().find(|i| i.type_id == row.header.type_id && i.name == short_name).ok_or_else(
                || {
                    if schema.indexes.iter().any(|i| i.name == short_name || i.name == index_name) {
                        ElmdbError::new(ErrorCode::EndpointMismatch, "entity type does not match vector index")
                    } else {
                        ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index '{index_name}'"))
                    }
                },
            )?
        };
        let IndexKind::Vector { dims, .. } = &def.kind else {
            return Err(ElmdbError::new(
                ErrorCode::InvalidArgument,
                format!("index '{index_name}' is not a vector index"),
            ));
        };
        for c in chunks {
            validate_vector(c, *dims)?;
        }

        let index_id = def.id;
        let db = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
        let mut meta = load_meta_or_init(txn, db, def)?;
        let old_chunks = list_live_chunks_for_entity(txn, db, entity)?;

        for (chunk_idx, vec) in chunks.iter().enumerate() {
            let chunk = chunk_idx as u32;
            let vec_gen = meta.next_vec_gen;
            meta.next_vec_gen = meta
                .next_vec_gen
                .checked_add(1)
                .ok_or_else(|| ElmdbError::new(ErrorCode::StorageFull, "vector generation counter exhausted"))?;

            txn.put(dbs.embeddings, &primary_emb_key(index_id, entity, chunk), &encode_primary_emb(vec_gen, vec))?;

            let lmk = live_map_key(entity, chunk);
            if let Some(old_raw) = txn.get(db, &lmk)? {
                let old_map = LiveMapping::decode(old_raw)?;
                if old_map.node_id.is_some() {
                    meta.live_nodes = meta.live_nodes.saturating_sub(1);
                    meta.dead_nodes = meta.dead_nodes.saturating_add(1);
                }
            }
            txn.put(db, &lmk, &LiveMapping { node_id: None, vec_gen }.encode())?;

            let pk = pending_key(entity, chunk);
            let existed_pending = txn.get(db, &pk)?.is_some();
            txn.put(db, &pk, &PendingOp::Upsert(vec_gen).encode())?;
            if !existed_pending {
                meta.pending_count = meta.pending_count.saturating_add(1);
            }
        }

        let new_len = chunks.len() as u32;
        for (old_chunk, old_map) in old_chunks {
            if old_chunk >= new_len {
                remove_single_chunk(txn, dbs, db, (index_id, entity, old_chunk), old_map, &mut meta, true)?;
            }
        }

        txn.put(db, &[TAG_META], &meta.encode())?;
        Ok((
            index_id,
            BackpressureHint {
                pending_count: meta.pending_count,
                backpressure: meta.pending_count > PENDING_BACKPRESSURE_THRESHOLD,
            },
        ))
    })?;
    Ok((index_id, hint))
}

/// Removes all embeddings for `entity` in `index_id` cleanly (both primary and derived).
pub fn delete_entity_embeddings<S: KvStore, V: Vfs, C: Clock>(
    engine: &Engine<S, V, C>,
    index_id: IndexId,
    entity: EntityKey,
) -> Result<()> {
    engine.run_maintenance_write(|txn, schema, dbs| {
        let def = schema
            .index_by_id(index_id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", index_id.0)))?;
        if !matches!(def.kind, IndexKind::Vector { .. }) {
            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "index is not a Vector index"));
        }
        let db = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
        remove_all_chunks_for_entity(txn, dbs, db, def, entity, true)
    })?;
    Ok(())
}

fn list_live_chunks_for_entity<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    entity: EntityKey,
) -> Result<Vec<(u32, LiveMapping)>> {
    let mut prefix = [0u8; 9];
    prefix[0] = TAG_LIVE_MAP;
    prefix[1..9].copy_from_slice(&entity.to_be_bytes());
    let mut cur = txn.cursor(db)?;
    let mut out = Vec::new();
    let mut step = cur.seek_ge(&prefix)?;
    while let Some((k, v)) = step {
        if k.len() != 13 || !k.starts_with(&prefix) {
            break;
        }
        let chunk = u32::from_be_bytes(k[9..13].try_into().expect("4"));
        out.push((chunk, LiveMapping::decode(v)?));
        step = cur.next()?;
    }
    Ok(out)
}

fn remove_single_chunk<S: KvStore, V: Vfs>(
    txn: &mut GatedWriteTxn<'_, S, V>,
    dbs: &SystemDbs,
    db: SubDbHandle,
    target: (IndexId, EntityKey, u32),
    old_map: LiveMapping,
    meta: &mut HnswMeta,
    delete_primary: bool,
) -> Result<()> {
    let (index_id, entity, chunk) = target;
    if delete_primary {
        txn.del(dbs.embeddings, &primary_emb_key(index_id, entity, chunk))?;
    }
    txn.del(db, &live_map_key(entity, chunk))?;
    if old_map.node_id.is_some() {
        meta.live_nodes = meta.live_nodes.saturating_sub(1);
        meta.dead_nodes = meta.dead_nodes.saturating_add(1);
    }
    let pk = pending_key(entity, chunk);
    if txn.del(db, &pk)? {
        meta.pending_count = meta.pending_count.saturating_sub(1);
    }
    Ok(())
}

fn remove_all_chunks_for_entity<S: KvStore, V: Vfs>(
    txn: &mut GatedWriteTxn<'_, S, V>,
    dbs: &SystemDbs,
    db: SubDbHandle,
    def: &IndexDef,
    entity: EntityKey,
    delete_primary: bool,
) -> Result<()> {
    let chunks = list_live_chunks_for_entity(txn, db, entity)?;
    let mut meta = load_meta_or_init(txn, db, def)?;
    let mut touched = false;
    for (chunk, map) in chunks {
        remove_single_chunk(txn, dbs, db, (def.id, entity, chunk), map, &mut meta, delete_primary)?;
        touched = true;
    }
    if delete_primary {
        // Also purge any primary embeddings preserved while entity was trashed!
        let mut prefix = [0u8; 12];
        prefix[0..4].copy_from_slice(&def.id.0.to_be_bytes());
        prefix[4..12].copy_from_slice(&entity.to_be_bytes());
        let mut to_del = Vec::new();
        {
            let mut cur = txn.cursor(dbs.embeddings)?;
            let mut step = cur.seek_ge(&prefix)?;
            while let Some((k, _)) = step {
                if k.len() != 16 || !k.starts_with(&prefix) {
                    break;
                }
                to_del.push(k.to_vec());
                step = cur.next()?;
            }
        }
        for k in to_del {
            txn.del(dbs.embeddings, &k)?;
        }
    }
    if touched {
        txn.put(db, &[TAG_META], &meta.encode())?;
    }
    Ok(())
}

/// Re-enqueues preserved primary embeddings for a restored entity back into `TAG_LIVE_MAP` + `TAG_PENDING`.
fn restore_chunks_for_entity<S: KvStore, V: Vfs>(
    txn: &mut GatedWriteTxn<'_, S, V>,
    dbs: &SystemDbs,
    db: SubDbHandle,
    def: &IndexDef,
    entity: EntityKey,
) -> Result<()> {
    let mut meta = load_meta_or_init(txn, db, def)?;
    let dims = meta.dims as usize;
    let mut prefix = [0u8; 12];
    prefix[0..4].copy_from_slice(&def.id.0.to_be_bytes());
    prefix[4..12].copy_from_slice(&entity.to_be_bytes());
    let mut restored: Vec<(u32, Vec<f32>)> = Vec::new();
    {
        let mut cur = txn.cursor(dbs.embeddings)?;
        let mut step = cur.seek_ge(&prefix)?;
        while let Some((k, v)) = step {
            if k.len() != 16 || !k.starts_with(&prefix) {
                break;
            }
            let chunk = u32::from_be_bytes(k[12..16].try_into().expect("4"));
            let (_, vec) = decode_primary_emb(v, dims)?;
            restored.push((chunk, vec));
            step = cur.next()?;
        }
    }
    if restored.is_empty() {
        return Ok(());
    }
    for (chunk, vec) in restored {
        let vec_gen = meta.next_vec_gen;
        meta.next_vec_gen = meta
            .next_vec_gen
            .checked_add(1)
            .ok_or_else(|| ElmdbError::new(ErrorCode::StorageFull, "vector generation counter exhausted"))?;
        txn.put(dbs.embeddings, &primary_emb_key(def.id, entity, chunk), &encode_primary_emb(vec_gen, &vec))?;
        txn.put(db, &live_map_key(entity, chunk), &LiveMapping { node_id: None, vec_gen }.encode())?;
        let pk = pending_key(entity, chunk);
        let existed = txn.get(db, &pk)?.is_some();
        txn.put(db, &pk, &PendingOp::Upsert(vec_gen).encode())?;
        if !existed {
            meta.pending_count = meta.pending_count.saturating_add(1);
        }
    }
    txn.put(db, &[TAG_META], &meta.encode())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// HNSW Insertion & Search Algorithms
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct DistNode {
    dist: f32,
    id: u32,
}

impl PartialEq for DistNode {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.dist.to_bits() == other.dist.to_bits()
    }
}
impl Eq for DistNode {}

/// Max-heap ordering (largest distance at top, used for bounded `ef` result set).
impl PartialOrd for DistNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for DistNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist.total_cmp(&other.dist).then_with(|| self.id.cmp(&other.id))
    }
}

/// Min-heap ordering (smallest distance at top, used for candidate exploration queue).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MinDistNode(DistNode);

impl PartialOrd for MinDistNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for MinDistNode {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.dist.total_cmp(&self.0.dist).then_with(|| other.0.id.cmp(&self.0.id))
    }
}

fn load_node<R: KvRead>(txn: &R, db: SubDbHandle, id: u32, dims: usize) -> Result<PackedHnswNode> {
    let raw = txn
        .get(db, &node_key(id))?
        .ok_or_else(|| ElmdbError::new(ErrorCode::Corrupted, format!("missing hnsw node {id}")))?;
    PackedHnswNode::decode(raw, dims)
}

#[derive(Clone, Copy)]
struct QueryVec<'a> {
    vec: &'a [f32],
    norm: f64,
}

fn greedy_search_layer<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    q: QueryVec<'_>,
    mut curr_id: u32,
    mut curr_dist: f32,
    layer: usize,
) -> Result<(u32, f32)> {
    let dims = q.vec.len();
    loop {
        let node = load_node(txn, db, curr_id, dims)?;
        let mut improved = false;
        if let Some(neighs) = node.neighbors.get(layer) {
            for &nid in neighs {
                let n = load_node(txn, db, nid, dims)?;
                let d = cosine_distance_query_vs_quant(q.vec, q.norm, &n.vec);
                if d < curr_dist {
                    curr_dist = d;
                    curr_id = nid;
                    improved = true;
                }
            }
        }
        if !improved {
            return Ok((curr_id, curr_dist));
        }
    }
}

fn search_layer_beam<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    q: QueryVec<'_>,
    entry_id: u32,
    ef: usize,
    layer: usize,
) -> Result<Vec<DistNode>> {
    let dims = q.vec.len();
    let ef = ef.max(1);
    let ep_node = load_node(txn, db, entry_id, dims)?;
    let ep_dist = cosine_distance_query_vs_quant(q.vec, q.norm, &ep_node.vec);

    let mut visited: FxHashSet<u32> = FxHashSet::default();
    visited.insert(entry_id);

    let mut candidates: BinaryHeap<MinDistNode> = BinaryHeap::new();
    let mut results: BinaryHeap<DistNode> = BinaryHeap::new();
    let ep = DistNode { dist: ep_dist, id: entry_id };
    candidates.push(MinDistNode(ep));
    results.push(ep);

    while let Some(MinDistNode(curr)) = candidates.pop() {
        let worst = results.peek().map_or(f32::INFINITY, |r| r.dist);
        if curr.dist > worst && results.len() >= ef {
            break;
        }
        let node = load_node(txn, db, curr.id, dims)?;
        let Some(neighs) = node.neighbors.get(layer) else {
            continue;
        };
        for &nid in neighs {
            if !visited.insert(nid) {
                continue;
            }
            let n = load_node(txn, db, nid, dims)?;
            let d = cosine_distance_query_vs_quant(q.vec, q.norm, &n.vec);
            let worst_now = results.peek().map_or(f32::INFINITY, |r| r.dist);
            if d < worst_now || results.len() < ef {
                let dn = DistNode { dist: d, id: nid };
                candidates.push(MinDistNode(dn));
                results.push(dn);
                if results.len() > ef {
                    results.pop();
                }
            }
        }
    }

    let mut out = results.into_vec();
    out.sort_by(|a, b| a.dist.total_cmp(&b.dist).then_with(|| a.id.cmp(&b.id)));
    Ok(out)
}

/// Layer-0 HNSW beam search with **inline tombstone & predicate filtering**:
/// - Traverses tombstoned and predicate-unmatched nodes transparently as graph bridges (bounded by
///   `max_visits`) so tombstones or selective predicates (`|allowed| > 2048`) never starve `ef`!
/// - Counts ONLY live, candidate-matching nodes toward `results.len() < ef` and `worst` termination!
fn search_layer_beam_filtered<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    q: QueryVec<'_>,
    entry_id: u32,
    ef: usize,
    allowed: Option<&FxHashSet<EntityKey>>,
) -> Result<Vec<(EntityKey, u32, f32)>> {
    let dims = q.vec.len();
    let ef = ef.max(1);
    let max_visits = ef.saturating_mul(32).max(4096);

    let ep_node = load_node(txn, db, entry_id, dims)?;
    let ep_dist = cosine_distance_query_vs_quant(q.vec, q.norm, &ep_node.vec);

    let mut visited: FxHashSet<u32> = FxHashSet::default();
    visited.insert(entry_id);

    let mut candidates: BinaryHeap<MinDistNode> = BinaryHeap::new();
    let mut results: BinaryHeap<DistNode> = BinaryHeap::new();
    let mut accepted: FxHashMap<u32, (EntityKey, u32, f32)> = FxHashMap::default();

    let is_live_match = |node: &PackedHnswNode| -> Result<bool> {
        if allowed.is_some_and(|set| !set.contains(&node.entity)) {
            return Ok(false);
        }
        let Some(lm_raw) = txn.get(db, &live_map_key(node.entity, node.chunk))? else {
            return Ok(false);
        };
        let lm = LiveMapping::decode(lm_raw)?;
        Ok(lm.vec_gen == node.vec_gen)
    };

    let ep = DistNode { dist: ep_dist, id: entry_id };
    candidates.push(MinDistNode(ep));
    if is_live_match(&ep_node)? {
        results.push(ep);
        accepted.insert(entry_id, (ep_node.entity, ep_node.chunk, ep_dist));
    }

    while let Some(MinDistNode(curr)) = candidates.pop() {
        let worst = results.peek().map_or(f32::INFINITY, |r| r.dist);
        if results.len() >= ef && curr.dist > worst {
            break;
        }
        if visited.len() >= max_visits {
            break;
        }
        let node = load_node(txn, db, curr.id, dims)?;
        let Some(neighs) = node.neighbors.first() else {
            continue;
        };
        for &nid in neighs {
            if !visited.insert(nid) {
                continue;
            }
            let n = load_node(txn, db, nid, dims)?;
            let d = cosine_distance_query_vs_quant(q.vec, q.norm, &n.vec);
            let worst_now = results.peek().map_or(f32::INFINITY, |r| r.dist);
            if results.len() < ef || d < worst_now {
                let dn = DistNode { dist: d, id: nid };
                candidates.push(MinDistNode(dn));
                if is_live_match(&n)? {
                    results.push(dn);
                    accepted.insert(nid, (n.entity, n.chunk, d));
                    if results.len() > ef
                        && let Some(evicted) = results.pop()
                    {
                        accepted.remove(&evicted.id);
                    }
                }
            }
            if visited.len() >= max_visits {
                break;
            }
        }
    }

    let mut out: Vec<(EntityKey, u32, f32)> =
        results.into_vec().into_iter().filter_map(|dn| accepted.get(&dn.id).copied()).collect();
    out.sort_by(|a, b| a.2.total_cmp(&b.2).then_with(|| a.0.cmp(&b.0)).then_with(|| a.1.cmp(&b.1)));
    Ok(out)
}

/// Standard HNSW diversity heuristic (`SELECT-NEIGHBORS-HEURISTIC`) to prevent hub collapse
/// and keep high recall across clustered embeddings.
fn select_neighbors_heuristic<R: KvRead>(
    txn: &R,
    db: SubDbHandle,
    dims: usize,
    candidates: &[DistNode],
    max_conn: usize,
) -> Result<Vec<u32>> {
    let mut selected: Vec<(u32, QuantVec)> = Vec::with_capacity(max_conn);
    let mut pruned: Vec<u32> = Vec::new();
    for cand in candidates {
        if selected.len() >= max_conn {
            break;
        }
        let cnode = load_node(txn, db, cand.id, dims)?;
        let closer_to_selected = selected.iter().any(|(_, svec)| cosine_distance_quant(&cnode.vec, svec) < cand.dist);
        if !closer_to_selected {
            selected.push((cand.id, cnode.vec));
        } else {
            pruned.push(cand.id);
        }
    }
    let mut out: Vec<u32> = selected.into_iter().map(|(id, _)| id).collect();
    for pid in pruned {
        if out.len() >= max_conn {
            break;
        }
        out.push(pid);
    }
    Ok(out)
}

fn insert_hnsw_node<S: KvStore, V: Vfs>(
    txn: &mut GatedWriteTxn<'_, S, V>,
    db: SubDbHandle,
    meta: &mut HnswMeta,
    entity: EntityKey,
    chunk: u32,
    vec_gen: u64,
    f32_vec: &[f32],
) -> Result<u32> {
    let dims = meta.dims as usize;
    let new_id = meta.next_node_id;
    meta.next_node_id = meta
        .next_node_id
        .checked_add(1)
        .ok_or_else(|| ElmdbError::new(ErrorCode::StorageFull, "HNSW NodeId u32 space exhausted; compact required"))?;
    let level = deterministic_level(meta.seed, entity, chunk);
    let quant_vec = QuantVec::from_f32(f32_vec, meta.quantization);
    let mut neighbors: Vec<Vec<u32>> = vec![Vec::new(); (level as usize) + 1];

    let Some(ep_id) = meta.entry_point else {
        // First node in graph!
        let node = PackedHnswNode { entity, chunk, vec_gen, level, vec: quant_vec, neighbors };
        txn.put(db, &node_key(new_id), &node.encode())?;
        meta.entry_point = Some(new_id);
        meta.max_level = level;
        meta.live_nodes = meta.live_nodes.saturating_add(1);
        return Ok(new_id);
    };

    let q = QueryVec { vec: f32_vec, norm: l2_norm(f32_vec) };
    let ep_node = load_node(txn, db, ep_id, dims)?;
    let mut curr_id = ep_id;
    let mut curr_dist = cosine_distance_query_vs_quant(q.vec, q.norm, &ep_node.vec);

    // Phase 1: Greedy descent from `max_level` down to `level + 1`.
    let max_l = meta.max_level as usize;
    for l in ((level as usize) + 1..=max_l).rev() {
        let (nid, nd) = greedy_search_layer(txn, db, q, curr_id, curr_dist, l)?;
        curr_id = nid;
        curr_dist = nd;
    }

    // Phase 2: Beam search & bidirectional wiring from `min(level, max_level)` down to 0.
    let top_connect = (level as usize).min(max_l);
    let mut to_link: Vec<(usize, Vec<u32>)> = Vec::with_capacity(top_connect + 1);
    for l in (0..=top_connect).rev() {
        let cands = search_layer_beam(txn, db, q, curr_id, HNSW_EF_CONSTRUCTION, l)?;
        if let Some(best) = cands.first() {
            curr_id = best.id;
        }
        let max_conn = if l == 0 { HNSW_M0 } else { HNSW_M };
        let chosen = select_neighbors_heuristic(txn, db, dims, &cands, max_conn)?;
        neighbors[l] = chosen.clone();
        to_link.push((l, chosen));
    }

    // Store new node BEFORE updating back-links so neighbor pruning can load `new_id`.
    let new_node = PackedHnswNode { entity, chunk, vec_gen, level, vec: quant_vec, neighbors };
    txn.put(db, &node_key(new_id), &new_node.encode())?;

    // Wire reciprocal edges and prune any neighbor exceeding `max_conn`.
    for (l, chosen) in to_link {
        let max_conn = if l == 0 { HNSW_M0 } else { HNSW_M };
        for nb_id in chosen {
            let mut nb = load_node(txn, db, nb_id, dims)?;
            if nb.neighbors.len() <= l {
                continue;
            }
            if !nb.neighbors[l].contains(&new_id) {
                nb.neighbors[l].push(new_id);
                if nb.neighbors[l].len() > max_conn {
                    let nb_f32 = nb.vec.to_f32();
                    let nb_norm = l2_norm(&nb_f32);
                    let mut scored: Vec<DistNode> = Vec::with_capacity(nb.neighbors[l].len());
                    for &cid in &nb.neighbors[l] {
                        let cn = load_node(txn, db, cid, dims)?;
                        let d = cosine_distance_query_vs_quant(&nb_f32, nb_norm, &cn.vec);
                        scored.push(DistNode { dist: d, id: cid });
                    }
                    scored.sort_by(|a, b| a.dist.total_cmp(&b.dist).then_with(|| a.id.cmp(&b.id)));
                    nb.neighbors[l] = select_neighbors_heuristic(txn, db, dims, &scored, max_conn)?;
                }
                txn.put(db, &node_key(nb_id), &nb.encode())?;
            }
        }
    }

    if level > meta.max_level {
        meta.entry_point = Some(new_id);
        meta.max_level = level;
    }
    meta.live_nodes = meta.live_nodes.saturating_add(1);
    Ok(new_id)
}

// ---------------------------------------------------------------------------
// Bounded-Batch Drain, Flush, and Ping-Pong Compaction
// ---------------------------------------------------------------------------

/// Result of one bounded `drain_pending_batch` step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainResult {
    pub processed: usize,
    pub needs_compaction: bool,
}

/// Drains up to `batch_size` (default `DEFAULT_DRAIN_BATCH = 48`) entries from `TAG_PENDING`
/// into the HNSW graph in **one strictly bounded commit** so interactive writes are never starved.
/// Does NOT synchronously run full-index compaction inside this commit; inspect `needs_compaction`
/// or call `compact_now` / `flush` separately!
pub fn drain_pending_batch<S: KvStore, V: Vfs, C: Clock>(
    engine: &Engine<S, V, C>,
    index_id: IndexId,
    batch_size: usize,
) -> Result<DrainResult> {
    let limit = batch_size.max(1);
    // Fast RO pre-check validating IndexKind::Vector and inspecting both pending queue and compaction state.
    {
        let (ro, schema) = engine.begin_read_pinned()?;
        let def = schema
            .index_by_id(index_id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", index_id.0)))?;
        if !matches!(def.kind, IndexKind::Vector { .. }) {
            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "index is not a Vector index"));
        }
        let Some(db) = engine.gate().open_subdb_ro(&ro, &def.subdb_slot().name(), DbFlags::DEFAULT)? else {
            return Ok(DrainResult { processed: 0, needs_compaction: false });
        };
        let meta = load_meta_or_init(&ro, db, def)?;
        let mut cur = ro.cursor(db)?;
        let has_pending = cur.seek_ge(&[TAG_PENDING])?.is_some_and(|(k, _)| k.first() == Some(&TAG_PENDING));
        if !has_pending {
            return Ok(DrainResult { processed: 0, needs_compaction: meta.should_compact() });
        }
    }

    let (result, _) = engine.run_maintenance_write(|txn, schema, dbs| {
        let def = schema
            .index_by_id(index_id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", index_id.0)))?;
        let db = txn.open_subdb(&def.subdb_slot().name(), DbFlags::DEFAULT)?;
        let mut meta = load_meta_or_init(txn, db, def)?;

        let mut items: Vec<(EntityKey, u32, PendingOp)> = Vec::with_capacity(limit.min(128));
        {
            let mut cur = txn.cursor(db)?;
            let mut step = cur.seek_ge(&[TAG_PENDING])?;
            while let Some((k, v)) = step {
                if k.first() != Some(&TAG_PENDING) {
                    break;
                }
                if k.len() != 13 {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt vec_pending key"));
                }
                let ek = EntityKey::from_be_bytes(k[1..9].try_into().expect("8"));
                let chunk = u32::from_be_bytes(k[9..13].try_into().expect("4"));
                items.push((ek, chunk, PendingOp::decode(v)?));
                if items.len() >= limit {
                    break;
                }
                step = cur.next()?;
            }
        }
        if items.is_empty() {
            meta.pending_count = 0;
            txn.put(db, &[TAG_META], &meta.encode())?;
            return Ok(DrainResult { processed: 0, needs_compaction: meta.should_compact() });
        }

        let dims = meta.dims as usize;
        for &(ek, chunk, op) in &items {
            let pk = pending_key(ek, chunk);
            if txn.del(db, &pk)? {
                meta.pending_count = meta.pending_count.saturating_sub(1);
            }
            if let PendingOp::Upsert(gen_id) = op {
                let lmk = live_map_key(ek, chunk);
                let Some(lm_raw) = txn.get(db, &lmk)? else {
                    continue;
                };
                let lm = LiveMapping::decode(lm_raw)?;
                if lm.vec_gen != gen_id || lm.node_id.is_some() {
                    continue;
                }
                let Some(emb_raw) = txn.get(dbs.embeddings, &primary_emb_key(index_id, ek, chunk))? else {
                    continue;
                };
                let (emb_gen, vec) = decode_primary_emb(emb_raw, dims)?;
                if emb_gen != gen_id {
                    continue;
                }
                let node_id = insert_hnsw_node(txn, db, &mut meta, ek, chunk, gen_id, &vec)?;
                txn.put(db, &lmk, &LiveMapping { node_id: Some(node_id), vec_gen: gen_id }.encode())?;
            }
        }

        txn.put(db, &[TAG_META], &meta.encode())?;
        Ok(DrainResult { processed: items.len(), needs_compaction: meta.should_compact() })
    })?;
    Ok(result)
}

/// Drains ALL pending vectors in bounded `DEFAULT_DRAIN_BATCH` commits and compacts via ping-pong
/// slot swap if the tombstone ratio exceeds 25% (> 1,024 total nodes) — including delete-only workloads!
pub fn flush<S: KvStore, V: Vfs, C: Clock>(engine: &Engine<S, V, C>, index_id: IndexId) -> Result<usize> {
    let mut total = 0usize;
    let should_compact = loop {
        let res = drain_pending_batch(engine, index_id, DEFAULT_DRAIN_BATCH)?;
        total += res.processed;
        if res.processed == 0 {
            break res.needs_compaction;
        }
    };
    if should_compact {
        let _ = compact_now(engine, index_id)?;
    }
    Ok(total)
}

/// Ping-pong compacts a vector index into a fresh staging `SubDbSlot` drawn from `SubDbPool`,
/// rebuilding only live vectors (dropping all tombstoned `dead_nodes` and reclaiming `NodeId`s!),
/// atomically swapping the active slot in `meta`, and truncating the retired slot in O(1)!
pub fn compact_now<S: KvStore, V: Vfs, C: Clock>(engine: &Engine<S, V, C>, index_id: IndexId) -> Result<VectorStats> {
    let (stats, _) = engine.swap_index_slot(index_id, |txn, schema, dbs, old_db, staging_db| {
        let def = schema
            .index_by_id(index_id)
            .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", index_id.0)))?;
        let old_meta = load_meta_or_init(txn, old_db, def)?;
        let mut new_meta = HnswMeta::new(old_meta.dims, old_meta.quantization, old_meta.seed);
        new_meta.next_vec_gen = old_meta.next_vec_gen;

        // Collect all live `(entity, chunk, vec_gen)` entries from `old_db`'s `TAG_LIVE_MAP`.
        let mut live_entries: Vec<(EntityKey, u32, u64)> = Vec::new();
        {
            let mut cur = txn.cursor(old_db)?;
            let mut step = cur.seek_ge(&[TAG_LIVE_MAP])?;
            while let Some((k, v)) = step {
                if k.first() != Some(&TAG_LIVE_MAP) {
                    break;
                }
                if k.len() != 13 {
                    return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt live_map key during compact"));
                }
                let ek = EntityKey::from_be_bytes(k[1..9].try_into().expect("8"));
                let chunk = u32::from_be_bytes(k[9..13].try_into().expect("4"));
                let lm = LiveMapping::decode(v)?;
                live_entries.push((ek, chunk, lm.vec_gen));
                step = cur.next()?;
            }
        }

        let dims = new_meta.dims as usize;
        for (ek, chunk, vec_gen) in live_entries {
            let Some(emb_raw) = txn.get(dbs.embeddings, &primary_emb_key(index_id, ek, chunk))? else {
                continue;
            };
            let (emb_gen, vec) = decode_primary_emb(emb_raw, dims)?;
            if emb_gen != vec_gen {
                continue;
            }
            let node_id = insert_hnsw_node(txn, staging_db, &mut new_meta, ek, chunk, vec_gen, &vec)?;
            txn.put(staging_db, &live_map_key(ek, chunk), &LiveMapping { node_id: Some(node_id), vec_gen }.encode())?;
        }
        txn.put(staging_db, &[TAG_META], &new_meta.encode())?;

        Ok(VectorStats {
            dims: new_meta.dims,
            quantization: new_meta.quantization,
            live_nodes: new_meta.live_nodes,
            dead_nodes: new_meta.dead_nodes,
            pending_count: new_meta.pending_count,
            needs_compaction: new_meta.should_compact(),
        })
    })?;
    Ok(stats)
}

/// Returns current live/dead/pending statistics for a vector index.
pub fn stats<S: KvStore, V: Vfs, C: Clock>(engine: &Engine<S, V, C>, index_id: IndexId) -> Result<VectorStats> {
    let (txn, schema) = engine.begin_read_pinned()?;
    let def = schema
        .index_by_id(index_id)
        .ok_or_else(|| ElmdbError::new(ErrorCode::UnknownIndex, format!("unknown index id {}", index_id.0)))?;
    let Some(db) = engine.gate().open_subdb_ro(&txn, &def.subdb_slot().name(), DbFlags::DEFAULT)? else {
        let IndexKind::Vector { dims, quantization, .. } = &def.kind else {
            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "index is not a Vector index"));
        };
        return Ok(VectorStats {
            dims: *dims,
            quantization: Quantization::parse(quantization)?,
            live_nodes: 0,
            dead_nodes: 0,
            pending_count: 0,
            needs_compaction: false,
        });
    };
    let meta = load_meta_or_init(&txn, db, def)?;
    Ok(VectorStats {
        dims: meta.dims,
        quantization: meta.quantization,
        live_nodes: meta.live_nodes,
        dead_nodes: meta.dead_nodes,
        pending_count: meta.pending_count,
        needs_compaction: meta.should_compact(),
    })
}

// ---------------------------------------------------------------------------
// Generation-Safe Hybrid Vector Query (`search_near` + `search_like`)
// ---------------------------------------------------------------------------

/// Options controlling `search_near`.
#[derive(Debug, Clone, Copy)]
pub struct SearchNearOpts<'a> {
    pub top_k: usize,
    pub candidates: Option<&'a FxHashSet<EntityKey>>,
    pub best_chunk_per_entity: bool,
}

impl<'a> SearchNearOpts<'a> {
    pub fn new(top_k: usize) -> Self {
        Self { top_k, candidates: None, best_chunk_per_entity: true }
    }
}

/// Executes top-`k` cosine similarity search over a vector index with:
/// 1. Exact F32 scan fallback when `candidates` has `<= 2048` entities
/// 2. HNSW beam search (`ef = max(4k, 64)`) + **pre-rerank generation & tombstone filter** via `TAG_LIVE_MAP`
/// 3. Exact F32 scan over `TAG_PENDING` for 100% read-your-writes freshness before drain
/// 4. Exact F32 cosine rerank of top `max(4k, 64)` candidates directly from `dbs.embeddings`
pub fn search_near<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    def: &IndexDef,
    db: SubDbHandle,
    query: &[f32],
    opts: SearchNearOpts<'_>,
) -> Result<Vec<VectorHit>> {
    let top_k = opts.top_k;
    let candidates = opts.candidates;
    let best_chunk_per_entity = opts.best_chunk_per_entity;
    if top_k == 0 {
        return Ok(Vec::new());
    }
    let meta = load_meta_or_init(txn, db, def)?;
    validate_vector(query, meta.dims)?;
    let dims = meta.dims as usize;
    let q_norm = l2_norm(query);
    if q_norm <= 0.0 {
        return Ok(Vec::new());
    }
    let q = QueryVec { vec: query, norm: q_norm };

    // Fast path: tight predicate filter (`|allowed| <= 2048`) -> exact F32 scan over `dbs.embeddings`!
    if let Some(allowed) = candidates
        && allowed.len() <= EXACT_SCAN_THRESHOLD
    {
        return exact_scan_allowed(txn, dbs, db, def.id, query, allowed, opts);
    }

    let rerank_k = (top_k.saturating_mul(4)).max(64);
    let mut pool: FxHashMap<(EntityKey, u32), f32> = FxHashMap::default();

    // 1. HNSW search with inline tombstone & predicate filtering inside layer-0 beam!
    if let Some(ep_id) = meta.entry_point {
        let ep_node = load_node(txn, db, ep_id, dims)?;
        let mut curr_id = ep_id;
        let mut curr_dist = cosine_distance_query_vs_quant(q.vec, q.norm, &ep_node.vec);
        for l in (1..=meta.max_level as usize).rev() {
            let (nid, nd) = greedy_search_layer(txn, db, q, curr_id, curr_dist, l)?;
            curr_id = nid;
            curr_dist = nd;
        }
        let beam = search_layer_beam_filtered(txn, db, q, curr_id, rerank_k, candidates)?;
        for (entity, chunk, dist) in beam {
            pool.entry((entity, chunk))
                .and_modify(|d| {
                    if dist < *d {
                        *d = dist;
                    }
                })
                .or_insert(dist);
        }
    }

    // 2. Exact F32 scan of `TAG_PENDING` upserts (exact read-your-writes before background drain!).
    let mut pending_exact: Vec<VectorHit> = Vec::new();
    {
        let mut cur = txn.cursor(db)?;
        let mut step = cur.seek_ge(&[TAG_PENDING])?;
        while let Some((k, v)) = step {
            if k.first() != Some(&TAG_PENDING) {
                break;
            }
            if k.len() != 13 {
                return Err(ElmdbError::new(ErrorCode::Corrupted, "corrupt pending key"));
            }
            if let PendingOp::Upsert(gen_id) = PendingOp::decode(v)? {
                let ek = EntityKey::from_be_bytes(k[1..9].try_into().expect("8"));
                let chunk = u32::from_be_bytes(k[9..13].try_into().expect("4"));
                if candidates.is_none_or(|set| set.contains(&ek))
                    && let Some(lm_raw) = txn.get(db, &live_map_key(ek, chunk))?
                {
                    let lm = LiveMapping::decode(lm_raw)?;
                    if lm.vec_gen == gen_id
                        && let Some(emb_raw) = txn.get(dbs.embeddings, &primary_emb_key(def.id, ek, chunk))?
                    {
                        let (emb_gen, vec) = decode_primary_emb(emb_raw, dims)?;
                        if emb_gen == gen_id {
                            let dist = cosine_distance_f32(query, &vec);
                            pending_exact.push(VectorHit { key: ek, chunk, score: 1.0 - dist });
                            // Remove from HNSW pool so we don't rerank twice.
                            pool.remove(&(ek, chunk));
                        }
                    }
                }
            }
            step = cur.next()?;
        }
    }

    // 3. Exact F32 rerank of top `rerank_k` HNSW candidates directly from `dbs.embeddings`!
    // When `best_chunk_per_entity == true`, collapse to best chunk per entity BEFORE truncating to `rerank_k`
    // so a single multi-chunk document can never monopolize the rerank budget!
    let mut hnsw_cands: Vec<((EntityKey, u32), f32)> = if best_chunk_per_entity {
        let mut by_entity: FxHashMap<EntityKey, (u32, f32)> = FxHashMap::default();
        for ((ek, chunk), dist) in pool {
            by_entity
                .entry(ek)
                .and_modify(|(bc, bd)| {
                    if dist < *bd || (dist.to_bits() == bd.to_bits() && chunk < *bc) {
                        *bc = chunk;
                        *bd = dist;
                    }
                })
                .or_insert((chunk, dist));
        }
        by_entity.into_iter().map(|(ek, (chunk, dist))| ((ek, chunk), dist)).collect()
    } else {
        pool.into_iter().collect()
    };
    hnsw_cands.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    hnsw_cands.truncate(rerank_k);

    let mut reranked: Vec<VectorHit> = Vec::with_capacity(hnsw_cands.len() + pending_exact.len());
    for ((ek, chunk), _) in hnsw_cands {
        if let Some(emb_raw) = txn.get(dbs.embeddings, &primary_emb_key(def.id, ek, chunk))? {
            let (_, vec) = decode_primary_emb(emb_raw, dims)?;
            let sim = 1.0 - cosine_distance_f32(query, &vec);
            reranked.push(VectorHit { key: ek, chunk, score: sim });
        }
    }
    reranked.extend(pending_exact);

    finalize_hits(reranked, top_k, best_chunk_per_entity)
}

/// Finds entities most similar to an existing entity's primary embedding (chunk 0, excluding self).
pub fn search_like<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    def: &IndexDef,
    db: SubDbHandle,
    seed_entity: EntityKey,
    top_k: usize,
    candidates: Option<&FxHashSet<EntityKey>>,
) -> Result<Vec<VectorHit>> {
    if top_k == 0 {
        return Ok(Vec::new());
    }
    let meta = load_meta_or_init(txn, db, def)?;
    let Some(lm_raw) = txn.get(db, &live_map_key(seed_entity, 0))? else {
        return Ok(Vec::new());
    };
    let lm = LiveMapping::decode(lm_raw)?;
    let Some(emb_raw) = txn.get(dbs.embeddings, &primary_emb_key(def.id, seed_entity, 0))? else {
        return Ok(Vec::new());
    };
    let (emb_gen, seed_vec) = decode_primary_emb(emb_raw, meta.dims as usize)?;
    if emb_gen != lm.vec_gen {
        return Ok(Vec::new());
    }
    let mut hits = search_near(
        txn,
        dbs,
        def,
        db,
        &seed_vec,
        SearchNearOpts { top_k: top_k.saturating_add(1), candidates, best_chunk_per_entity: true },
    )?;
    hits.retain(|h| h.key != seed_entity);
    hits.truncate(top_k);
    Ok(hits)
}

fn exact_scan_allowed<R: KvRead>(
    txn: &R,
    dbs: &SystemDbs,
    db: SubDbHandle,
    index_id: IndexId,
    query: &[f32],
    allowed: &FxHashSet<EntityKey>,
    opts: SearchNearOpts<'_>,
) -> Result<Vec<VectorHit>> {
    let dims = query.len();
    let mut hits = Vec::new();
    let mut cur = txn.cursor(dbs.embeddings)?;
    for &ek in allowed {
        let mut prefix = [0u8; 12];
        prefix[0..4].copy_from_slice(&index_id.0.to_be_bytes());
        prefix[4..12].copy_from_slice(&ek.to_be_bytes());
        let mut step = cur.seek_ge(&prefix)?;
        while let Some((k, v)) = step {
            if k.len() != 16 || !k.starts_with(&prefix) {
                break;
            }
            let chunk = u32::from_be_bytes(k[12..16].try_into().expect("4"));
            if let Some(lm_raw) = txn.get(db, &live_map_key(ek, chunk))? {
                let lm = LiveMapping::decode(lm_raw)?;
                let (emb_gen, vec) = decode_primary_emb(v, dims)?;
                if emb_gen == lm.vec_gen {
                    let sim = 1.0 - cosine_distance_f32(query, &vec);
                    hits.push(VectorHit { key: ek, chunk, score: sim });
                }
            }
            step = cur.next()?;
        }
    }
    finalize_hits(hits, opts.top_k, opts.best_chunk_per_entity)
}

fn finalize_hits(mut hits: Vec<VectorHit>, top_k: usize, best_chunk_per_entity: bool) -> Result<Vec<VectorHit>> {
    hits.sort_by(|a, b| {
        b.score.total_cmp(&a.score).then_with(|| a.key.cmp(&b.key)).then_with(|| a.chunk.cmp(&b.chunk))
    });
    if best_chunk_per_entity {
        let mut seen: FxHashSet<EntityKey> = FxHashSet::default();
        hits.retain(|h| seen.insert(h.key));
    }
    hits.truncate(top_k);
    Ok(hits)
}

// ---------------------------------------------------------------------------
// ExternalProjection implementation (plugs directly into `ProjectionRegistry`)
// ---------------------------------------------------------------------------

/// `ProjectionRegistry` plug-in for `IndexKind::Vector` indexes.
#[derive(Debug, Default)]
pub struct VectorProjection;

impl<S: KvStore, V: Vfs> ExternalProjection<S, V> for VectorProjection {
    fn handles(&self, kind: &IndexKind) -> bool {
        matches!(kind, IndexKind::Vector { .. })
    }

    fn apply_entity_delta(
        &self,
        txn: &mut GatedWriteTxn<'_, S, V>,
        _schema: &CompiledSchema,
        dbs: &SystemDbs,
        def: &IndexDef,
        db: SubDbHandle,
        delta: &EntityDelta,
    ) -> Result<()> {
        let was_live = delta.before.as_ref().is_some_and(|r| !r.header.is_trashed());
        match &delta.after {
            None => {
                // Entity permanently destroyed -> delete both primary embeddings and derived HNSW entries.
                remove_all_chunks_for_entity(txn, dbs, db, def, delta.key, true)?;
            }
            Some(row) if row.header.is_trashed() => {
                // Entity soft-trashed -> tombstone HNSW/live_map while PRESERVING `dbs.embeddings` for Restore!
                remove_all_chunks_for_entity(txn, dbs, db, def, delta.key, false)?;
            }
            Some(_) if !was_live && delta.before.is_some() => {
                // Entity restored from trash -> re-enqueue preserved primary embeddings!
                restore_chunks_for_entity(txn, dbs, db, def, delta.key)?;
            }
            Some(_) => {}
        }
        Ok(())
    }

    fn apply_edge_delta(
        &self,
        _txn: &mut GatedWriteTxn<'_, S, V>,
        _schema: &CompiledSchema,
        _dbs: &SystemDbs,
        _def: &IndexDef,
        _db: SubDbHandle,
        _delta: &EdgeDelta,
    ) -> Result<()> {
        Ok(())
    }

    fn on_index_added(
        &self,
        txn: &mut GatedWriteTxn<'_, S, V>,
        _schema: &CompiledSchema,
        _dbs: &SystemDbs,
        def: &IndexDef,
        db: SubDbHandle,
    ) -> Result<BuildState> {
        let meta = load_meta_or_init(txn, db, def)?;
        txn.put(db, &[TAG_META], &meta.encode())?;
        Ok(BuildState::Ready)
    }

    fn rebuild_index(
        &self,
        txn: &mut GatedWriteTxn<'_, S, V>,
        _schema: &CompiledSchema,
        dbs: &SystemDbs,
        def: &IndexDef,
        db: SubDbHandle,
    ) -> Result<()> {
        let IndexKind::Vector { dims, quantization, .. } = &def.kind else {
            return Err(ElmdbError::new(ErrorCode::InvalidArgument, "expected Vector index"));
        };
        let q = Quantization::parse(quantization)?;
        let mut meta = HnswMeta::new(*dims, q, 0xE1ED_B200_0000_0000 ^ (def.id.0 as u64));

        // Scan all primary embeddings belonging to `def.id`: index live entities, keep trashed, purge destroyed orphans.
        let prefix = def.id.0.to_be_bytes();
        let mut entries: Vec<(EntityKey, u32, u64, Vec<f32>)> = Vec::new();
        let mut orphans: Vec<Vec<u8>> = Vec::new();
        let mut max_gen = 0u64;
        {
            let mut cur = txn.cursor(dbs.embeddings)?;
            let mut step = cur.seek_ge(&prefix)?;
            while let Some((k, v)) = step {
                if k.len() != 16 || k[0..4] != prefix {
                    break;
                }
                let ek = EntityKey::from_be_bytes(k[4..12].try_into().expect("8"));
                let chunk = u32::from_be_bytes(k[12..16].try_into().expect("4"));
                let (gen_id, vec) = decode_primary_emb(v, *dims as usize)?;
                max_gen = max_gen.max(gen_id);
                match read_packed_row(txn, dbs, ek)? {
                    Some(row) if row.header.type_id == def.type_id => {
                        if !row.header.is_trashed() {
                            entries.push((ek, chunk, gen_id, vec));
                        }
                    }
                    _ => orphans.push(k.to_vec()),
                }
                step = cur.next()?;
            }
        }
        for ok in orphans {
            txn.del(dbs.embeddings, &ok)?;
        }
        meta.next_vec_gen = max_gen.saturating_add(1);

        txn.truncate(db)?;
        for (ek, chunk, gen_id, vec) in entries {
            let node_id = insert_hnsw_node(txn, db, &mut meta, ek, chunk, gen_id, &vec)?;
            txn.put(db, &live_map_key(ek, chunk), &LiveMapping { node_id: Some(node_id), vec_gen: gen_id }.encode())?;
        }
        txn.put(db, &[TAG_META], &meta.encode())?;
        Ok(())
    }

    fn check_index(
        &self,
        txn: &S::RoTxn<'_>,
        _schema: &CompiledSchema,
        dbs: &SystemDbs,
        def: &IndexDef,
        db: Option<SubDbHandle>,
    ) -> Result<Vec<IndexMismatch>> {
        let mut mismatches = Vec::new();
        let Some(db) = db else {
            mismatches.push(IndexMismatch {
                index_id: def.id,
                index_name: def.name.clone(),
                detail: "vector sub-DB missing".into(),
            });
            return Ok(mismatches);
        };
        let meta = load_meta_or_init(txn, db, def)?;
        let dims = meta.dims as usize;

        // 1. Collect all primary embeddings for LIVE entities of `def.type_id` under `def.id`.
        let prefix = def.id.0.to_be_bytes();
        let mut primary: FxHashMap<(EntityKey, u32), u64> = FxHashMap::default();
        {
            let mut cur = txn.cursor(dbs.embeddings)?;
            let mut step = cur.seek_ge(&prefix)?;
            while let Some((k, v)) = step {
                if k.len() != 16 || k[0..4] != prefix {
                    break;
                }
                let ek = EntityKey::from_be_bytes(k[4..12].try_into().expect("8"));
                let chunk = u32::from_be_bytes(k[12..16].try_into().expect("4"));
                let (gen_id, _) = decode_primary_emb(v, dims)?;
                if let Some(row) = read_packed_row(txn, dbs, ek)?
                    && !row.header.is_trashed()
                    && row.header.type_id == def.type_id
                {
                    primary.insert((ek, chunk), gen_id);
                }
                step = cur.next()?;
            }
        }

        // 2. Verify `TAG_LIVE_MAP` matches `primary` 1-to-1 and every drained `node_id` resolves cleanly!
        let mut live_map_count = 0usize;
        let mut drained_count = 0u64;
        {
            let mut cur = txn.cursor(db)?;
            let mut step = cur.seek_ge(&[TAG_LIVE_MAP])?;
            while let Some((k, v)) = step {
                if k.first() != Some(&TAG_LIVE_MAP) {
                    break;
                }
                if k.len() != 13 {
                    mismatches.push(IndexMismatch {
                        index_id: def.id,
                        index_name: def.name.clone(),
                        detail: "corrupt live_map key".into(),
                    });
                    return Ok(mismatches);
                }
                let ek = EntityKey::from_be_bytes(k[1..9].try_into().expect("8"));
                let chunk = u32::from_be_bytes(k[9..13].try_into().expect("4"));
                let lm = LiveMapping::decode(v)?;
                live_map_count += 1;
                if primary.get(&(ek, chunk)) != Some(&lm.vec_gen) {
                    mismatches.push(IndexMismatch {
                        index_id: def.id,
                        index_name: def.name.clone(),
                        detail: format!("live_map generation mismatch for ({ek:?}, {chunk})"),
                    });
                }
                if let Some(nid) = lm.node_id {
                    drained_count += 1;
                    let node = load_node(txn, db, nid, dims)?;
                    if node.entity != ek || node.chunk != chunk || node.vec_gen != lm.vec_gen {
                        mismatches.push(IndexMismatch {
                            index_id: def.id,
                            index_name: def.name.clone(),
                            detail: format!("HNSW node {nid} metadata mismatch for ({ek:?}, {chunk})"),
                        });
                    }
                } else if txn.get(db, &pending_key(ek, chunk))?.is_none() {
                    mismatches.push(IndexMismatch {
                        index_id: def.id,
                        index_name: def.name.clone(),
                        detail: format!("un-drained vector ({ek:?}, {chunk}) missing from vec_pending"),
                    });
                }
                if mismatches.len() >= 16 {
                    return Ok(mismatches);
                }
                step = cur.next()?;
            }
        }

        if live_map_count != primary.len() {
            mismatches.push(IndexMismatch {
                index_id: def.id,
                index_name: def.name.clone(),
                detail: format!("live_map count {live_map_count} != primary embeddings count {}", primary.len()),
            });
        }
        if drained_count != meta.live_nodes {
            mismatches.push(IndexMismatch {
                index_id: def.id,
                index_name: def.name.clone(),
                detail: format!("meta.live_nodes {} != actual drained count {drained_count}", meta.live_nodes),
            });
        }
        Ok(mismatches)
    }
}
