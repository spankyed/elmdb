//! Which sync each `durable()` waits for, and when the worker syncs on its own. A test finishes or fails every sync
//! (`ManualSyncer`) and moves time (`ManualClock`), so nothing here depends on how long anything takes. Ported from
//! apack's `tests/lmdb/durability.spec.ts` and the durability half of `store.spec.ts`.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::error::ErrorCode;

const IDLE: Duration = Duration::from_millis(100);
const CAP: Duration = Duration::from_millis(1000);

/// A sync the test settles: each started sync waits for one `finish`
struct ManualSyncer {
    started: Sender<()>,
    outcomes: Mutex<Receiver<Result<()>>>,
}

impl Syncer for ManualSyncer {
    fn sync(&self) -> Result<()> {
        // A test that has gone (its harness dropped, as a failing test's is while it unwinds) fails what's left
        if self.started.send(()).is_err() {
            return Err(Error::new(ErrorCode::Io, "the test has gone"));
        }
        self.outcomes.lock().unwrap().recv().unwrap_or_else(|_| Err(Error::new(ErrorCode::Io, "the test has gone")))
    }
}

/// Time that moves only when the test moves it
struct ManualClock {
    base: Instant,
    elapsed: Arc<Mutex<Duration>>,
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + *self.elapsed.lock().unwrap()
    }
    fn is_manual(&self) -> bool {
        true
    }
}

/// Its fields drop in order, so the test's ends of the channels go before the worker, whose close then fails its last
/// sync rather than waiting on a test that has gone
struct Harness {
    started: Receiver<()>,
    finish: Sender<Result<()>>,
    elapsed: Arc<Mutex<Duration>>,
    commit: GroupCommit,
}

impl Harness {
    fn new() -> Self {
        let (started_tx, started) = channel();
        let (finish, outcomes) = channel();
        let elapsed = Arc::new(Mutex::new(Duration::ZERO));
        let commit = GroupCommit::start(
            Policy::new(IDLE, CAP),
            Box::new(ManualSyncer { started: started_tx, outcomes: Mutex::new(outcomes) }),
            Box::new(ManualClock { base: Instant::now(), elapsed: Arc::clone(&elapsed) }),
        );
        Self { commit, started, finish, elapsed }
    }

    /// Whether a sync has started that the test hasn't settled; waits briefly for one to start
    fn sync_started(&self) -> bool {
        self.started.recv_timeout(Duration::from_millis(200)).is_ok()
    }

    /// Asserts no sync starts (a sync the worker would start now would have started within this wait)
    fn no_sync_starts(&self) {
        assert!(self.started.recv_timeout(Duration::from_millis(50)).is_err(), "a sync started");
    }

    fn finish(&self, outcome: Result<()>) {
        self.finish.send(outcome).unwrap();
    }

    /// Waits for the worker to finish ending the sync the test just settled
    fn settled(&self) {
        for _ in 0..200 {
            if self.commit.shared.lock().running.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the worker never ended the sync");
    }

    /// Waits for the worker to finish ending the sync with this id
    fn ended(&self, id: u64) {
        for _ in 0..400 {
            if self.commit.shared.lock().finished_through >= id {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the worker never ended sync {id}");
    }

    fn advance(&self, by: Duration) {
        *self.elapsed.lock().unwrap() += by;
        self.commit.wake();
    }

    fn close(self) {
        self.closed().unwrap();
    }

    /// Closes after a sync failed, which the close reports
    fn close_after_failure(self) {
        assert!(self.closed().unwrap_err().message.contains("EIO"));
    }

    fn closed(self) -> Result<()> {
        // Close syncs what is owed: settle each sync that starts until it returns
        let Harness { mut commit, started, finish, .. } = self;
        let closer = std::thread::spawn(move || commit.close());
        while !closer.is_finished() {
            if started.recv_timeout(Duration::from_millis(20)).is_ok() {
                finish.send(Ok(())).unwrap();
            }
        }
        closer.join().unwrap()
    }
}

fn outcome(durable: &Durable) -> &'static str {
    // The worker records an outcome as it ends a sync; give it the moment it takes
    for _ in 0..40 {
        match durable.try_result() {
            Some(Ok(())) => return "resolved",
            Some(Err(_)) => return "rejected",
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    "pending"
}

fn eio() -> Error {
    Error::new(ErrorCode::Io, "EIO")
}

#[test]
fn the_policy_is_due_after_the_idle_time_or_the_cap() {
    let t0 = Instant::now();
    let mut p = Policy::new(IDLE, CAP);
    assert_eq!(p.due_at(), None);
    p.on_commit(t0);
    assert_eq!(p.due_at(), Some(t0 + IDLE));
    // Each commit pushes the idle sync back, up to the cap after the first
    p.on_commit(t0 + Duration::from_millis(950));
    assert_eq!(p.due_at(), Some(t0 + CAP));
    p.on_sync_started();
    assert_eq!(p.due_at(), None);
}

#[test]
fn waits_for_a_sync_begun_after_the_write_and_a_write_during_it_waits_for_the_next() {
    let h = Harness::new();
    h.commit.on_commit();
    let first = h.commit.durable();
    assert!(h.sync_started(), "a sync starts at once, not at the timer");

    h.commit.on_commit();
    let second = h.commit.durable();
    h.no_sync_starts();

    h.finish(Ok(()));
    assert_eq!(outcome(&first), "resolved");
    assert_eq!(outcome(&second), "pending", "the first sync finishing is not the second write on disk");
    assert!(h.sync_started(), "the next sync starts once the first ends");
    h.finish(Ok(()));
    assert_eq!(outcome(&second), "resolved");
    h.close();
}

#[test]
fn shares_the_running_sync_with_a_caller_that_committed_nothing_since() {
    let h = Harness::new();
    h.commit.on_commit();
    let asked = h.commit.durable();
    assert!(h.sync_started());
    let joined = h.commit.durable();
    h.no_sync_starts();
    h.finish(Ok(()));
    assert_eq!(outcome(&asked), "resolved");
    assert_eq!(outcome(&joined), "resolved");
    h.close();
}

#[test]
fn rejects_every_caller_when_its_sync_fails_and_every_wait_after_it() {
    let h = Harness::new();
    h.commit.on_commit();
    let asked = h.commit.durable();
    let joined = h.commit.durable();
    assert!(h.sync_started());
    h.finish(Err(eio()));
    assert_eq!(outcome(&asked), "rejected");
    assert_eq!(outcome(&joined), "rejected", "a caller sharing the failed sync is told so too");
    assert_eq!(asked.wait().unwrap_err().message, "EIO");

    h.commit.on_commit();
    let later = h.commit.durable();
    assert_eq!(outcome(&later), "rejected", "no sync after a failed one can vouch for what it couldn't write");
    h.no_sync_starts();
    assert!(later.wait().unwrap_err().message.contains("EIO"));
    h.close_after_failure();
}

#[test]
fn a_sync_queued_behind_a_failed_one_fails_its_caller_though_it_succeeds() {
    let h = Harness::new();
    h.commit.on_commit();
    let first = h.commit.durable();
    assert!(h.sync_started());
    h.commit.on_commit();
    let second = h.commit.durable();
    h.finish(Err(eio()));
    assert_eq!(outcome(&first), "rejected");
    assert_eq!(outcome(&second), "pending", "the queued sync still runs");
    assert!(h.sync_started());
    h.finish(Ok(()));
    assert_eq!(outcome(&second), "rejected", "the second caller's commits are built on what the first sync lost");
    h.close_after_failure();
}

#[test]
fn a_failed_background_sync_fails_every_wait_after_it() {
    let h = Harness::new();
    h.commit.on_commit();
    let asked = h.commit.durable();
    assert!(h.sync_started());
    h.commit.on_commit();
    // The timer comes due while that sync runs, so the worker syncs again once it ends, with nobody waiting
    h.advance(IDLE);
    h.finish(Ok(()));
    assert_eq!(outcome(&asked), "resolved");
    assert!(h.sync_started(), "the timer's sync started");
    h.finish(Err(eio()));
    h.settled();

    let after = h.commit.durable();
    assert_eq!(outcome(&after), "rejected");
    h.no_sync_starts();
    h.close_after_failure();
}

#[test]
fn starts_no_sync_when_nothing_was_committed_since_the_last_finished() {
    let h = Harness::new();
    let nothing = h.commit.durable();
    assert_eq!(outcome(&nothing), "resolved");
    h.no_sync_starts();
    h.commit.on_commit();
    let asked = h.commit.durable();
    assert!(h.sync_started());
    h.finish(Ok(()));
    assert_eq!(outcome(&asked), "resolved");
    assert_eq!(outcome(&h.commit.durable()), "resolved");
    h.no_sync_starts();
    h.close();
}

#[test]
fn syncs_once_writes_pause_a_burst_one_sync_after_the_last() {
    let h = Harness::new();
    for _ in 0..5 {
        h.commit.on_commit();
        h.advance(IDLE / 2);
    }
    h.no_sync_starts();
    h.advance(IDLE / 2);
    assert!(h.sync_started());
    h.finish(Ok(()));
    h.advance(CAP);
    h.no_sync_starts();
    assert_eq!(h.commit.syncs(), 1, "nothing is owed after it");
    h.close();
}

#[test]
fn syncs_a_steady_stream_at_the_cap_after_its_first_unsynced_commit() {
    let h = Harness::new();
    let step = IDLE / 2;
    let mut elapsed = Duration::ZERO;
    while elapsed + step < CAP {
        h.commit.on_commit();
        h.advance(step);
        elapsed += step;
    }
    h.no_sync_starts();
    h.commit.on_commit();
    h.advance(step);
    assert!(h.sync_started());
    h.finish(Ok(()));
    h.close();
}

#[test]
fn a_wait_given_up_on_keeps_no_outcome() {
    let h = Harness::new();
    h.commit.on_commit();
    drop(h.commit.durable());
    assert!(h.sync_started());
    h.finish(Err(eio()));
    assert!(h.started.recv_timeout(Duration::from_millis(50)).is_err());
    assert!(h.commit.shared.lock().waiting.is_empty(), "the dropped wait's attempt is forgotten");
    h.close_after_failure();
}

#[test]
fn stopping_at_exit_waits_on_no_worker() {
    // A sync the test never settles stands in for a worker an exiting process has ended mid-sync, holding the syncer
    let Harness { commit, started, finish, .. } = Harness::new();
    commit.on_commit();
    drop(commit.durable());
    started.recv_timeout(Duration::from_millis(200)).expect("a sync started");
    commit.on_commit();
    let (stopped, stop) = channel();
    std::thread::spawn(move || {
        commit.stop_at_exit(Duration::from_millis(20));
        stopped.send(()).unwrap();
    });
    assert!(stop.recv_timeout(Duration::from_secs(5)).is_ok(), "the stop waited on the worker");
    // Let the worker end, as an exiting process would never need it to
    finish.send(Ok(())).unwrap();
}

#[test]
fn stopping_at_exit_drops_the_syncer_and_stops_the_worker_once_their_locks_come_free() {
    // Idle: both locks are free at once
    let Harness { commit, started, finish: _finish, .. } = Harness::new();
    let shared = Arc::clone(&commit.shared);
    commit.stop_at_exit(Duration::from_millis(20));
    assert!(shared.lock().closing, "the worker is told to stop");
    assert!(
        matches!(started.recv_timeout(Duration::from_secs(5)), Err(std::sync::mpsc::RecvTimeoutError::Disconnected)),
        "the syncer is dropped"
    );

    // Mid-sync: the syncer's lock comes free within the wait
    let Harness { commit, started, finish, .. } = Harness::new();
    commit.on_commit();
    drop(commit.durable());
    started.recv_timeout(Duration::from_millis(200)).expect("a sync started");
    let shared = Arc::clone(&commit.shared);
    let stopping = std::thread::spawn(move || commit.stop_at_exit(Duration::from_secs(5)));
    std::thread::sleep(Duration::from_millis(50));
    finish.send(Ok(())).unwrap();
    stopping.join().unwrap();
    assert!(shared.syncer.lock().unwrap().is_none(), "the stop waited for the sync and dropped the syncer");
}

#[test]
fn dropping_group_commit_syncs_what_it_owes() {
    let Harness { commit, started, finish, .. } = Harness::new();
    commit.on_commit();
    let dropping = std::thread::spawn(move || drop(commit));
    assert!(started.recv_timeout(Duration::from_secs(5)).is_ok(), "the drop syncs the owed commit");
    finish.send(Ok(())).unwrap();
    dropping.join().unwrap();
}

/// The failed-sync harness (`docs/architecture/testing.md`): random commits, waits, syncs finished or failed, and time, checked against
/// what a wait promises rather than against a copy of the implementation. A wait that resolved had a sync covering its
/// commits finish successfully, and no sync failed before it settled; a wait that rejected had a sync fail before it
/// settled; and once every sync is let through, every wait has settled and a new one resolves unless a sync failed.
mod failed_syncs {
    use proptest::prelude::*;

    use super::*;

    #[derive(Debug, Clone)]
    enum Event {
        Commit,
        Wait,
        Finish(bool),
        Advance(u64),
    }

    fn event() -> impl Strategy<Value = Event> {
        prop_oneof![
            3 => Just(Event::Commit),
            2 => Just(Event::Wait),
            // Successes outnumber failures, so waits resolve as well as reject
            2 => Just(Event::Finish(true)),
            1 => Just(Event::Finish(false)),
            1 => (1u64..300).prop_map(Event::Advance),
        ]
    }

    struct Attempt {
        id: u64,
        mark: u64,
        started: u64,
        finished: Option<(u64, bool)>,
    }

    struct Waiting {
        target: u64,
        created: u64,
        handle: Durable,
        settled: Option<(u64, bool)>,
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 48, failure_persistence: None, ..ProptestConfig::default() })]

        #[test]
        fn a_wait_resolves_only_on_a_covering_sync_before_any_failure_and_rejects_only_after_one(events in prop::collection::vec(event(), 1..30)) {
            let h = Harness::new();
            let mut clock = 0u64;
            let mut commits = 0u64;
            let mut attempts: Vec<Attempt> = Vec::new();
            let mut waits: Vec<Waiting> = Vec::new();
            // Records each sync the worker starts, with the commits it covers
            let pump = |h: &Harness, attempts: &mut Vec<Attempt>, clock: u64| {
                while h.started.recv_timeout(Duration::from_millis(15)).is_ok() {
                    let running = h.commit.shared.lock().running.expect("a started sync is running");
                    attempts.push(Attempt { id: running.id, mark: running.mark, started: clock, finished: None });
                }
            };
            let observe = |waits: &mut Vec<Waiting>, clock: u64| {
                for w in waits.iter_mut().filter(|w| w.settled.is_none()) {
                    if let Some(outcome) = w.handle.try_result() {
                        w.settled = Some((clock, outcome.is_ok()));
                    }
                }
            };
            for event in &events {
                clock += 1;
                match event {
                    Event::Commit => {
                        h.commit.on_commit();
                        commits += 1;
                    }
                    Event::Wait => waits.push(Waiting { target: commits, created: clock, handle: h.commit.durable(), settled: None }),
                    Event::Finish(ok) => {
                        if let Some(a) = attempts.last_mut().filter(|a| a.finished.is_none()) {
                            h.finish(if *ok { Ok(()) } else { Err(eio()) });
                            h.ended(a.id);
                            a.finished = Some((clock, *ok));
                        }
                    }
                    Event::Advance(ms) => h.advance(Duration::from_millis(*ms)),
                }
                pump(&h, &mut attempts, clock);
                observe(&mut waits, clock);
            }
            // Let every sync through
            for _ in 0..64 {
                clock += 1;
                if let Some(a) = attempts.last_mut().filter(|a| a.finished.is_none()) {
                    h.finish(Ok(()));
                    h.ended(a.id);
                    a.finished = Some((clock, true));
                }
                pump(&h, &mut attempts, clock);
                observe(&mut waits, clock);
                if waits.iter().all(|w| w.settled.is_some()) && attempts.iter().all(|a| a.finished.is_some()) {
                    break;
                }
            }
            for w in &waits {
                let (at, ok) = w.settled.expect("every wait settles once syncs are let through");
                if ok {
                    // Already synced when it began, or a covering sync succeeded while it waited
                    let covered = attempts.iter().any(|a| a.mark >= w.target && a.finished.is_some_and(|(f, good)| good && f >= w.created && f <= at));
                    let already = !attempts.iter().any(|a| a.mark < w.target && a.started <= w.created && a.finished.is_none_or(|(f, _)| f > w.created))
                        && attempts.iter().filter(|a| a.finished.is_some_and(|(f, good)| good && f <= w.created)).any(|a| a.mark >= w.target);
                    prop_assert!(covered || already || w.target == 0, "a wait for {} resolved with no sync covering it", w.target);
                    let failed_before = attempts.iter().any(|a| a.finished.is_some_and(|(f, good)| !good && f <= at));
                    prop_assert!(!failed_before, "a wait for {} resolved after a sync failed", w.target);
                } else {
                    let failed = attempts.iter().any(|a| a.finished.is_some_and(|(f, good)| !good && f <= at));
                    prop_assert!(failed, "a wait for {} rejected with no sync failing before", w.target);
                }
            }
            let last = h.commit.durable();
            for _ in 0..8 {
                if h.started.recv_timeout(Duration::from_millis(15)).is_ok() {
                    h.finish(Ok(()));
                }
            }
            let failed = attempts.iter().any(|a| a.finished.is_some_and(|(_, good)| !good));
            prop_assert_eq!(outcome(&last), if failed { "rejected" } else { "resolved" });
            if failed { h.close_after_failure() } else { h.close() }
        }
    }
}
