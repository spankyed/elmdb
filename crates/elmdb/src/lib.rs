//! elmdb: an embedded graph database written in Rust on LMDB. The design is in the repository's `docs/`.

mod database;
pub mod engine;
mod error;
pub mod format;
pub mod schema;
mod store;

pub use database::{Database, OpenOptions, OpenReport};
pub use engine::Unit;
pub use engine::batch::{Batch, BatchResult, Op, OpResult};
pub use engine::changes::ChangeSet;
pub use error::{Error, ErrorCode, Result};
pub use store::sync::Durable;
