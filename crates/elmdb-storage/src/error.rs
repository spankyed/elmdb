use std::io;

use thiserror::Error;

/// Storage-layer error taxonomy shared across LMDB and simulation backends.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("key not found")]
    NotFound,

    #[error("key already exists")]
    KeyExist,

    #[error("storage map is full (map_size={map_size})")]
    MapFull { map_size: usize },

    #[error("storage map was resized externally; transaction must retry after adopting new map size")]
    MapResized,

    #[error("database directory '{0}' is already open in this process")]
    DatabaseLocked(String),

    #[error("storage was opened read-only")]
    ReadOnly,

    #[error("maximum number of sub-databases reached (max_dbs={max_dbs})")]
    DbsFull { max_dbs: u32 },

    #[error("maximum number of concurrent readers reached")]
    ReadersFull,

    #[error("write transaction has too many dirty pages")]
    TxnFull,

    #[error("key or value exceeds storage size limit (len={len})")]
    BadValSize { len: usize },

    #[error("unknown or closed sub-database handle ({handle})")]
    InvalidSubDb { handle: u32 },

    #[error("savepoint depth mismatch (expected={expected}, actual={actual})")]
    SavepointMismatch { expected: u32, actual: u32 },

    #[error("invalid storage state: {0}")]
    InvalidState(String),

    #[error("storage corruption detected: {0}")]
    Corrupted(String),

    #[error("durability gate is permanently poisoned after prior sync failure: {0}")]
    Poisoned(String),

    #[error("durability sync failed: {0}")]
    DurabilityFailed(String),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

impl StorageError {
    /// Returns true if this error represents an unrecoverable corruption or poison state.
    pub fn is_fatal(&self) -> bool {
        matches!(self, Self::Corrupted(_) | Self::Poisoned(_))
    }
}
