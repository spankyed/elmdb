#![forbid(unsafe_code)]

//! Deterministic simulation harness (`SimKvStore`, `SimVfs`, `SimClock`, `SimRng`)
//! for TigerStyle crash, torn-page, fault-injection, and MVCC verification of elmdb v2.

pub mod clock;
pub mod kv;
pub mod rng;
pub mod vfs;

pub use clock::SimClock;
pub use kv::{SimKvConfig, SimKvStore};
pub use rng::SimRng;
pub use vfs::{SimVfs, SimVfsFaults};
