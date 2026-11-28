//! Model 3 (`lifecycle::model`): Loom + Shuttle concurrency model over real
//! `crate::lifecycle::DbLifecycle<T>`.
//!
//! Verifies:
//! - `enter_op()` + `LifecycleGuard::drop()` + `close()` `SeqCst` + `Condvar` drain handshake
//!   guarantees `on_drained_close` NEVER runs while any `LifecycleGuard` still holds an `Arc<T>`
//!   (`Arc::strong_count == 1` at close time, guaranteeing deterministic `mdb_env_close()`).
//! - Concurrent `close()` callers block on `close_cv` until closing finishes and observe the
//!   exact same completion outcome (both `Ok(())` and `Err(DurabilityFailed)`).
//! - Winner zero-timeout and bounded-yield timeouts wake unbounded (`None`) concurrent `close()`
//!   waiters cleanly, and `MarkCloseDone` panic unwinding never hangs waiters or reports `Ok(())`.

use std::sync::Arc as StdArc;
use std::time::Duration;

use elmdb_storage::locks::model::{Arc, AtomicBool, AtomicUsize, Ordering, explore, thread};
use elmdb_wire::{ElmdbError, ErrorCode};

use super::DbLifecycle;

struct TrackedHandle {
    dropped: AtomicBool,
}

impl TrackedHandle {
    fn new() -> Self {
        Self { dropped: AtomicBool::new(false) }
    }
}

#[test]
fn enter_op_and_close_never_drop_handle_while_op_active() {
    explore(|| {
        let lifecycle = Arc::new(DbLifecycle::new(TrackedHandle::new()));
        let close_calls = Arc::new(AtomicUsize::new(0));
        let before_take_calls = Arc::new(AtomicUsize::new(0));

        let t_op = {
            let lifecycle = Arc::clone(&lifecycle);
            thread::spawn(move || {
                if let Ok(guard) = lifecycle.enter_op() {
                    assert!(
                        !guard.dropped.load(Ordering::SeqCst),
                        "handle was already closed/dropped while LifecycleGuard was live"
                    );
                    thread::yield_now();
                    assert!(
                        !guard.dropped.load(Ordering::SeqCst),
                        "handle was closed/dropped mid-operation before LifecycleGuard dropped"
                    );
                }
            })
        };

        let t_close = {
            let lifecycle = Arc::clone(&lifecycle);
            let close_calls = Arc::clone(&close_calls);
            let before_take_calls = Arc::clone(&before_take_calls);
            thread::spawn(move || {
                lifecycle
                    .close(
                        None,
                        true,
                        false,
                        false,
                        || {
                            before_take_calls.fetch_add(1, Ordering::SeqCst);
                        },
                        |handle| {
                            // CRITICAL INVARIANT: every LifecycleGuard MUST have dropped its Arc<T>
                            // BEFORE `active_ops` hit 0 so `mdb_env_close()` runs right here!
                            assert_eq!(
                                StdArc::strong_count(&handle),
                                1,
                                "on_drained_close ran while another thread still held Arc<T>"
                            );
                            handle.dropped.store(true, Ordering::SeqCst);
                            close_calls.fetch_add(1, Ordering::SeqCst);
                            drop(handle);
                            Ok(())
                        },
                    )
                    .expect("unbounded close must succeed");
            })
        };

        t_op.join().unwrap();
        t_close.join().unwrap();

        assert_eq!(before_take_calls.load(Ordering::SeqCst), 1);
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
        let err = lifecycle.enter_op().err().expect("enter_op after close must fail");
        assert_eq!(err.code, ErrorCode::Closed);
    });
}

#[test]
fn concurrent_close_callers_wait_and_observe_same_error_outcome() {
    explore(|| {
        let lifecycle = Arc::new(DbLifecycle::new(TrackedHandle::new()));
        let close_finished = Arc::new(AtomicBool::new(false));
        let close_calls = Arc::new(AtomicUsize::new(0));

        // Hold an active op on a third thread so the winning closer actually blocks on drain_cv
        // while the second closer blocks on close_cv.
        let t_op = {
            let lifecycle = Arc::clone(&lifecycle);
            thread::spawn(move || {
                if let Ok(guard) = lifecycle.enter_op() {
                    assert!(!guard.dropped.load(Ordering::SeqCst));
                    thread::yield_now();
                }
            })
        };

        let spawn_closer = |lifecycle: Arc<DbLifecycle<TrackedHandle>>,
                            close_finished: Arc<AtomicBool>,
                            close_calls: Arc<AtomicUsize>| {
            thread::spawn(move || {
                let res = lifecycle.close(
                    None,
                    true,
                    false,
                    false,
                    || {},
                    |handle| {
                        assert_eq!(StdArc::strong_count(&handle), 1);
                        handle.dropped.store(true, Ordering::SeqCst);
                        close_calls.fetch_add(1, Ordering::SeqCst);
                        close_finished.store(true, Ordering::SeqCst);
                        Err(ElmdbError::new(ErrorCode::DurabilityFailed, "injected close sync failure"))
                    },
                );
                // NEITHER closer may return before `on_drained_close` has completed!
                assert!(close_finished.load(Ordering::SeqCst), "close returned before close finished");
                let err = res.expect_err("sync failure must propagate to all concurrent close callers");
                assert_eq!(err.code, ErrorCode::DurabilityFailed);
            })
        };

        let c1 = spawn_closer(Arc::clone(&lifecycle), Arc::clone(&close_finished), Arc::clone(&close_calls));
        let c2 = spawn_closer(Arc::clone(&lifecycle), Arc::clone(&close_finished), Arc::clone(&close_calls));

        t_op.join().unwrap();
        c1.join().unwrap();
        c2.join().unwrap();

        assert_eq!(close_calls.load(Ordering::SeqCst), 1, "on_drained_close must execute exactly once");
    });
}

#[test]
fn close_bounded_timeout_wakes_unbounded_waiter_without_hanging() {
    explore(|| {
        let lifecycle = Arc::new(DbLifecycle::new(TrackedHandle::new()));
        let guard = lifecycle.enter_op().expect("initial enter_op");

        // `c1` wins `closed.swap(true)` first via explicit start before spawning `c2`, or races
        // with `c2` when both have bounded timeouts: exercise `Some(Duration::from_millis(1))`
        // (the `remaining_yields = 2` path) alongside `Some(Duration::ZERO)` and an unbounded
        // waiter once `c1` has won!
        let r1 = lifecycle.close(
            Some(Duration::ZERO),
            true,
            false,
            false,
            || {},
            |_| panic!("on_drained_close must not run when op did not drain"),
        );
        assert_eq!(r1.expect_err("zero-timeout close must fail").code, ErrorCode::DurabilityFailed);

        // Unbounded waiter arriving after winner timed out must immediately observe the recorded error.
        let r2 = lifecycle.close(
            None,
            true,
            false,
            false,
            || {},
            |_| panic!("on_drained_close must not run for second caller"),
        );
        assert_eq!(r2.expect_err("waiter must see DurabilityFailed").code, ErrorCode::DurabilityFailed);
        drop(guard);

        // Now test two concurrent bounded (`1ms`) closers racing while an op is held (read-only mode
        // -> `DatabaseLocked`), plus `propagate_sync_err = false` (`close_at_exit` -> `Ok(())`).
        let ro_lc = Arc::new(DbLifecycle::new(TrackedHandle::new()));
        let ro_guard = ro_lc.enter_op().expect("ro enter_op");
        let t1 = {
            let ro_lc = Arc::clone(&ro_lc);
            thread::spawn(move || {
                ro_lc.close(Some(Duration::from_millis(1)), true, true, false, || {}, |_| panic!("must not drain"))
            })
        };
        let t2 = {
            let ro_lc = Arc::clone(&ro_lc);
            thread::spawn(move || {
                ro_lc.close(Some(Duration::from_millis(1)), false, true, false, || {}, |_| panic!("must not drain"))
            })
        };
        assert_eq!(t1.join().unwrap().expect_err("ro close timeout").code, ErrorCode::DatabaseLocked);
        t2.join().unwrap().expect("close_at_exit suppresses timeout error");
        drop(ro_guard);
    });
}

#[test]
fn close_panic_wakes_concurrent_waiter_with_error() {
    explore(|| {
        let lifecycle = Arc::new(DbLifecycle::new(TrackedHandle::new()));

        let c1 = {
            let lifecycle = Arc::clone(&lifecycle);
            thread::spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    lifecycle.close(
                        None,
                        true,
                        false,
                        false,
                        || {},
                        |_| {
                            panic!("simulated close sync panic");
                        },
                    )
                }));
                match res {
                    Err(_) => {} // c1 won and panicked; MarkCloseDone must have woken c2
                    Ok(Err(err)) => assert_eq!(err.code, ErrorCode::DurabilityFailed),
                    Ok(Ok(())) => panic!("panicking close must never report Ok(())"),
                }
            })
        };

        let c2 = {
            let lifecycle = Arc::clone(&lifecycle);
            thread::spawn(move || {
                let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    lifecycle.close(
                        None,
                        true,
                        false,
                        false,
                        || {},
                        |_| {
                            panic!("simulated close sync panic");
                        },
                    )
                }));
                match res {
                    Err(_) => {} // c2 won and panicked; MarkCloseDone must have woken c1
                    Ok(Err(err)) => assert_eq!(err.code, ErrorCode::DurabilityFailed),
                    Ok(Ok(())) => panic!("panicking close must never report Ok(())"),
                }
            })
        };

        c1.join().unwrap();
        c2.join().unwrap();
    });
}

/// Shuttle-only 3-thread case (2 concurrent active ops + 1 closer) verifying `active_ops`
/// drains cleanly from `2 -> 1 -> 0` and only runs `on_drained_close` once BOTH ops drop.
#[cfg(shuttle)]
#[test]
fn two_concurrent_ops_drain_completely_before_close() {
    explore(|| {
        let lifecycle = Arc::new(DbLifecycle::new(TrackedHandle::new()));
        let closed_ok = Arc::new(AtomicBool::new(false));

        let spawn_op = |lifecycle: Arc<DbLifecycle<TrackedHandle>>| {
            thread::spawn(move || {
                if let Ok(guard) = lifecycle.enter_op() {
                    assert!(!guard.dropped.load(Ordering::SeqCst));
                    thread::yield_now();
                    assert!(!guard.dropped.load(Ordering::SeqCst));
                }
            })
        };

        let op1 = spawn_op(Arc::clone(&lifecycle));
        let op2 = spawn_op(Arc::clone(&lifecycle));
        let closer = {
            let lifecycle = Arc::clone(&lifecycle);
            let closed_ok = Arc::clone(&closed_ok);
            thread::spawn(move || {
                lifecycle
                    .close(
                        None,
                        true,
                        false,
                        false,
                        || {},
                        |handle| {
                            assert_eq!(StdArc::strong_count(&handle), 1);
                            handle.dropped.store(true, Ordering::SeqCst);
                            closed_ok.store(true, Ordering::SeqCst);
                            Ok(())
                        },
                    )
                    .expect("close");
            })
        };

        op1.join().unwrap();
        op2.join().unwrap();
        closer.join().unwrap();
        assert!(closed_ok.load(Ordering::SeqCst));
    });
}
