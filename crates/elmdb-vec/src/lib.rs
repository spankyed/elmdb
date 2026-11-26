#![forbid(unsafe_code)]

//! Clean-room elmdb v2 LMDB-backed HNSW vector engine (`#![forbid(unsafe_code)]`):
//! - Per-vector SQ8 default (1536-dim `< 2 KiB`, zero LMDB overflow pages), F16/F32 opt-in
//! - Exact F32 rerank of top `max(4k, 64)` from primary `embeddings`
//! - Persisted `vec_pending` queue drained in bounded 32–64 batches (zero commit-time HNSW stall!)
//! - Pre-rerank `(entity, vec_gen)` / tombstone filtering (zero stale hits!)
//! - Exact F32 scan fallback when predicate filter `|allowed| <= 2048`
//! - Lazy 25% ping-pong compaction into pooled `SubDbSlot` via `Engine::swap_index_slot` (zero DBI leaks!)

pub mod hnsw;
pub mod quant;

pub use hnsw::{
    BackpressureHint, COMPACT_MIN_NODES, DEFAULT_DRAIN_BATCH, EXACT_SCAN_THRESHOLD, HNSW_EF_CONSTRUCTION, HNSW_M,
    HNSW_M0, HNSW_MAX_LEVEL, HnswMeta, PENDING_BACKPRESSURE_THRESHOLD, SearchNearOpts, VectorHit, VectorProjection,
    VectorStats, compact_now, delete_entity_embeddings, deterministic_level, drain_pending_batch, flush, search_like,
    search_near, set_entity_embeddings, stats,
};
pub use quant::{QuantVec, Quantization, cosine_distance_f32, cosine_similarity_f32, f16_to_f32, f32_to_f16};
