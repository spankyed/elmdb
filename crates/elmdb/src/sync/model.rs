//! What a model runs on, Loom's threads or Shuttle's, and how it explores them: Loom every interleaving within
//! `LOOM_MAX_PREEMPTIONS`; Shuttle `ELMDB_SHUTTLE_ITERATIONS` schedules (100,000 by default), a third random and a third
//! each under PCT at depths 2 and 3, printing a failing one's schedule for `shuttle::replay`

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

/// Random schedules find what any interleaving shows; PCT (Burckhardt et al., 2010) finds a bug that needs `d`
/// orderings in a row with a probability it bounds from below, which uniformly random schedules don't, so a model runs
/// under both, at the two depths an ordering bug between two threads takes
#[cfg(shuttle)]
pub(crate) fn explore(model: impl Fn() + Send + Sync + 'static) {
    let iterations = std::env::var("ELMDB_SHUTTLE_ITERATIONS").map_or(100_000, |n| n.parse().expect("a count"));
    let each = (iterations / 3).max(1);
    let model = std::sync::Arc::new(model);
    let run = |model: &std::sync::Arc<dyn Fn() + Send + Sync>| {
        let model = std::sync::Arc::clone(model);
        move || model()
    };
    let model: std::sync::Arc<dyn Fn() + Send + Sync> = model;
    shuttle::check_random(run(&model), each);
    shuttle::check_pct(run(&model), each, 2);
    shuttle::check_pct(run(&model), each, 3);
}
