//! elmdb: an embedded graph database written in Rust on LMDB. The design is in the repository's `docs/`.

mod database;
mod error;
pub mod format;
pub mod schema;
mod store;

pub use database::{Database, OpenOptions, OpenReport};
pub use error::{Error, ErrorCode, Result};
pub use store::sync::Durable;
