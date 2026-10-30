//! The synchronization primitives a concurrency model replaces: `std`'s, and Loom's when built with `--cfg loom`
//! (`docs/architecture/testing.md`, Concurrency models). Code that uses these is what a model explores; everything
//! else keeps `std`'s, so a build with `--cfg loom` runs only the models.

#[cfg(not(loom))]
pub(crate) use std::sync::{Condvar, Mutex};

#[cfg(loom)]
pub(crate) use loom::sync::{Condvar, Mutex};
