//! elmdb: an embedded graph database written in Rust on LMDB. The design is in the repository's `docs/`.

mod database;
mod error;

pub use database::{Database, OpenOptions};
pub use error::{Error, ErrorCode, Result};
