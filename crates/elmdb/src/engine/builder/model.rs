//! The index builder's scheduling under Shuttle's threads (`crate::sync::model`): its queue, the pause a rank index's
//! drain waits for, stopping, failures and the waiters are the builder's own `Core`; what a piece of work does is the
//! stand-in each case gives it, and time is a clock the case moves, Shuttle modelling none

use std::time::{Duration, Instant};

use super::{Core, PATIENCE, QUIET, Waited, Work};
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

/// Another index, for what a change leaves to build
fn other() -> Work {
    Work::Index { entity_type: "Task".to_owned(), index: "b".to_owned() }
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
                    core.queue([rank()]);
                    let asked = order.fetch_add(1, Ordering::SeqCst);
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
fn work_queued_while_it_runs_runs_again() {
    explore(|| {
        let (core, _) = core();
        let runs = Arc::new(AtomicU64::new(0));
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let runner = {
            let (core, runs, started, release) =
                (Arc::clone(&core), Arc::clone(&runs), Arc::clone(&started), Arc::clone(&release));
            thread::spawn(move || {
                core.run(|_, _| {
                    if runs.fetch_add(1, Ordering::SeqCst) == 0 {
                        started.store(true, Ordering::SeqCst);
                        while !release.load(Ordering::SeqCst) {
                            thread::yield_now();
                        }
                    }
                    Ok(())
                });
            })
        };
        core.queue([index()]);
        while !started.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        // A commit landing as a drain finishes: the drain has read what it reads, so this needs a run of its own
        core.queue([index()]);
        release.store(true, Ordering::SeqCst);
        core.wait();
        assert_eq!(runs.load(Ordering::SeqCst), 2, "work queued while it ran never ran after");
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
fn a_stop_ends_the_builder_waiting_or_pausing_and_its_waiters() {
    explore(|| {
        // The clock never moves, so a rank index's pause would last until the stop
        let (core, _) = core();
        let runner = {
            let core = Arc::clone(&core);
            thread::spawn(move || core.run(|_, _| Ok(())))
        };
        let queuer = {
            let core = Arc::clone(&core);
            thread::spawn(move || core.queue([rank()]))
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
fn a_stop_ends_the_work_underway_and_its_waiters() {
    explore(|| {
        let (core, _) = core();
        let started = Arc::new(AtomicBool::new(false));
        let runner = {
            let (core, started) = (Arc::clone(&core), Arc::clone(&started));
            thread::spawn(move || {
                core.run(|_, stopping| {
                    started.store(true, Ordering::SeqCst);
                    while !stopping() {
                        thread::yield_now();
                    }
                    Ok(())
                });
            })
        };
        core.queue([index()]);
        let waiter = {
            let core = Arc::clone(&core);
            thread::spawn(move || core.wait())
        };
        while !started.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        core.stop();
        runner.join().unwrap();
        waiter.join().unwrap();
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
        // When the case last queued it, as the case knows it
        let last = Arc::new(AtomicU64::new(0));
        let runner = {
            let (core, ticks, last) = (Arc::clone(&core), std::sync::Arc::clone(&ticks), Arc::clone(&last));
            thread::spawn(move || {
                core.run(move |_, _| {
                    let now = Duration::from_nanos(ticks.elapsed.load(Ordering::SeqCst));
                    let queued = Duration::from_nanos(last.load(Ordering::SeqCst));
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
        last.store(ticks.elapsed.load(Ordering::SeqCst), Ordering::SeqCst);
        core.queue([rank()]);
        tick(&core, &ticks, step);
        tick(&core, &ticks, step);
        core.wait();
        core.stop();
        runner.join().unwrap();
    });
}

#[test]
fn a_steady_stream_of_commits_is_drained_once_patience_has_passed() {
    explore(|| {
        let (core, ticks) = core();
        let started = Arc::new(AtomicBool::new(false));
        let runner = {
            let (core, ticks, started) = (Arc::clone(&core), std::sync::Arc::clone(&ticks), Arc::clone(&started));
            thread::spawn(move || {
                core.run(move |_, _| {
                    let now = Duration::from_nanos(ticks.elapsed.load(Ordering::SeqCst));
                    assert!(now >= PATIENCE, "the drain started at {now:?}, the commits never having paused");
                    started.store(true, Ordering::SeqCst);
                    Ok(())
                });
            })
        };
        // Queued again before each `QUIET` has passed, so only the cap ends the pause: without it this never ends
        let step = QUIET / 2;
        while !started.load(Ordering::SeqCst) {
            core.queue([rank()]);
            tick(&core, &ticks, step);
            thread::yield_now();
        }
        core.stop();
        runner.join().unwrap();
    });
}

/// What the builder builds to is replaced by a change that begins and ends (`Core::begin_change`, `end_change`), a
/// schema change's. Between the two nothing is queued, and a wait that answered then would say built of what the
/// change is about to queue
#[test]
fn a_wait_asked_for_as_a_change_is_made_waits_for_the_work_the_change_leaves() {
    explore(|| {
        let (core, _) = core();
        let done = Arc::new(AtomicBool::new(false));
        let runner = {
            let (core, done) = (Arc::clone(&core), Arc::clone(&done));
            thread::spawn(move || {
                core.run(|work, _| {
                    if *work == other() {
                        done.store(true, Ordering::SeqCst);
                    }
                    Ok(())
                });
            })
        };
        core.begin_change();
        let waiter = {
            let (core, done) = (Arc::clone(&core), Arc::clone(&done));
            thread::spawn(move || {
                core.wait();
                assert!(done.load(Ordering::SeqCst), "the wait answered with the change's work not done");
            })
        };
        core.end_change([other()], true);
        waiter.join().unwrap();
        core.stop();
        runner.join().unwrap();
    });
}

#[test]
fn work_under_way_as_a_change_begins_is_told_to_stop_and_is_neither_done_nor_failed() {
    explore(|| {
        let (core, _) = core();
        let started = Arc::new(AtomicBool::new(false));
        let runner = {
            let (core, started) = (Arc::clone(&core), Arc::clone(&started));
            thread::spawn(move || {
                core.run(|_, stopping| {
                    started.store(true, Ordering::SeqCst);
                    // As a build goes on a chunk at a time until told to stop, and is then refused its commit by what
                    // the change stored
                    while !stopping() {
                        thread::yield_now();
                    }
                    Err(Error::new(ErrorCode::SchemaChanged, "what it built to was replaced"))
                });
            })
        };
        core.queue([index()]);
        while !started.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        core.begin_change();
        core.end_change([], true);
        assert!(core.wait().is_none());
        assert!(core.lock().failures.is_empty(), "work of what the builder built to before is listed as failed");
        core.stop();
        runner.join().unwrap();
    });
}

#[test]
fn work_queued_before_a_change_is_not_run_after_it() {
    explore(|| {
        let (core, _) = core();
        let started = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let ran = Arc::new(AtomicU64::new(0));
        let runner = {
            let (core, started, release, ran) =
                (Arc::clone(&core), Arc::clone(&started), Arc::clone(&release), Arc::clone(&ran));
            thread::spawn(move || {
                core.run(|work, _| {
                    if *work == other() {
                        ran.fetch_add(1, Ordering::SeqCst);
                        return Ok(());
                    }
                    started.store(true, Ordering::SeqCst);
                    while !release.load(Ordering::SeqCst) {
                        thread::yield_now();
                    }
                    Ok(())
                });
            })
        };
        core.queue([index()]);
        while !started.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        // Queued behind the work under way, for what the builder builds to now
        core.queue([other()]);
        core.begin_change();
        release.store(true, Ordering::SeqCst);
        core.end_change([], true);
        core.wait();
        assert_eq!(ran.load(Ordering::SeqCst), 0, "work queued for what the builder built to before ran after");
        core.stop();
        runner.join().unwrap();
    });
}

#[test]
fn the_work_a_change_leaves_and_the_work_queued_as_it_is_made_each_run_once() {
    explore(|| {
        let (core, _) = core();
        let runs = Arc::new(Mutex::new(Vec::new()));
        let runner = {
            let (core, runs) = (Arc::clone(&core), Arc::clone(&runs));
            thread::spawn(move || {
                core.run(|work, _| {
                    runs.lock().unwrap().push(work.clone());
                    Ok(())
                });
            })
        };
        core.begin_change();
        // A commit that leaves a rank index pushes, landing as the change is made
        core.queue([index()]);
        core.end_change([other()], true);
        core.wait();
        let mut runs = runs.lock().unwrap().clone();
        runs.sort_by_key(|work| format!("{work:?}"));
        assert_eq!(runs, [index(), other()]);
        core.stop();
        runner.join().unwrap();
    });
}

/// A schema change made by a listener that a build's commit delivered to runs on the builder's thread, within the
/// piece of work under way: that work is of what the builder built to before, and the thread goes on to what the
/// change left
#[test]
fn a_change_made_by_the_work_under_way_leaves_it_undone_and_the_thread_at_what_the_change_left() {
    explore(|| {
        let (core, _) = core();
        let ran = Arc::new(AtomicBool::new(false));
        let runner = {
            let (core, ran) = (Arc::clone(&core), Arc::clone(&ran));
            thread::spawn(move || {
                let within = Arc::clone(&core);
                core.run(|work, stopping| {
                    if *work == other() {
                        ran.store(true, Ordering::SeqCst);
                        return Ok(());
                    }
                    within.begin_change();
                    within.end_change([other()], true);
                    assert!(stopping(), "the work under way isn't told to stop by the change it made");
                    Err(Error::new(ErrorCode::SchemaChanged, "what it built to was replaced"))
                });
            })
        };
        core.queue([index()]);
        assert!(core.wait().is_none());
        assert!(ran.load(Ordering::SeqCst), "the wait answered with the change's work not done");
        assert!(core.lock().failures.is_empty());
        core.stop();
        runner.join().unwrap();
    });
}

#[test]
fn a_stop_ends_a_wait_for_a_change_that_has_not_ended() {
    explore(|| {
        let (core, _) = core();
        core.begin_change();
        let waiter = {
            let core = Arc::clone(&core);
            thread::spawn(move || core.wait())
        };
        core.stop();
        waiter.join().unwrap();
    });
}

/// Work queued as a change is made is for what the change leaves, which the builder isn't told until the change
/// ends: taken before, it would be built to what the builder built to before
#[test]
fn work_queued_as_a_change_is_made_is_not_begun_until_every_change_has_ended() {
    explore(|| {
        let (core, _) = core();
        let ended = Arc::new(AtomicBool::new(false));
        let runner = {
            let (core, ended) = (Arc::clone(&core), Arc::clone(&ended));
            thread::spawn(move || {
                core.run(|_, _| {
                    assert!(ended.load(Ordering::SeqCst), "work begun with a change not yet ended");
                    Ok(())
                });
            })
        };
        // Two changes at once, a schema change's and an adoption's: the count of them is what the thread waits on
        core.begin_change();
        core.begin_change();
        core.queue([index()]);
        core.end_change([], true);
        thread::yield_now();
        ended.store(true, Ordering::SeqCst);
        core.end_change([other()], false);
        core.wait();
        core.stop();
        runner.join().unwrap();
    });
}

/// A change that wasn't made leaves what had failed as true as it was, and one that was made leaves none
#[test]
fn a_change_that_was_not_made_keeps_the_failures_and_one_that_was_made_clears_them() {
    explore(|| {
        let (core, _) = core();
        let runner = {
            let core = Arc::clone(&core);
            thread::spawn(move || core.run(|_, _| Err(Error::new(ErrorCode::Io, "the stand-in fails"))))
        };
        core.queue([index()]);
        core.wait();
        assert_eq!(core.lock().failures.len(), 1);
        let left = core.begin_change();
        assert!(left.is_empty(), "the failed work is neither queued nor under way");
        core.end_change(left, false);
        core.wait();
        assert_eq!(core.lock().failures.len(), 1, "a change that wasn't made forgot a build that failed");
        core.begin_change();
        core.end_change([], true);
        core.wait();
        assert!(core.lock().failures.is_empty(), "a change that was made kept a failure of what was built to before");
        core.stop();
        runner.join().unwrap();
    });
}

/// A close tells the thread to stop and then says whether the database closed (`Core::begin_close`, `end_close`).
/// One that gives up leaves the database open with what is left queued for another thread, and a wait that
/// answered while it was under way would say built of that
#[test]
fn a_wait_asked_for_as_a_close_is_under_way_waits_for_what_a_close_that_gives_up_leaves() {
    explore(|| {
        let (core, _) = core();
        let done = Arc::new(AtomicBool::new(false));
        core.begin_close();
        let waiter = {
            let (core, done) = (Arc::clone(&core), Arc::clone(&done));
            thread::spawn(move || {
                core.wait();
                assert!(done.load(Ordering::SeqCst), "the wait answered with what the close left not built");
            })
        };
        core.queue([other()]);
        core.end_close(false);
        // The thread started for what was left
        let runner = {
            let (core, done) = (Arc::clone(&core), Arc::clone(&done));
            thread::spawn(move || {
                core.run(|_, _| {
                    done.store(true, Ordering::SeqCst);
                    Ok(())
                });
            })
        };
        waiter.join().unwrap();
        core.stop();
        runner.join().unwrap();
    });
}

#[test]
fn a_close_that_closes_ends_a_wait_and_one_under_way_is_said_to_a_caller_it_may_wait_for() {
    explore(|| {
        let (core, _) = core();
        core.queue([index()]);
        core.begin_close();
        // A listener, which the close may be waiting for
        assert!(matches!(core.waited(false), Waited::Closing));
        let waiter = {
            let core = Arc::clone(&core);
            thread::spawn(move || assert!(matches!(core.waited(true), Waited::Idle)))
        };
        core.end_close(true);
        waiter.join().unwrap();
        // Closed, there is nothing to wait for, whatever was queued
        assert!(matches!(core.waited(false), Waited::Idle));
    });
}

/// The thread a close stops ends with no work under way, so the thread started after a close that gave up finds the
/// builder idle but for what is queued, and whoever waits is woken by that thread's work, not held by the first's
#[test]
fn the_thread_a_close_stops_ends_idle_and_another_takes_what_is_queued() {
    explore(|| {
        let (core, _) = core();
        let started = Arc::new(AtomicBool::new(false));
        let first = {
            let (core, started) = (Arc::clone(&core), Arc::clone(&started));
            thread::spawn(move || {
                core.run(|_, stopping| {
                    started.store(true, Ordering::SeqCst);
                    while !stopping() {
                        thread::yield_now();
                    }
                    Ok(())
                });
            })
        };
        core.queue([index()]);
        while !started.load(Ordering::SeqCst) {
            thread::yield_now();
        }
        core.begin_close();
        first.join().unwrap();
        {
            let state = core.lock();
            assert!(!state.busy && state.active.is_none(), "the thread ended with work under way");
        }
        core.queue([other()]);
        core.end_close(false);
        let ran = Arc::new(AtomicBool::new(false));
        let second = {
            let (core, ran) = (Arc::clone(&core), Arc::clone(&ran));
            thread::spawn(move || {
                core.run(|work, _| {
                    if *work == other() {
                        ran.store(true, Ordering::SeqCst);
                    }
                    Ok(())
                });
            })
        };
        core.wait();
        assert!(ran.load(Ordering::SeqCst), "the wait answered with what was queued for the second thread not built");
        core.stop();
        second.join().unwrap();
    });
}
