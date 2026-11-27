//! Owned safe Rust wrapper over `lmdb-master-sys` for elmdb v2.
//!
//! All `unsafe` FFI calls are isolated inside this crate with explicit `// SAFETY:`
//! invariants. Key guarantees:
//! - Zero `transmute` to `'static`: `LmdbRwTxn` owns an explicit LIFO `Vec<NonNull<MDB_txn>>`
//!   native nested savepoint stack.
//! - Zero `mdb_dbi_close()` calls: sub-DB handles live for the lifetime of `LmdbStore` and
//!   are emptied in O(1) via `mdb_drop(txn, dbi, 0)`.
//! - Zero-copy `&'txn [u8]` reads lifetime-bound to `LmdbRoTxn` / `LmdbRwTxn`.
//! - 16 GiB sparse virtual address reservation on 64-bit Unix (64 MiB on Windows).

mod cursor;
mod env;
mod error;
pub mod meta;
mod txn;

pub use cursor::LmdbCursor;
pub use env::{DEFAULT_MAP_SIZE, DEFAULT_MAX_DBS, DEFAULT_MAX_READERS, LmdbConfig, LmdbStore, platform_boot_id};
pub use meta::{MetaPageInfo, read_active_meta_page, verify_data_file_headers};
pub use txn::{LmdbRoTxn, LmdbRwTxn};
