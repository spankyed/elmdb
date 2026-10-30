//! Group commit (`docs/architecture/engine.md`, Durability): a unit commits without a sync, and a worker thread syncs once writes
//! pause for the idle time, and at the latest the cap after the first unsynced commit, so a steady stream of writes
//! can't hold a sync off. `durable()` waits for a sync that began after the last commit, and fails when that sync does.
//!
//! A failed sync is final for the open database: an OS may mark the pages it couldn't write clean and drop them, so a
//! later sync that succeeds proves nothing about them, and every later commit is built on them. Every wait after a
//! failure fails with it, until the database is reopened.
//!
//! When a sync is due is `Policy`'s, a pure function of the instants it is told; the clock and the flush are behind
//! `Clock` and `Syncer`, so a test drives both.

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// When a sync is due: the idle time after the last commit, or the cap after the first unsynced one, whichever is first
#[derive(Debug, Clone)]
pub struct Policy {
    idle: Duration,
    cap: Duration,
    first_unsynced: Option<Instant>,
    last_commit: Option<Instant>,
}

impl Policy {
    pub fn new(idle: Duration, cap: Duration) -> Self {
        Self { idle, cap, first_unsynced: None, last_commit: None }
    }

    pub fn on_commit(&mut self, now: Instant) {
        self.first_unsynced.get_or_insert(now);
        self.last_commit = Some(now);
    }

    /// A sync began: the commits before it are its, and the next commit starts the timers again
    pub fn on_sync_started(&mut self) {
        self.first_unsynced = None;
        self.last_commit = None;
    }

    /// When the next sync is due, or `None` while nothing was committed since the last began
    pub fn due_at(&self) -> Option<Instant> {
        Some((self.last_commit? + self.idle).min(self.first_unsynced? + self.cap))
    }
}

/// What makes the environment's commits durable
pub trait Syncer: Send + Sync + 'static {
    fn sync(&self) -> Result<()>;
}

/// What the worker reads time from
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Instant;
    /// Whether time moves only when a test says: the worker then waits to be woken rather than for a deadline
    fn is_manual(&self) -> bool {
        false
    }
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Debug, Clone, Copy)]
struct Attempt {
    id: u64,
    /// The commits it covers: every one made before it began
    mark: u64,
}

struct State {
    /// Commits made, ever
    committed: u64,
    /// Commits known to be on disk
    synced: u64,
    running: Option<Attempt>,
    /// The attempt that runs once the running one ends, which a `durable()` asked for
    queued: Option<u64>,
    next_id: u64,
    /// Every attempt with an id up to this one has ended
    finished_through: u64,
    /// The attempts somebody waits on, with how many wait, and what each ended with once it has
    waiting: HashMap<u64, (usize, Option<std::result::Result<(), Error>>)>,
    policy: Policy,
    closing: bool,
    /// Attempts that ran, failed ones included
    syncs: u64,
    /// The first sync that failed, which every wait after it fails with
    failed: Option<Error>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    /// Taken out when the worker stops, so a `Durable` outliving its database holds nothing of the environment
    syncer: Mutex<Option<Box<dyn Syncer>>>,
    clock: Box<dyn Clock>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("the sync state's lock is never poisoned")
    }
}

/// The group-commit worker of one open database
pub struct GroupCommit {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

/// A wait for a sync. Resolved already when nothing was owed
pub struct Durable {
    shared: Option<Arc<Shared>>,
    id: u64,
    /// The outcome of a wait settled before it began: an earlier sync's failure
    settled: Option<Error>,
}

impl GroupCommit {
    pub fn start(policy: Policy, syncer: Box<dyn Syncer>, clock: Box<dyn Clock>) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                committed: 0,
                synced: 0,
                running: None,
                queued: None,
                next_id: 1,
                finished_through: 0,
                waiting: HashMap::new(),
                policy,
                closing: false,
                syncs: 0,
                failed: None,
            }),
            changed: Condvar::new(),
            syncer: Mutex::new(Some(syncer)),
            clock,
        });
        let worker = {
            let shared = Arc::clone(&shared);
            std::thread::Builder::new()
                .name("elmdb-sync".to_owned())
                .spawn(move || run(&shared))
                .expect("the sync thread starts")
        };
        Self { shared, worker: Some(worker) }
    }

    /// A unit committed
    pub fn on_commit(&self) {
        let mut state = self.shared.lock();
        state.committed += 1;
        let now = self.shared.clock.now();
        state.policy.on_commit(now);
        self.shared.changed.notify_all();
    }

    /// Waits for every unit committed before the call to be on disk; see the module's comment
    pub fn durable(&self) -> Durable {
        let mut state = self.shared.lock();
        if let Some(failed) = &state.failed {
            return Durable::failed(failed);
        }
        let target = state.committed;
        if state.synced >= target {
            return Durable::resolved();
        }
        let id = match state.running {
            // A sync that began after the last commit covers it
            Some(running) if running.mark >= target => running.id,
            _ => match state.queued {
                Some(id) => id,
                None => {
                    let id = state.next_id;
                    state.next_id += 1;
                    state.queued = Some(id);
                    id
                }
            },
        };
        state.waiting.entry(id).or_insert((0, None)).0 += 1;
        self.shared.changed.notify_all();
        Durable { shared: Some(Arc::clone(&self.shared)), id, settled: None }
    }

    /// Attempts that ran, failed ones included
    pub fn syncs(&self) -> u64 {
        self.shared.lock().syncs
    }

    /// Wakes the worker to read the clock again; a test calls it when it moves a manual clock
    #[cfg(test)]
    pub fn wake(&self) {
        let _state = self.shared.lock();
        self.shared.changed.notify_all();
    }

    /// Syncs what is owed and stops the worker
    pub fn close(&mut self) -> Result<()> {
        let result = self.durable().wait();
        self.shared.lock().closing = true;
        self.shared.changed.notify_all();
        if let Some(worker) = self.worker.take() {
            worker.join().expect("the sync thread doesn't panic");
        }
        self.shared.syncer.lock().expect("the syncer's lock is never poisoned").take();
        result
    }
}

impl GroupCommit {
    /// Stops group commit in a process that is exiting, waiting on no other thread: on Windows the exit handlers run
    /// after every other thread was ended, the worker among them, so neither a sync asked of it nor a join would return.
    /// The caller syncs what is owed on its own thread. The worker is told to stop and left to end with the process,
    /// and the syncer dropped if its lock comes free within `wait`, which a worker ended mid-sync holds for good
    pub fn stop_at_exit(mut self, wait: Duration) {
        let deadline = Instant::now() + wait;
        if let Some(mut state) = lock_before(&self.shared.state, deadline) {
            state.closing = true;
            self.shared.changed.notify_all();
        }
        // Dropping the handle detaches the worker, and leaves `Drop` nothing to close
        self.worker.take();
        if let Some(mut syncer) = lock_before(&self.shared.syncer, deadline) {
            syncer.take();
        }
    }
}

/// The lock, unless another thread holds it past `deadline`
pub(super) fn lock_before<T>(mutex: &Mutex<T>, deadline: Instant) -> Option<MutexGuard<'_, T>> {
    loop {
        match mutex.try_lock() {
            Ok(guard) => return Some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if Instant::now() >= deadline => return None,
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(1)),
        }
    }
}

impl Drop for GroupCommit {
    fn drop(&mut self) {
        if self.worker.is_some() {
            // A failed sync at close has nobody to tell; the commits it couldn't flush are the OS's to write
            let _ = self.close();
        }
    }
}

impl Durable {
    /// A wait with nothing to wait for
    pub fn resolved() -> Self {
        Self { shared: None, id: 0, settled: None }
    }

    /// A wait that fails with an earlier sync's failure
    fn failed(earlier: &Error) -> Self {
        Self { shared: None, id: 0, settled: Some(after_failure(earlier)) }
    }

    /// Blocks until the sync this waits on ends, and gives its outcome
    pub fn wait(mut self) -> Result<()> {
        let Some(shared) = self.shared.take() else { return self.settled.take().map_or(Ok(()), Err) };
        let mut state = shared.lock();
        while state.finished_through < self.id {
            state = shared.changed.wait(state).expect("the sync state's lock is never poisoned");
        }
        release(&mut state, self.id)
    }

    /// The outcome if the sync has ended, without blocking or giving up the wait
    pub fn try_result(&self) -> Option<Result<()>> {
        let Some(shared) = &self.shared else { return Some(self.settled.clone().map_or(Ok(()), Err)) };
        let state = shared.lock();
        (state.finished_through >= self.id)
            .then(|| state.waiting.get(&self.id).and_then(|(_, r)| r.clone()).unwrap_or(Ok(())))
    }
}

impl Drop for Durable {
    /// A wait given up on releases its place, so an attempt's outcome isn't kept for nobody
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            let _ = release(&mut shared.lock(), self.id);
        }
    }
}

/// What a wait after a failed sync fails with
fn after_failure(earlier: &Error) -> Error {
    Error::new(
        earlier.code,
        format!(
            "An earlier sync failed, so nothing committed since is known to be on disk until the database is reopened: {}",
            earlier.message
        ),
    )
}

/// One waiter's outcome of attempt `id`, forgetting the attempt once its last waiter has gone
fn release(state: &mut State, id: u64) -> Result<()> {
    let Some((count, outcome)) = state.waiting.get_mut(&id) else { return Ok(()) };
    let outcome = outcome.clone().unwrap_or(Ok(()));
    *count -= 1;
    if *count == 0 {
        state.waiting.remove(&id);
    }
    outcome
}

fn run(shared: &Shared) {
    loop {
        let attempt = {
            let mut state = shared.lock();
            loop {
                if state.running.is_none() {
                    if let Some(id) = state.queued.take() {
                        break begin(&mut state, id);
                    }
                    let owed = state.committed > state.synced;
                    let due = state.policy.due_at();
                    if owed && due.is_some_and(|due| due <= shared.clock.now()) {
                        let id = state.next_id;
                        state.next_id += 1;
                        break begin(&mut state, id);
                    }
                    if state.closing {
                        return;
                    }
                }
                // Woken by a commit, a `durable()`, a test moving a manual clock, or the deadline
                state = match state.policy.due_at() {
                    Some(due) if !shared.clock.is_manual() => {
                        let wait = due.saturating_duration_since(shared.clock.now()).max(Duration::from_millis(1));
                        shared.changed.wait_timeout(state, wait).expect("the sync state's lock is never poisoned").0
                    }
                    _ => shared.changed.wait(state).expect("the sync state's lock is never poisoned"),
                };
            }
        };
        let outcome = match &*shared.syncer.lock().expect("the syncer's lock is never poisoned") {
            Some(syncer) => syncer.sync(),
            None => Ok(()),
        };
        let mut state = shared.lock();
        state.running = None;
        state.finished_through = attempt.id;
        state.syncs += 1;
        let outcome = match (outcome, &state.failed) {
            (Ok(()), None) => {
                state.synced = state.synced.max(attempt.mark);
                Ok(())
            }
            (Ok(()), Some(failed)) => Err(after_failure(failed)),
            (Err(e), _) => {
                state.failed.get_or_insert_with(|| e.clone());
                Err(e)
            }
        };
        if let Some((_, slot)) = state.waiting.get_mut(&attempt.id) {
            *slot = Some(outcome);
        }
        shared.changed.notify_all();
    }
}

fn begin(state: &mut State, id: u64) -> Attempt {
    let attempt = Attempt { id, mark: state.committed };
    state.running = Some(attempt);
    state.policy.on_sync_started();
    attempt
}

#[cfg(test)]
mod tests;
