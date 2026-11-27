#![forbid(unsafe_code)]

//! Single-pass canonical binary wire protocol, canonical typed values, unified numeric
//! index key encoding, per-response string dictionary, and single authoritative `ErrorCode`
//! taxonomy shared across `elmdb-engine`, `elmdb-node`, and `@elmdb/ears`.

pub mod error;
pub mod value;
pub mod wire;

pub use error::{ElmdbError, ErrorCode, Result};
pub use value::{
    CanonicalValue, MAX_VALUE_DEPTH, NUMERIC_INDEX_KEY_LEN, ValueRef, ValueTag, compare_numeric, decode_value,
    decode_value_ref, encode_numeric_index_key, encode_value,
};
pub use wire::{
    CommitReturnFlags, DecodedBatch, DecodedResultFrame, EntityTarget, ResultDecoder, ResultEncoder, WireOp,
    decode_batch, encode_batch,
};
