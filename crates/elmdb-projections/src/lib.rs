#![forbid(unsafe_code)]

//! Clean-room elmdb v2 unified incremental projection engine:
//! - Single `ProjectionRegistry` implementing `elmdb_engine::ProjectionSink`
//! - 1 pooled LMDB sub-DB per index (`SubDbSlot`) with 1-byte table tags (zero `MDB_DBS_FULL` leaks!)
//! - Watermark-bounded chunked builds (`key <= built_up_to || key > high_water`, zero double-work)
//! - Exact `check()` differential verifier (`derived == rebuild(primary)`)
//! - Core projections: Value + Unique, ICU Collation Order, BM25F Full-Text with Block-Max WAND,
//!   and Rarest-First Trigram (`RoaringBitmap` + `trig_df`).

pub mod order;
pub mod registry;
pub mod state;
pub mod text;
pub mod trigram;
pub mod value;

pub use order::{CachedCollator, CollatorCache, CollatorStamp, SortKey};
pub use registry::{BuildPolicy, ExternalProjection, ProjectionRegistry};
pub use state::{BuildState, CheckReport, DEFAULT_BUILD_CHUNK, IndexMismatch, IndexStatus, ProjectionStats};
pub use text::{BmwSearchStats, TextHit, TextStats, map_utf8_spans_to_utf16};
pub use value::{INLINE_POSTING_LIMIT, MAX_INDEXED_STRING, ValueKeyClass};
