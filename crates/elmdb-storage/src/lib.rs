#![forbid(unsafe_code)]

//! Core storage abstractions, durability choke point, bounded DBI slot pool,
//! and strict 4-domain lock hierarchy for elmdb v2.
//!
//! This crate has zero LMDB dependency so production code and deterministic
//! simulation (`elmdb-sim`) compile against the exact same traits.

pub mod clock;
pub mod durability;
pub mod error;
pub mod kv;
pub mod locks;
pub mod slot;
pub mod vfs;

pub use clock::{Clock, SystemClock};
pub use durability::{
    DurabilityGate, DurabilityMode, GatedWriteTxn, RecoveryReport, RecoveryStatus, SYNC_FILE_BYTES, SYNC_SIDECAR_NAME,
    SYNC_SLOT_BYTES, SyncSlot,
};
pub use error::StorageError;
pub use kv::{CommitReceipt, DbFlags, KvCursor, KvEntry, KvRead, KvStore, KvWrite, SubDbHandle, with_savepoint};
pub use locks::{
    DbiPublishGate, LockDomain, MapGate, MetaLock, MetaRwLock, OrderedArcReadGuard, OrderedReadGuard, SchemaEpochLock,
    WriterLock,
};
pub use slot::{MAX_POOLED_SLOTS, SubDbPool, SubDbSlot};
pub use vfs::{BootId, OsVfs, Vfs, VfsLockGuard};
