//! The index builder's scheduling under Shuttle's threads (`crate::sync::model`): its queue, the pause a rank index's
//! drain waits for, stopping, failures and the waiters are the builder's own `Core`; what a piece of work does is the
//! stand-in each case gives it, and time is a clock the case moves, Shuttle modelling none

use std::time::{Duration, Instant};

use super::{Core, PATIENCE, QUIET, Work};
use crate::error::{Error, ErrorCode};
use crate::store::sync::Clock;
use crate::sync::Mutex;
use crate::sync::model::{Arc, AtomicBool, AtomicU64, Ordering, explore, thread};

/// Time that moves only when a case moves it, in nanoseconds since the case began
struct Ticks {
    base: Instant,
    elapsed: AtomicU64,
}

struct ManualClock(std::sync::Arc<Ticks>);

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.0.base + Duration::from_nanos(self.0.elapsed.load(Ordering::SeqCst))
    }
    fn is_manual(&self) -> bool {
        true
    }
}

fn core() -> (Arc<Core>, std::sync::Arc<Ticks>) {
    let ticks = std::sync::Arc::new(Ticks { base: Instant::now(), elapsed: AtomicU64::new(0) });
    (Arc::new(Core::new(Box::new(ManualClock(std::sync::Arc::clone(&ticks))))), ticks)
}

/// Moves the clock under the builder's lock, which its waits check it under, and wakes them
fn tick(core: &Core, ticks: &Ticks, by: Duration) {
    let state = core.lock();
    ticks.elapsed.fetch_add(u64::try_from(by.as_nanos()).expect("a short tick"), Ordering::SeqCst);
    drop(state);
    core.changed.notify_all();
}

fn rank() -> Work {
    Work::Rank { name: "rank".to_owned() }
}

fn index() -> Work {
    Work::Index { entity_type: "Task".to_owned(), index: "a".to_owned() }
}

#[test]
fn work_queued_is_run_after_it_was_queued_and_waits_in_the_queue_once() {
    explore(|| {
        let (core, ticks) = core();
        // One count for the order of queue calls and runs
        let order = Arc::new(AtomicU64::new(0));
        let starts = Arc::new(Mutex::new(Vec::new()));
        let runner = {
            let (core, order, starts) = (Arc::clone(&core), Arc::clone(&order), Arc::clone(&starts));
            thread::spawn(move || {
                core.run(|_, _| {
                    starts.lock().unwrap().push(order.fetch_add(1, Ordering::SeqCst));
                    Ok(())
                });
            })
        };
        let queuers: Vec<_> = (0..2)
            .map(|_| {
                let (core, order) = (Arc::clone(&core), Arc::clone(&order));
                thread::spawn(move || {
                    let asked = order.fetch_add(1, Ordering::SeqCst);
                    core.queue([rank()]);
                    let waiting = core.lock().queue.iter().filter(|work| **work == rank()).count();
                    assert!(waiting <= 1, "the rank index waits in the queue {waiting} times");
                    asked
                })
            })
            .collect();
        let asked: Vec<u64> = queuers.into_iter().map(|queuer| queuer.join().unwrap()).collect();
        tick(&core, &ticks, QUIET);
        core.wait();
        let starts = starts.lock().unwrap().clone();
        for asked in asked {
            assert!(starts.iter().any(|start| *start > asked), "work queued at {asked} never ran after: {starts:?}");
        }
        core.stop();
        runner.join().unwrap();
    });
}

#[test]
fn a_waiter_returns_once_the_work_queued_before_it_is_done() {
    explore(|| {
        let (core, _) = core();
        let done = Arc::new(AtomicBool::new(false));
        let runner = {
            let (core, done) = (Arc::clone(&core), Arc::clone(&done));
            thread::spawn(move || {
                core.run(|_, _| {
                    thread::yield_now();
                    done.store(true, Ordering::SeqCst);
                    Ok(())
                });
            })
        };
        let waiter = {
            let core = Arc::clone(&core);
            thread::spawn(move || {
                // An index's work waits for no pause, so no tick is needed for it to run
                core.queue([index()]);
                core.wait();
            })
        };
        waiter.join().unwrap();
        assert!(done.load(Ordering::SeqCst), "the waiter returned before the work queued before it was done");
        core.stop();
        runner.join().unwrap();
    });
}

#[test]
fn a_stop_ends_the_builder_waiting_pausing_or_working_and_its_waiters() {
    explore(|| {
        // The clock never moves, so a rank index's pause would last until the stop
        let (core, _) = core();
        let runner = {
            let core = Arc::clone(&core);
            thread::spawn(move || {
                core.run(|work, stopping| {
                    if *work == index() {
                        while !stopping() {
                            thread::yield_now();
                        }
                    }
                    Ok(())
                });
            })
        };
        let queuer = {
            let core = Arc::clone(&core);
            thread::spawn(move || core.queue([index(), rank()]))
        };
        let waiter = {
            let core = Arc::clone(&core);
            thread::spawn(move || core.wait())
        };
        core.stop();
        runner.join().unwrap();
        waiter.join().unwrap();
        queuer.join().unwrap();
    });
}

#[test]
fn a_failure_is_seen_by_the_waiter_its_work_wakes() {
    explore(|| {
        let (core, _) = core();
        let runner = {
            let core = Arc::clone(&core);
            thread::spawn(move || core.run(|_, _| Err(Error::new(ErrorCode::Io, "the stand-in fails"))))
        };
        core.queue([index()]);
        core.wait();
        let failures = core.lock().failures.clone();
        assert_eq!(failures.len(), 1, "the waiter returned without the failure: {failures:?}");
        assert_eq!(failures[0].work, index());
        core.stop();
        runner.join().unwrap();
    });
}

#[test]
fn a_rank_index_waits_for_its_commits_to_pause() {
    explore(|| {
        let (core, ticks) = core();
        let runner = {
            let (core, ticks) = (Arc::clone(&core), std::sync::Arc::clone(&ticks));
            thread::spawn(move || {
                let inner = Arc::clone(&core);
                core.run(move |_, _| {
                    let now = Duration::from_nanos(ticks.elapsed.load(Ordering::SeqCst));
                    let state = inner.lock();
                    let queued = state.queued_at["rank"].saturating_duration_since(ticks.base);
                    // The case's ticks add to less than `PATIENCE`, so only a pause lets the work start
                    assert!(now < PATIENCE);
                    assert!(now - queued >= QUIET, "the drain started {:?} after the last commit", now - queued);
                    Ok(())
                });
            })
        };
        let step = Duration::from_millis(3);
        core.queue([rank()]);
        tick(&core, &ticks, step);
        core.queue([rank()]);
        tick(&core, &ticks, step);
        tick(&core, &ticks, step);
        core.wait();
        core.stop();
        runner.join().unwrap();
    });
}
