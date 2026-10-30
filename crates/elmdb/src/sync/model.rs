//! What a model runs on, Loom's threads or Shuttle's, and how it explores them: Loom every interleaving within
//! `LOOM_MAX_PREEMPTIONS`, Shuttle `ELMDB_SHUTTLE_ITERATIONS` random schedules (100,000 by default), printing a failing
//! one's schedule for `shuttle::replay`

#[cfg(loom)]
pub(crate) use loom::sync::Arc;
#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicBool, Ordering};
#[cfg(loom)]
pub(crate) use loom::thread;

#[cfg(shuttle)]
pub(crate) use shuttle::sync::Arc;
#[cfg(shuttle)]
pub(crate) use shuttle::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(shuttle)]
pub(crate) use shuttle::thread;

#[cfg(loom)]
pub(crate) fn explore(model: impl Fn() + Send + Sync + 'static) {
    loom::model(model);
}

#[cfg(shuttle)]
pub(crate) fn explore(model: impl Fn() + Send + Sync + 'static) {
    let iterations = std::env::var("ELMDB_SHUTTLE_ITERATIONS").map_or(100_000, |n| n.parse().expect("a count"));
    shuttle::check_random(model, iterations);
}
