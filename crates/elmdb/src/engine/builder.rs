//! Building indexes in the background: a thread per open database takes the unbuilt indexes one at a time, in chunks of
//! `CHUNK` entities, each chunk a unit of its own, so the app's writes go on between them. Writes keep every index
//! whether built or not, so a build only fills in the entities that predate it. Until an index is built, a plan walks
//! the type's rows instead.
//!
//! A rank index is built and kept here too: its nodes are added a chunk at a time, then pushed a chunk of `CHUNK`
//! pushes at a time until none waits, which is also how the pushes a commit leaves over its budget are drained. A
//! build's chunks and a drain's commit like any other write, telling the watches and the tracked reads whose reads
//! they wrote into.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::database::EngineSchema;
use crate::error::ErrorCode;
use crate::store::Store;
use crate::store::sync::{Clock, SystemClock};
use crate::sync::{Condvar, Mutex};

use super::derive::build_chunk;
use super::unit::Txn;

crate::sync::thread_local! {
    /// Address of the `Core` currently executing on this thread, if any.
    /// Destructor-free (`const`-initialized `Cell`), so `on_its_thread()` and `join()` never panic
    /// during thread-local teardown after `std::thread::current()` TLS has already been destroyed.
    static ACTIVE_BUILDER: Cell<usize> = const { Cell::new(0) };
}

#[cfg(all(test, shuttle))]
mod model;

/// How many entities one unit of a build covers
const CHUNK: usize = 1000;

/// How long a rank index's drain waits after the last commit that left it pushes, so that a burst of commits shares
/// one drain: each drain reads and writes back the nodes its pushes reach, which over a graph whose links are spread
/// is nearly all of them, however few the commits before it changed. Reads go on meanwhile, reporting the bound on their scores as it stands
const QUIET: Duration = Duration::from_millis(5);

/// The longest a drain waits for commits to pause, so a steady stream of them is still drained
const PATIENCE: Duration = Duration::from_millis(100);

/// How many more times a drain computed outside the write lock is computed there again, after the commits that made it
/// stale have paused, before it is computed under the lock: a burst usually ends within them, and under the lock every
/// write waits for the whole drain. More don't lower a write's latency further and none leaves scores staler for longer
/// (`docs/evidence/rank-index.md`, Cutting how long a drain holds the lock)
const STALE_RETRIES: u32 = 2;

/// How many pushes one unit of a rank index's drain makes in memory, about 10 ms a million (2026-10-31, Apple M1 Pro,
/// release), so a drain finishes in one unit, writing each node it moved once, rather than in several each writing
/// them again
const DRAIN_PUSHES: usize = 20_000_000;

/// How long the second build this builder begins of one index's order waits, and twice as long each one after:
/// a build is begun again when a write of another elmdb's ended the last, and two elmdbs that write one database in
/// turn would otherwise each build its orders at every turn
const ORDER_PUT_OFF: Duration = Duration::from_secs(1);

/// The longest a build of an order is put off
const ORDER_PUT_OFF_MOST: Duration = Duration::from_secs(600);

/// What the thread does: build a type's index, build the order a value index keeps of its strings, or build or
/// drain a rank index
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Work {
    Index { entity_type: String, index: String },
    Order { entity_type: String, index: String },
    Rank { name: String },
}

/// An index being built, or that failed to build
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexFailure {
    pub work: Work,
    pub message: String,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Work>,
    busy: bool,
    active: Option<Work>,
    stopping: bool,
    failures: Vec<IndexFailure>,
    /// When work was last queued for each rank index: the commit its drain waits to be the last of a burst
    queued_at: HashMap<String, Instant>,
    /// Work put off, each piece with when it is due
    later: Vec<(Instant, Work)>,
    /// The orders this builder has begun a build of: how long the next build it begins of each is put off, and
    /// whether that wait is over
    begun: HashMap<Work, (Duration, bool)>,
    /// What holds the builds up while the builder waits it out, nothing of its work made: a sync behind
    /// (`SYNC_BEHIND`) or a full reader table, which a wait for the builds answers with rather than waiting on
    held: Option<crate::Error>,
    /// How many times what the builder builds to was replaced: its schema, by a schema change. A piece of work is
    /// taken in a generation, and one that ends in another is let go of, neither done nor failed
    generation: u64,
    /// The changes begun and not ended (`begin_change`): what was queued is gone, and what the change leaves to
    /// build isn't queued yet, so a wait for the builds waits for the change's end
    changing: usize,
    /// The closes that have told the thread to stop and haven't said whether the database closed (`begin_close`):
    /// a wait for the builds waits for each to say, and work queued meanwhile waits in the queue for a thread. A
    /// count, since one close may begin as another gives up
    closing: usize,
    /// The database closed, or its builder was stopped for good: nothing more is built, and no one waits
    closed: bool,
    /// A thread runs this building: claimed by `claim_start` before it is started, given up by the thread as it
    /// ends. One at a time, whoever asks
    running: bool,
}

/// How a wait for the builds ended (`Core::waited`)
pub enum Waited {
    /// Nothing is queued or under way and no change or close is being made, or the builder is closed
    Idle,
    /// The builder waits out what refused its work with nothing made
    Held(crate::Error),
    /// A close is under way, which the caller said it must not wait for
    Closing,
    /// Work is queued with no thread to take it and none coming: the caller starts one
    Unrun,
}

/// What the thread decides, apart from what doing a piece of work means: its queue, the pause a rank index's drain
/// waits for, stopping, and the waiters. The builder's thread runs it over the store, and the model
/// (`builder/model.rs`) over a stand-in, on a clock it moves
struct Core {
    state: Mutex<State>,
    changed: Condvar,
    clock: Box<dyn Clock>,
}

/// What holds of the state whenever its lock is let go, checked in a debug build after each change of it: the counts
/// and flags are each other's, and what a thread runs it has claimed
fn holds(state: &State) {
    debug_assert!(state.active.is_none() || state.busy, "work under way with the builder said to be idle");
    debug_assert!(state.active.is_none() || state.running, "work under way with no thread claimed");
    debug_assert!(!state.closed || state.stopping, "closed and not stopping");
    debug_assert!(state.closing == 0 || state.stopping, "a close under way and the thread not told to stop");
    debug_assert!(
        state.queue.iter().enumerate().all(|(at, work)| !state.queue.iter().skip(at + 1).any(|other| other == work)),
        "a piece of work queued twice"
    );
}

impl Core {
    fn new(clock: Box<dyn Clock>) -> Self {
        Self { state: Mutex::new(State::default()), changed: Condvar::new(), clock }
    }

    fn lock(&self) -> crate::sync::MutexGuard<'_, State> {
        self.state.lock().expect("the builder's lock is never poisoned")
    }

    fn queue(&self, indexes: impl IntoIterator<Item = Work>) {
        let mut state = self.lock();
        for index in indexes {
            if let Work::Rank { name } = &index {
                state.queued_at.insert(name.clone(), self.clock.now());
            }
            if !state.queue.contains(&index) {
                state.queue.push_back(index);
            }
        }
        holds(&state);
        drop(state);
        self.changed.notify_all();
    }

    /// Claims the starting of a thread, where one is needed: there is work and no thread runs, and no close is
    /// under way or has closed the database. Whoever is answered true starts it, and no one else does until it ends
    fn claim_start(&self) -> bool {
        let mut state = self.lock();
        let needed = !state.running
            && state.closing == 0
            && !state.closed
            && (!state.queue.is_empty() || !state.later.is_empty());
        if needed {
            state.running = true;
        }
        holds(&state);
        needed
    }

    /// Claims a thread for a test that runs the core on one of its own, as `claim_start` claims one for the
    /// building's
    #[cfg(test)]
    fn claimed(&self) -> &Self {
        self.lock().running = true;
        self
    }

    /// Gives up a claim `claim_start` answered true for, its thread not started
    fn unclaim(&self) {
        let mut state = self.lock();
        state.running = false;
        holds(&state);
        drop(state);
        self.changed.notify_all();
    }

    /// Whether a thread runs, or is claimed and about to
    fn running(&self) -> bool {
        self.lock().running
    }

    /// Whether the caller is the thread that runs this
    fn on_its_thread(&self) -> bool {
        ACTIVE_BUILDER.with(Cell::get) == (self as *const Self as usize)
    }

    /// Whether `work` is put off (`put_off`): not waited for (`waited`)
    fn put_off_now(&self, work: &Work) -> bool {
        self.lock().later.iter().any(|(_, put_off)| put_off == work)
    }

    /// Lists `work` as failed with `message`, in place of a failure it had
    fn fail(&self, work: Work, message: String) {
        let mut state = self.lock();
        state.failures.retain(|failure| failure.work != work);
        state.failures.push(IndexFailure { work, message });
        holds(&state);
    }

    /// Whether the building is stopped for good: nothing more is built
    fn closed(&self) -> bool {
        self.lock().closed
    }

    /// Stops the builder for good: its thread ends once the piece of work it is on has, and no one waits for it
    fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.stopping = true;
        state.closed = true;
        holds(&state);
        drop(state);
        self.changed.notify_all();
    }

    /// That a close is under way: the thread is told to stop, as by `stop`, and whoever waits for the builds waits
    /// for `end_close` to say whether the database closed. Work queued from here stays queued
    fn begin_close(&self) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.stopping = true;
        state.closing += 1;
        holds(&state);
        drop(state);
        self.changed.notify_all();
    }

    /// That a close `begin_close` began closed the database, or gave up and left it open: closed, the builder is
    /// stopped for good; left open, with no other close under way, a thread may be started again for what is queued
    fn end_close(&self, closed: bool) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closing = state.closing.saturating_sub(1);
        if closed {
            state.closed = true;
        } else if !state.closed && state.closing == 0 {
            state.stopping = false;
        }
        holds(&state);
        drop(state);
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn stopping(&self) -> bool {
        self.lock().stopping
    }

    /// Whether a thread may not be started: a close is under way, or the builder is stopped for good
    #[cfg(test)]
    fn closing(&self) -> bool {
        let state = self.lock();
        state.closing > 0 || state.closed
    }

    /// Whether anything is queued or put off
    fn has_work(&self) -> bool {
        let state = self.lock();
        !state.queue.is_empty() || !state.later.is_empty()
    }

    /// That what the builder builds to is about to be replaced. What is queued or put off was for what it built to
    /// until now, and goes; the piece of work the thread is on is told to stop, and is let go of when it ends, a
    /// rank index's that was waiting for its commits to pause among them; and the thread takes no work from the
    /// queue until every change begun has ended, since what it would build it to is not yet what the change leaves. Whoever waits for the builds waits until `end_change`. Answers the work that goes:
    /// what a change that wasn't made, or can't say what is left to build, queues again
    fn begin_change(&self) -> Vec<Work> {
        let mut state = self.lock();
        state.generation += 1;
        state.changing += 1;
        let mut left: Vec<Work> = state.active.iter().cloned().collect();
        left.extend(std::mem::take(&mut state.queue));
        left.extend(std::mem::take(&mut state.later).into_iter().map(|(_, work)| work));
        holds(&state);
        drop(state);
        self.changed.notify_all();
        left
    }

    /// That a change `begin_change` began has ended: `work` is what there is to build now, queued with whatever
    /// was queued since the change began, a commit's rank index among it. One that was `made` leaves no build
    /// failed: what failed was for what the builder built to before, and is among `work` if it is still to build.
    /// One that wasn't keeps what had failed, which is as true as it was
    fn end_change(&self, work: impl IntoIterator<Item = Work>, made: bool) {
        self.queue(work);
        let mut state = self.lock();
        if made {
            state.failures.clear();
        }
        state.changing = state.changing.checked_sub(1).expect("a change ended was begun");
        holds(&state);
        drop(state);
        self.changed.notify_all();
    }

    /// The generation as it stands: what a pause within a piece of work is held to
    fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Whether a piece of work taken in `generation` is to stop: the builder stops, or builds to something else
    fn over(&self, generation: u64) -> bool {
        let state = self.lock();
        state.stopping || state.generation != generation
    }

    /// Blocks until every queued index is built or has failed, or the builder stops, or is held up by what it waits
    /// out, which this answers. Work put off (`later`) is not waited for, by decision: it is the build of an order
    /// that another elmdb's writes keep ending, begun again after a wait of up to `ORDER_PUT_OFF_MOST`, and may be
    /// ended again as often. Its index is built and every read of it answers, sorting where it would have read the
    /// order, so nothing a caller waits for the builds to have is missing, and waiting would hold it for minutes, or
    /// for as long as the other elmdb writes
    #[cfg(test)]
    fn wait(&self) -> Option<crate::Error> {
        match self.waited(true) {
            Waited::Held(held) => Some(held),
            Waited::Idle | Waited::Closing | Waited::Unrun => None,
        }
    }

    /// `wait`, saying how it ended. A close under way is waited through, to its closing the database or giving up
    /// and leaving it open with what is left queued again, unless `through_a_close` is false: a caller the close
    /// may itself be waiting for, a listener, is told of it instead
    fn waited(&self, through_a_close: bool) -> Waited {
        let mut state = self.lock();
        // Work queued with no thread, and none to come of a change or a close under way, is waited for by no one:
        // the caller starts a thread for it, or says it is left (`Database::wait_for_index_builds`)
        while (state.changing > 0 || state.closing > 0 || (state.running && (!state.queue.is_empty() || state.busy)))
            && !state.closed
            && state.held.is_none()
        {
            if state.closing > 0 && !through_a_close {
                return Waited::Closing;
            }
            state = self.changed.wait(state).expect("the builder's lock is never poisoned");
        }
        match state.held.clone() {
            Some(held) if !state.closed => Waited::Held(held),
            _ if !state.closed && !state.running && !state.queue.is_empty() => Waited::Unrun,
            _ => Waited::Idle,
        }
    }

    /// What holds the builds up, or that nothing does
    fn hold(&self, held: Option<crate::Error>) {
        self.lock().held = held;
        self.changed.notify_all();
    }

    /// Takes the queue's work one piece at a time until stopped, `build` doing each, given whether to stop. Run by
    /// the thread `claim_start` claimed, which it gives up as it ends
    fn run(&self, mut build: impl FnMut(&Work, &dyn Fn() -> bool) -> crate::Result<()>) {
        let previous_active = ACTIVE_BUILDER.with(|cell| cell.replace(self as *const Self as usize));
        debug_assert!(self.lock().running, "a thread runs the building that wasn't claimed");
        // Ends idle, and given up, whichever way it ends, a panic outside a build's own among them: a thread started
        // after it finds no work under way, and may be started at all, and no wait is left waiting on this one
        struct Ending<'a> {
            core: &'a Core,
            previous_active: usize,
        }
        impl Drop for Ending<'_> {
            fn drop(&mut self) {
                ACTIVE_BUILDER.with(|cell| cell.set(self.previous_active));
                let mut state = self.core.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                state.busy = false;
                state.active = None;
                state.running = false;
                holds(&state);
                drop(state);
                self.core.changed.notify_all();
            }
        }
        let _ending = Ending { core: self, previous_active };
        let ended = drop::<crate::sync::MutexGuard<'_, State>>;
        loop {
            let (work, generation) = {
                let mut state = self.lock();
                loop {
                    if state.stopping {
                        return ended(state);
                    }
                    // What was put off until now is work again, and may begin
                    let now = self.clock.now();
                    let (due, later) = std::mem::take(&mut state.later).into_iter().partition(|(at, _)| *at <= now);
                    state.later = later;
                    for (_, work) in due as Vec<(Instant, Work)> {
                        if let Some((_, over)) = state.begun.get_mut(&work) {
                            *over = true;
                        }
                        if !state.queue.contains(&work) {
                            state.queue.push_back(work);
                        }
                    }
                    // None while a change is being made: what the thread would build it to is the change's to say
                    if state.changing == 0
                        && let Some(next) = state.queue.pop_front()
                    {
                        state.busy = true;
                        state.active = Some(next.clone());
                        break (next, state.generation);
                    }
                    state.busy = false;
                    holds(&state);
                    self.changed.notify_all();
                    // Asleep until work is queued, or until the first of what was put off is due
                    let first = state.later.iter().map(|(at, _)| *at).min().filter(|_| !self.clock.is_manual());
                    state = match first {
                        Some(at) => {
                            let wait = at.saturating_duration_since(now);
                            self.changed.wait_timeout(state, wait).expect("the builder's lock is never poisoned").0
                        }
                        None => self.changed.wait(state).expect("the builder's lock is never poisoned"),
                    };
                }
            };
            if let Work::Rank { name } = &work {
                self.await_quiet(name, generation);
            }
            // A build that panics is that work's failure, as one that answers an error is: the thread goes on to the
            // next, where the panic would end it with the work under way, and a close would meet the panic
            let built = catch_unwind(AssertUnwindSafe(|| build(&work, &|| self.over(generation))))
                .unwrap_or_else(|panic| Err(crate::Error::new(ErrorCode::IndexBuildFailed, panicked(&*panic))));
            let mut state = self.lock();
            // What held the build up is over with it, however it ended: a panic while it waited one out among them
            state.held = None;
            // Work over rows another process has replaced isn't owed any more: adopting that queues what is. Nor is
            // work for what the builder built to when it was taken: what it failed on may be what replaced that
            let current = state.generation == generation;
            let ended_ok = built.is_ok();
            match built {
                Err(e) if e.code != ErrorCode::DataReplaced && current => {
                    // The work's one failure: a build that fails again says why it failed last
                    state.failures.retain(|failure| failure.work != work);
                    state.failures.push(IndexFailure { work: work.clone(), message: e.message });
                }
                // Built, it is failed no longer: a thread started after one that failed it has the same list. One
                // told to stop answers `Ok` with the build part made, and says nothing of it
                Ok(()) if current && !state.stopping => state.failures.retain(|failure| failure.work != work),
                _ => {}
            }
            // Told to stop as it ended, the work may be part made: it goes back to the head of the queue, for the
            // thread after this one. Any of it is done again to no effect where it was finished, and work a change
            // took isn't the building's to keep
            if state.stopping
                && current
                && ended_ok
                && !state.queue.contains(&work)
                && !state.later.iter().any(|(_, put_off)| *put_off == work)
            {
                state.queue.push_front(work.clone());
            }
            state.active = None;
            // mutant-skip: replace && with \|\| in Core::run
            // Either busy with work queued; where they differ the thread goes on, and its next turn sets it from the
            // queue before it sleeps, a waiter seeing it busy only in between
            state.busy = !state.stopping && !state.queue.is_empty();
            if state.stopping {
                return ended(state);
            }
            holds(&state);
            drop(state);
            self.changed.notify_all();
        }
    }

    /// Whether a build of an order may begin now: the first this builder begins of it, or one whose wait is over
    fn may_begin(&self, work: &Work) -> bool {
        self.lock().begun.get(work).is_none_or(|(_, over)| *over)
    }

    /// That a build of an order began: the next this builder begins of it waits, twice as long as the last did
    fn began(&self, work: &Work) {
        let mut state = self.lock();
        let next = match state.begun.get(work) {
            Some((wait, _)) => (*wait * 2).min(ORDER_PUT_OFF_MOST),
            None => ORDER_PUT_OFF,
        };
        state.begun.insert(work.clone(), (next, false));
    }

    /// Puts a build of an order off for as long as its next is to wait
    fn put_off(&self, work: &Work) {
        let mut state = self.lock();
        let wait = state.begun.get(work).map_or(ORDER_PUT_OFF, |(wait, _)| *wait);
        let due = self.clock.now() + wait;
        if !state.later.iter().any(|(_, put_off)| put_off == work) {
            state.later.push((due, work.clone()));
        }
        self.changed.notify_all();
    }

    /// Waits until no commit has left a rank index pushes for `QUIET`, at most `PATIENCE`, or the builder stops or
    /// builds to something other than it did in `generation`. On a clock that moves only when a test moves it, the
    /// wait is for a wake: a queue, a stop, a change, or the test's tick
    fn await_quiet(&self, name: &str, generation: u64) {
        let started = self.clock.now();
        let mut state = self.lock();
        loop {
            let now = self.clock.now();
            if state.stopping || state.generation != generation || now.saturating_duration_since(started) >= PATIENCE {
                return;
            }
            let since = state.queued_at.get(name).map_or(QUIET, |at| now.saturating_duration_since(*at));
            if since >= QUIET {
                return;
            }
            state = if self.clock.is_manual() {
                self.changed.wait(state).expect("the builder's lock is never poisoned")
            } else {
                self.changed.wait_timeout(state, QUIET - since).expect("the builder's lock is never poisoned").0
            };
        }
    }
}

/// What a panic said, for the failure of the build it ended
fn panicked(panic: &(dyn std::any::Any + Send)) -> String {
    let said = panic
        .downcast_ref::<&str>()
        .map(|said| (*said).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a value that isn't text".to_owned());
    format!("the build panicked, saying: {said}")
}

/// How a build's chunk and a drain's commit: the database's own commit, which delivers to the watches it wrote into
pub type Commits =
    Arc<dyn Fn(&mut dyn FnMut(&mut Txn<'_, '_>) -> crate::Result<bool>) -> crate::Result<bool> + Send + Sync>;

/// What the builder builds to: the store, the schema, and how its chunks commit under it
pub type Target = (Arc<Store>, Arc<EngineSchema>, Commits);

/// A database's index building, from its open until it is dropped: the queue, what waits for it, what it builds to,
/// and the thread that runs it. The thread is started with the first work there is, stopped by a close, and started
/// again where the close gave up and left the database open, each by the building: `start` starts one only where
/// its core says one is needed (`Core::claim_start`), so no two run, and none runs during a close
#[derive(Clone)]
pub struct Building {
    core: Arc<Core>,
    /// What the thread takes each piece of work with: set as a thread is started where none is, replaced by a schema
    /// change (`end_change`), and let go of by a close, since it holds the store
    target: Arc<Mutex<Option<Target>>>,
    /// The thread, from its start until a close waits for it or lets go of it
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Default for Building {
    fn default() -> Self {
        Self {
            core: Arc::new(Core::new(Box::new(SystemClock))),
            target: Arc::new(Mutex::new(None)),
            thread: Arc::new(Mutex::new(None)),
        }
    }
}

impl Building {
    fn target(&self) -> crate::sync::MutexGuard<'_, Option<Target>> {
        self.target.lock().expect("the builder's target is never poisoned")
    }

    fn handle(&self) -> crate::sync::MutexGuard<'_, Option<JoinHandle<()>>> {
        self.thread.lock().expect("the builder's thread's lock is never poisoned")
    }

    /// Starts the thread where one is needed: there is work, no thread runs, and no close is under way. What it
    /// builds to is `target`'s, asked only where nothing is built to yet. The caller holds the schema for reading or
    /// writing, which a close takes for writing to begin, so no close begins as this starts one.
    ///
    /// Refused where the system won't start a thread (`THREAD_REFUSED`): the claim is given up, the work stays
    /// queued, and the next to ask starts one
    pub fn start(&self, target: impl FnOnce() -> Target) -> crate::Result<()> {
        let mut handle = self.handle();
        if !self.core.claim_start() {
            return Ok(());
        }
        {
            let mut built_to = self.target();
            if built_to.is_none() {
                *built_to = Some(target());
            }
        }
        let core = Arc::clone(&self.core);
        #[cfg(test)]
        let forged = super::order::forged();
        let spawned = {
            let core = Arc::clone(&core);
            let target = Arc::clone(&self.target);
            #[cfg(test)]
            let refused = crate::failpoints::fails_here(crate::failpoints::points::THREAD_SPAWN);
            #[cfg(not(test))]
            let refused = false;
            let builder = std::thread::Builder::new().name("elmdb-index".to_owned());
            if refused {
                Err(std::io::Error::other("injected thread spawn failure"))
            } else {
                builder.spawn(move || {
                    // A stamp forged by the thread that opened the database is this thread's too
                    #[cfg(test)]
                    super::order::forge(forged);
                    core.run(|work, stopping| {
                        // What the builder builds to as the work begins. A schema change replaces it only once it
                        // has told the builder, so work begun to one schema and gone on under another is work the
                        // change has told to stop, and whose commits the store refuses
                        let built_to = target.lock().expect("the builder's target is never poisoned").clone();
                        // Let go of by a close, which has told this thread to stop
                        let Some((store, schema, commits)) = built_to else { return Ok(()) };
                        match work {
                            Work::Index { entity_type, index } => {
                                build_index(&store, &core, &commits, entity_type, index, stopping)?;
                                // A value index just built has an order to build
                                if !stopping() {
                                    core.queue([Work::Order {
                                        entity_type: entity_type.clone(),
                                        index: index.clone(),
                                    }]);
                                }
                                Ok(())
                            }
                            Work::Order { entity_type, index } => {
                                build_order(&store, &schema, &commits, &core, work, entity_type, index, stopping)
                            }
                            Work::Rank { name } => {
                                let taken = core.generation();
                                build_rank(&store, &schema, &core, &commits, name, stopping, &|| {
                                    core.await_quiet(name, taken)
                                })
                            }
                        }
                    });
                })
            }
        };
        let thread = match spawned {
            Ok(thread) => thread,
            Err(refused) => {
                self.core.unclaim();
                return Err(crate::Error::new(
                    ErrorCode::ThreadRefused,
                    format!(
                        "The system refused to start the index builder's thread ({refused}): what is left to build \
                         stays queued, and is started by the next write or wait for the builds"
                    ),
                ));
            }
        };
        // One that ended before this was started was given up already, and is let go of
        *handle = Some(thread);
        Ok(())
    }

    pub fn queue(&self, indexes: impl IntoIterator<Item = Work>) {
        self.core.queue(indexes);
    }

    /// Whether a thread runs, or is about to
    pub fn running(&self) -> bool {
        self.core.running()
    }

    /// Whether the caller is the thread that runs this: a listener one of its commits delivered to
    pub fn on_its_thread(&self) -> bool {
        self.core.on_its_thread()
    }

    /// Whether anything is queued or put off, for a thread to take
    pub fn has_work(&self) -> bool {
        self.core.has_work()
    }

    /// Whether `work` is put off: an order's build that another elmdb's writes keep ending (`Core::waited`)
    pub fn put_off(&self, work: &Work) -> bool {
        self.core.put_off_now(work)
    }

    /// Whether a thread may not be started: a close is under way, or the database closed
    #[cfg(test)]
    pub fn closing(&self) -> bool {
        self.core.closing()
    }

    /// Whether nothing more is built: the database closed, or a close at exit let its thread go
    pub fn closed(&self) -> bool {
        self.core.closed()
    }

    /// Lists `work` as failed, with why: what `indexStatus` and a wait for the builds then say of it
    pub fn fail(&self, work: Work, message: String) {
        self.core.fail(work, message);
    }

    /// That what is built to is about to change, the schema or the rows: the work queued is for what it was, and
    /// goes (`Core::begin_change`). The change ends when what this answers is ended or dropped
    pub fn begin_change(&self) -> Change {
        Change { left: self.core.begin_change(), core: Some(Arc::clone(&self.core)) }
    }

    /// That the schema change `change` is made, or wasn't: what is built to from here is `schema`, committed
    /// through `commits`, and `work` is what that leaves to build. Where nothing is built to yet, the thread
    /// started for `work` is given both
    pub fn end_change(&self, change: Change, schema: Arc<EngineSchema>, commits: Commits, work: Vec<Work>) {
        if let Some(target) = self.target().as_mut() {
            target.1 = schema;
            target.2 = commits;
        }
        change.end(work);
    }

    /// That a close is under way (`Core::begin_close`), until what this answers says the database closed or is
    /// dropped, which says it didn't. The thread is told to stop, and woken where it waits for a unit to end; what
    /// is built to is let go of, with its hold on the store. The thread is waited for, or let go of, by `join`
    #[must_use = "a close ends when this is dropped, or says the database closed"]
    pub fn begin_close(&self) -> Closing {
        self.core.begin_close();
        let built_to = self.target().take();
        if let Some((store, _, _)) = built_to {
            store.wake_unit_waiters();
        }
        Closing(Some(self.clone()))
    }

    /// Waits for the thread a close told to stop (`begin_close`) to end, leaving an index part built for the thread
    /// after it or the next open; or, `at_exit`, lets go of it unwaited for, in a process whose other threads may be
    /// gone already. Its caller holds nothing the thread may wait for: a chunk's commit delivers to the watches it
    /// wrote into, and a watch's read, and whatever its listener calls of the database, takes the schema's lock.
    /// Called on the thread itself it waits for nothing, the thread ending once what called this returns
    pub fn join(&self, at_exit: bool) {
        let thread = self.handle().take();
        if let Some(thread) = thread
            && !at_exit
            && !self.core.on_its_thread()
        {
            thread.join().expect("the index thread doesn't panic");
        }
    }

    /// Stops the building for good and waits for its thread: a database dropped where its close was refused, or
    /// one that closed, for which it does nothing more
    pub fn stop(&self) {
        self.core.stop();
        let built_to = self.target().take();
        if let Some((store, _, _)) = built_to {
            store.wake_unit_waiters();
        }
        self.join(false);
    }

    /// Stops the thread and waits for it, leaving the building as a close that gave up leaves it, with what is
    /// queued kept and no thread: how a test has work wait for one
    #[cfg(test)]
    pub fn pause(&self) {
        let closing = self.begin_close();
        self.join(false);
        drop(closing);
    }

    /// Blocks until every queued index is built or has failed, saying how the wait ended (`Core::waited`)
    pub fn wait(&self, through_a_close: bool) -> Waited {
        self.core.waited(through_a_close)
    }

    /// The work there is: under way, queued and put off
    #[cfg(test)]
    pub fn pending(&self) -> Vec<Work> {
        let state = self.core.lock();
        let put_off = state.later.iter().map(|(_, work)| work);
        state.active.iter().chain(state.queue.iter()).chain(put_off).cloned().collect()
    }

    pub fn failures(&self) -> Vec<IndexFailure> {
        self.core.lock().failures.clone()
    }
}

/// A close under way (`Building::begin_close`). Dropped, a close that gave up, was refused or panicked, it says the
/// database was left open, so no wait for the builds is left waiting for it
#[must_use = "a close ends when this is dropped, or says the database closed"]
pub struct Closing(Option<Building>);

impl Closing {
    /// That the close closed the database, or is one made in a process that is exiting, whatever becomes of it:
    /// nothing more is built. Said once; saying it again, or dropping this after, says nothing more
    pub fn closed(&mut self) {
        if let Some(building) = self.0.take() {
            building.core.end_close(true);
        }
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        if let Some(building) = self.0.take() {
            building.core.end_close(false);
        }
    }
}

/// A change to what is built to, begun and not ended (`Building::begin_change`). Dropped without being ended, a
/// failure or a panic part way, it ends as a change that wasn't made, with the work it took queued again, so no wait
/// for the builds is left waiting for it
#[must_use = "a change ends when this is dropped, as one that wasn't made, or when it is ended"]
pub struct Change {
    core: Option<Arc<Core>>,
    left: Vec<Work>,
}

impl Change {
    /// That the change is made, and `work` is what it leaves to build
    pub fn end(mut self, work: Vec<Work>) {
        if let Some(core) = self.core.take() {
            core.end_change(work, true);
        }
    }
}

impl Drop for Change {
    fn drop(&mut self) {
        if let Some(core) = self.core.take() {
            core.end_change(std::mem::take(&mut self.left), false);
        }
    }
}

/// How long the builder pauses, after its work was refused with nothing made by what it waits out, before it asks
/// whether a write would go on: a pause apart, each ask a sync where the pin is past its bound, and the work run
/// again only once one would
const HELD_WAIT: Duration = Duration::from_secs(1);

/// Whether a failure refused the builder's work with nothing of it made, for a cause outside the build that it waits
/// out rather than failing the build, which would stay failed until the database is opened again: a sync behind
/// (`SYNC_BEHIND`), or a full reader table met by a read before the chunk's commit
fn held_up(failure: &crate::Error) -> bool {
    matches!(failure.code, ErrorCode::SyncBehind | ErrorCode::ReadersFull)
}

/// Waits out what refused the builder's work, `refused`, a pause at a time, until `ask` says a write would go on
/// (`Store::hold_to_the_bound`: a sync that succeeds lets one past the bound) or the builder stops; a wait for the
/// builds meanwhile answers what refused it last
fn wait_out(
    ask: &dyn Fn() -> crate::Result<()>,
    core: &Core,
    stopping: &dyn Fn() -> bool,
    refused: crate::Error,
) -> crate::Result<()> {
    core.hold(Some(refused));
    let waited = loop {
        let until = Instant::now() + HELD_WAIT;
        // mutant-skip: replace < with <= in wait_out
        // The two differ only for an instant that lands on the deadline itself, which no test can arrange
        while Instant::now() < until && !stopping() {
            std::thread::sleep(Duration::from_millis(10));
        }
        if stopping() {
            break Ok(());
        }
        match ask() {
            Err(e) if e.code == ErrorCode::SyncBehind => core.hold(Some(e)),
            other => break other,
        }
    };
    core.hold(None);
    waited
}

/// What `attempt` answers, run again after each refusal that made nothing (`held_up`) is waited out; `None` where the
/// builder stops first
fn outlasting<T>(
    store: &Store,
    core: &Core,
    stopping: &dyn Fn() -> bool,
    mut attempt: impl FnMut() -> crate::Result<T>,
) -> crate::Result<Option<T>> {
    loop {
        if stopping() {
            return Ok(None);
        }
        match attempt() {
            Err(e) if held_up(&e) => wait_out(&|| store.hold_to_the_bound(), core, stopping, e)?,
            answered => return answered.map(Some),
        }
    }
}

/// Runs `chunk` as units until it says it's done, waiting out a unit the app holds open, a sync behind and a full
/// reader table; `Ok` once done or stopping
fn chunks(
    store: &Store,
    core: &Core,
    stopping: &dyn Fn() -> bool,
    mut chunk: impl FnMut() -> crate::Result<bool>,
) -> crate::Result<()> {
    loop {
        match outlasting(store, core, stopping, &mut chunk) {
            Ok(None | Some(true)) => return Ok(()),
            Ok(Some(false)) => {}
            // A unit held open by the app has the write transaction: try again once it ends
            Err(e) if e.code == ErrorCode::UnitOpen => store.wait_while_unit_open(stopping),
            Err(e) => return Err(e),
        }
    }
}

/// Builds a type's index over the entities that predate it, a chunk at a time, the last chunk marking it built. A read
/// that had no answer while it was built (`INDEX_BUILDING`, a text index's) read that marker to be refused, so the
/// commit that writes it is the one that tells it to read again. An index found built is left as it is: work is
/// queued by whoever finds it owed, an open, a schema change, a close that gave up, a wait for the builds, and two
/// of them may queue one index
fn build_index(
    store: &Store,
    core: &Core,
    commits: &Commits,
    entity_type: &str,
    index: &str,
    stopping: &dyn Fn() -> bool,
) -> crate::Result<()> {
    chunks(store, core, stopping, || {
        // Asked before each chunk: a built index makes no unit, whose commit would be heard and say nothing. One
        // built since the last chunk was built by a rebuild, a replacement or another process's builder, and is
        // theirs
        if store.read(|txn, db| Ok(db.get(txn, &super::keys::index_built(entity_type, index))?.is_some()))? {
            return Ok(true);
        }
        // Where the build has reached is the store's, written by the chunk that reached it: a chunk refused
        // leaves it where it was, and whatever replaces the rows takes it away (`derive::build_chunk`)
        commits(&mut |t| build_chunk(t, entity_type, index, CHUNK))
    })
}

/// Builds the order a value index keeps of its strings (`order::build_unit`), a chunk of its entries at a time, each
/// a unit: nothing for an index that keeps none, or isn't built. A build this builder has begun before and finds
/// ended is put off, and begins when its wait is over
#[allow(clippy::too_many_arguments)]
fn build_order(
    store: &Store,
    schema: &EngineSchema,
    commits: &Commits,
    core: &Core,
    work: &Work,
    entity_type: &str,
    index: &str,
    stopping: &dyn Fn() -> bool,
) -> crate::Result<()> {
    use super::order::{Step, build_unit, settled};
    let mut step = Step::Begin;
    chunks(store, core, stopping, || {
        let may_begin = core.may_begin(work);
        // A unit that would write nothing isn't made: its commit would be heard, and say nothing
        let nothing = store.read(|txn, db| {
            schema.validate(txn, db)?;
            settled(&super::unit::Snapshot { txn, db, schema }, entity_type, index, &step, may_begin)
        })?;
        // Where the build is moves on only once its unit commits
        let mut next = nothing.map(|reached| (reached, false));
        if next.is_none() {
            commits(&mut |t| {
                next = Some(build_unit(t, entity_type, index, step.clone(), may_begin, CHUNK)?);
                Ok(true)
            })?;
        }
        let (reached, began) = next.expect("the unit ran");
        if began {
            core.began(work);
        }
        step = reached;
        if step == Step::PutOff {
            core.put_off(work);
        }
        Ok(matches!(step, Step::Done | Step::PutOff))
    })
}

/// Adds a rank index's nodes a chunk at a time until every one is in it, then pushes a chunk at a time until none
/// waits, and settles its totals
/// Kept a function of its own for the instruction counts to enter at (`benches/instructions.rs`)
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn build_rank(
    store: &Store,
    schema: &EngineSchema,
    core: &Core,
    commits: &Commits,
    name: &str,
    stopping: &dyn Fn() -> bool,
    pause: &dyn Fn(),
) -> crate::Result<()> {
    failure_point!(BUILD_RANK at store.path(), crate::Error::new(crate::ErrorCode::Io, "injected rank build failure"));
    // Read before any chunk, and a full reader table waited out as a chunk's read waits it out
    let read = outlasting(store, core, stopping, || {
        store.read(|txn, db| {
            schema.validate(txn, db)?;
            let s = super::unit::Snapshot { txn, db, schema };
            Ok((
                super::ranks::totals(&s, name)?.stage != super::ranks::Stage::Joining,
                super::ranks::owes_work(&s, name)?,
            ))
        })
    })?;
    let Some((joined, owes)) = read else { return Ok(()) };
    // A commit that left the index alone queues it while a drain is computed; that drain has done what it owed
    if !owes {
        return Ok(());
    }
    if !joined {
        let mut after = None;
        chunks(store, core, stopping, || {
            // The cursor moves only once its chunk commits: a chunk refused leaves it where it was
            let mut next = None;
            let done = commits(&mut |t| {
                next = super::ranks::join_chunk(t, name, after, CHUNK)?;
                Ok(next.is_none())
            })?;
            after = next;
            Ok(done)
        })?;
    }
    let mirror = store.mirror();
    let mut retries = 0;
    chunks(store, core, stopping, || {
        // Computed outside the write lock, so commits go on meanwhile; only what it writes holds them up
        let planned = store.read(|txn, db| {
            schema.validate(txn, db)?;
            let s = super::unit::Snapshot { txn, db, schema };
            super::ranks::plan(&s, schema, name, DRAIN_PUSHES, Some(&mirror))
        })?;
        #[cfg(test)]
        store.rank_planned();
        // Written since it read: asked here first, so a burst of commits is waited out without taking the lock
        let may_retry = retries < STALE_RETRIES;
        let mut stale = may_retry
            && planned.as_ref().map_or(Ok(false), |drained| {
                store.read(|txn, db| drained.is_stale(&super::unit::Snapshot { txn, db, schema }))
            })?;
        #[cfg(test)]
        store.rank_checked();
        let done = !stale
            && commits(&mut |t| {
                let applied = planned.as_ref().map(|drained| drained.apply(t)).transpose()?;
                // A commit landed since the check: this unit writes nothing, and the drain waits it out like any other
                stale = may_retry && applied == Some(super::ranks::Applied::Stale);
                if stale {
                    return Ok(false);
                }
                #[cfg(test)]
                if let (Some(drained), Some(applied)) = (&planned, applied) {
                    let mut plans = store.rank_plans.lock();
                    if applied == super::ranks::Applied::Stale {
                        plans.redone += 1;
                    } else {
                        plans.applied += 1;
                    }
                    if drained.read_every_node() {
                        plans.every_node += 1;
                    } else {
                        plans.one_at_a_time += 1;
                    }
                }
                // Stale with its retries spent: drained again, here, from what the commits left
                let waiting = match applied {
                    // mutant-skip: delete match arm Some\(super::ranks::Applied::Written\{waiting\}\) in build_rank
                    // A drain written, then drained again in its unit: it stopped at the budget or with nothing
                    // waiting, so this one finds nothing to push, and one that stopped at its limit takes up the sweep
                    // where the next unit would
                    Some(super::ranks::Applied::Written { waiting }) => waiting,
                    _ => super::ranks::drain(t, name, DRAIN_PUSHES)?,
                };
                if waiting {
                    return Ok(false);
                }
                super::ranks::settle(t, name)?;
                Ok(true)
            })?;
        if stale {
            retries += 1;
            #[cfg(test)]
            {
                store.rank_plans.lock().retried += 1;
            }
            pause();
        }
        Ok(done)
    })
}

#[cfg(all(test, not(shuttle), not(loom)))]
mod tests {
    use super::*;

    /// A clock that stands where a test put it
    struct Stood(Instant);

    impl Clock for Stood {
        fn now(&self) -> Instant {
            self.0
        }
    }

    fn refused(code: ErrorCode) -> crate::Error {
        crate::Error::new(code, "refused")
    }

    #[test]
    fn a_chunk_refused_with_nothing_made_is_waited_out_and_any_other_failure_ends_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), crate::store::StoreOptions::default()).unwrap();
        let core = Core::new(Box::new(SystemClock));
        // A sync behind, then a full reader table: each waited out a pause, then the chunk run again
        let began = Instant::now();
        let mut tries = 0;
        chunks(&store, &core, &|| began.elapsed() > Duration::from_secs(20), || {
            tries += 1;
            match tries {
                1 => Err(refused(ErrorCode::SyncBehind)),
                2 => Err(refused(ErrorCode::ReadersFull)),
                _ => Ok(true),
            }
        })
        .unwrap();
        assert_eq!(tries, 3);
        assert!(began.elapsed() >= 2 * HELD_WAIT, "run again before its pause");
        assert!(core.lock().held.is_none(), "still held up once it went on");
        // Any other failure ends the build at once: given a stop after one try, a chunk tried again isn't
        let tries = std::cell::Cell::new(0);
        let ended = chunks(&store, &core, &|| tries.get() > 1, || {
            tries.set(tries.get() + 1);
            Err(refused(ErrorCode::Io))
        });
        assert_eq!(ended.unwrap_err().code, ErrorCode::Io);
        store.close().unwrap();
    }

    #[test]
    fn a_wait_out_asks_a_pause_apart_until_a_write_would_go_on_and_ends_at_a_stop() {
        let core = Core::new(Box::new(SystemClock));
        let began = Instant::now();
        let late = || began.elapsed() > Duration::from_secs(10);
        // Behind at the first ask, and through at the second
        let asks = std::cell::Cell::new(0);
        let ask = || {
            asks.set(asks.get() + 1);
            if asks.get() < 2 { Err(refused(ErrorCode::SyncBehind)) } else { Ok(()) }
        };
        wait_out(&ask, &core, &late, refused(ErrorCode::SyncBehind)).unwrap();
        assert_eq!(asks.get(), 2);
        assert!(began.elapsed() >= 2 * HELD_WAIT, "asked before a pause");
        assert!(core.lock().held.is_none());
        // Behind, and stopped meanwhile: it ends without asking again. At the fifth ask it would end all the same
        let (asks, stopped) = (std::cell::Cell::new(0), std::cell::Cell::new(false));
        let ask = || {
            asks.set(asks.get() + 1);
            stopped.set(true);
            if asks.get() < 5 { Err(refused(ErrorCode::SyncBehind)) } else { Ok(()) }
        };
        wait_out(&ask, &core, &|| stopped.get() || late(), refused(ErrorCode::SyncBehind)).unwrap();
        assert_eq!(asks.get(), 1, "asked again once stopped");
        // A failure other than a sync behind ends the wait with it
        let ask = || Err(refused(ErrorCode::Io));
        assert_eq!(wait_out(&ask, &core, &late, refused(ErrorCode::SyncBehind)).unwrap_err().code, ErrorCode::Io);
    }

    #[test]
    fn a_wait_for_the_builds_answers_what_holds_them_up_and_a_stop_ends_the_hold_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), crate::store::StoreOptions::default()).unwrap());
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        // As while the builder's thread runs a piece of work, every chunk of which is refused. Each wait is on a thread
        // of its own and bounded, so that a wait that never ends fails the test rather than hanging it
        core.claimed().lock().busy = true;
        let (built, building) = std::sync::mpsc::channel();
        {
            let (store, core) = (Arc::clone(&store), Arc::clone(&core));
            std::thread::spawn(move || {
                let _ = built.send(chunks(&store, &core, &|| core.stopping(), || Err(refused(ErrorCode::SyncBehind))));
            });
        }
        let (answered, answer) = std::sync::mpsc::channel();
        {
            let core = Arc::clone(&core);
            std::thread::spawn(move || {
                let _ = answered.send(core.wait());
            });
        }
        let held = answer.recv_timeout(Duration::from_secs(10)).expect("the wait for the builds waited on the hold");
        assert_eq!(held.expect("the wait answered what holds the builds up").code, ErrorCode::SyncBehind);
        let stopped_at = Instant::now();
        core.stop();
        building.recv_timeout(Duration::from_secs(10)).expect("the stopped builder went on").unwrap();
        assert!(stopped_at.elapsed() < HELD_WAIT / 2, "the stop waited for the pause to end");
        assert!(core.lock().held.is_none());
    }

    // The builder's state as the Shuttle model (`builder/model.rs`) holds it, here on real threads and in the suite
    // a mutation run builds, which the model's isn't: each case one rule of `Core`'s, with nothing timed but a
    // bound on how long a wait that shouldn't end is given to

    fn index(name: &str) -> Work {
        Work::Index { entity_type: "Task".to_owned(), index: name.to_owned() }
    }

    /// Runs `core` on a thread of its own, `build` doing each piece of work
    fn running(
        core: &Arc<Core>,
        build: impl FnMut(&Work, &dyn Fn() -> bool) -> crate::Result<()> + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        // Claimed before it is started, as `Building::start` claims one: a wait asked meanwhile sees it coming
        core.claimed();
        let core = Arc::clone(core);
        std::thread::spawn(move || core.run(build))
    }

    #[test]
    fn a_change_moves_the_generation_on_and_work_taken_before_it_is_to_stop() {
        let core = Core::new(Box::new(SystemClock));
        assert_eq!(core.generation(), 0);
        assert!(!core.over(0));
        core.begin_change();
        assert_eq!(core.generation(), 1);
        assert!(core.over(0), "work of the generation before isn't told to stop");
        assert!(!core.over(1), "work of the generation as it stands is told to stop");
        core.begin_change();
        assert_eq!(core.generation(), 2);
        // And stopping tells work of any generation
        core.stop();
        assert!(core.over(2));
    }

    #[test]
    fn closes_are_counted_and_the_thread_stays_stopped_until_each_has_said() {
        let core = Core::new(Box::new(SystemClock));
        core.begin_close();
        core.begin_close();
        core.end_close(false);
        assert!(core.closing() && core.lock().stopping, "a close under way, and a thread free to start beside it");
        core.end_close(false);
        assert!(!core.closing() && !core.lock().stopping);
        // One that closes stops it for good, and another that then gives up starts nothing
        core.begin_close();
        core.begin_close();
        core.end_close(true);
        core.end_close(false);
        assert!(core.closing() && core.lock().stopping);
    }

    #[test]
    fn a_wait_that_may_not_wait_through_a_close_is_told_of_one_and_of_nothing_else() {
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        // Work with no thread, and none to come: nothing waits for it, and the caller is told to start one
        core.queue([index("a")]);
        assert!(matches!(core.waited(false), Waited::Unrun));
        // Work with a thread claimed: a wait waits, and isn't told of a close that isn't there
        core.claimed();
        let (tell, told) = std::sync::mpsc::channel();
        let waiting = Arc::clone(&core);
        let waiter = std::thread::spawn(move || tell.send(matches!(waiting.waited(false), Waited::Idle)).unwrap());
        assert!(told.recv_timeout(Duration::from_millis(200)).is_err(), "the wait answered with work queued");
        core.stop();
        assert_eq!(told.recv_timeout(crate::waits::ARRIVES), Ok(true), "a builder stopped for good is waited for");
        waiter.join().unwrap();
        // A close under way is said to it at once
        let core = Core::new(Box::new(SystemClock));
        core.begin_close();
        assert!(matches!(core.waited(false), Waited::Closing));
    }

    #[test]
    fn a_builder_stopped_for_good_is_held_up_by_nothing() {
        let core = Core::new(Box::new(SystemClock));
        core.hold(Some(refused(ErrorCode::SyncBehind)));
        assert!(matches!(core.waited(true), Waited::Held(_)));
        core.stop();
        assert!(matches!(core.waited(true), Waited::Idle), "a closed builder answered what held it up");
    }

    #[test]
    fn a_failure_is_listed_once_for_its_generation_and_cleared_by_a_build_that_ends_built() {
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        let runs = Arc::new(Mutex::new(Vec::<&str>::new()));
        let plan = Arc::clone(&runs);
        let within = Arc::clone(&core);
        let runner = running(&core, move |work, _| {
            let Work::Index { index, .. } = work else { unreachable!() };
            let mut plan = plan.lock().unwrap();
            let what = plan.remove(0);
            drop(plan);
            assert!(what.starts_with(index.as_str()), "{index} ran where {what} was planned");
            match what {
                "a fails" | "b fails" => Err(refused(ErrorCode::Io)),
                "c replaced" => Err(refused(ErrorCode::DataReplaced)),
                "c changes" => {
                    // A schema change made from within the work, which then fails as its commit is refused
                    within.begin_change();
                    within.end_change([], false);
                    Err(refused(ErrorCode::SchemaChanged))
                }
                _ => Ok(()),
            }
        });
        let ran = |plan: &[&'static str], work: &[&str]| {
            runs.lock().unwrap().extend_from_slice(plan);
            core.queue(work.iter().map(|name| index(name)));
            assert!(matches!(core.waited(true), Waited::Idle));
            let mut failed: Vec<String> = core
                .lock()
                .failures
                .iter()
                .map(|failure| match &failure.work {
                    Work::Index { index, .. } => index.clone(),
                    _ => unreachable!(),
                })
                .collect();
            failed.sort();
            failed
        };
        assert_eq!(ran(&["a fails", "b fails"], &["a", "b"]), ["a", "b"]);
        assert_eq!(ran(&["a fails"], &["a"]), ["a", "b"], "a failure listed twice, or another's taken away");
        // Of work not failed before, so that one listed would show
        assert_eq!(ran(&["c replaced"], &["c"]), ["a", "b"], "work over rows replaced elsewhere listed as failed");
        assert_eq!(ran(&["c changes"], &["c"]), ["a", "b"], "a failure of the generation before listed");
        assert_eq!(ran(&["a built"], &["a"]), ["b"], "a build that ended built still listed as failed");
        core.stop();
        runner.join().unwrap();
    }

    #[test]
    fn work_that_ends_as_the_builder_is_stopped_leaves_what_had_failed_and_nothing_under_way() {
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let within = Arc::clone(&core);
        let runner = running(&core, move |_, _| {
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err(refused(ErrorCode::Io));
            }
            // A close that tells the thread to stop as this work ends, which said nothing of the build: it ends
            // having made it part way
            within.begin_close();
            Ok(())
        });
        core.queue([index("a")]);
        assert!(matches!(core.waited(true), Waited::Idle));
        assert_eq!(core.lock().failures.len(), 1);
        // Two pieces, the thread told to stop as the first ends
        core.queue([index("a"), index("b")]);
        runner.join().unwrap();
        let state = core.lock();
        assert_eq!(state.failures.len(), 1, "a build stopped part way cleared what had failed");
        assert!(!state.busy && state.active.is_none(), "the thread ended with work under way");
    }

    #[test]
    #[should_panic(expected = "work under way with the builder said to be idle")]
    fn the_state_s_invariants_are_checked_in_a_debug_build() {
        holds(&State { active: Some(index("a")), running: true, ..State::default() });
    }

    #[test]
    fn a_failure_listed_by_hand_replaces_the_work_s_own_and_leaves_the_others() {
        let building = Building::default();
        building.fail(index("a"), "first".to_owned());
        building.fail(index("b"), "other".to_owned());
        building.fail(index("a"), "second".to_owned());
        let failed: Vec<(Work, String)> =
            building.failures().into_iter().map(|failure| (failure.work, failure.message)).collect();
        assert_eq!(failed, [(index("b"), "other".to_owned()), (index("a"), "second".to_owned())]);
    }

    #[test]
    fn a_wait_waits_through_a_change_with_no_thread_running() {
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        core.begin_change();
        let (tell, told) = std::sync::mpsc::channel();
        let waiting = Arc::clone(&core);
        let waiter = std::thread::spawn(move || tell.send(matches!(waiting.waited(true), Waited::Idle)).unwrap());
        assert!(told.recv_timeout(Duration::from_millis(200)).is_err(), "the wait answered with a change under way");
        core.end_change(Vec::new(), true);
        assert_eq!(told.recv_timeout(crate::waits::ARRIVES), Ok(true));
        waiter.join().unwrap();
    }

    #[test]
    fn work_put_off_as_its_thread_is_stopped_is_not_queued_again() {
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        let within = Arc::clone(&core);
        let order = Work::Order { entity_type: "Task".to_owned(), index: "a".to_owned() };
        let put = order.clone();
        let runner = running(&core, move |_, _| {
            within.put_off(&put);
            within.begin_close();
            Ok(())
        });
        core.queue([order.clone()]);
        runner.join().unwrap();
        let state = core.lock();
        assert!(state.queue.is_empty(), "an order put off was queued again, to begin before its wait is over");
        assert!(state.later.iter().any(|(_, work)| *work == order));
    }

    #[test]
    fn a_build_that_panics_is_its_work_s_failure_and_the_thread_goes_on() {
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        let held = Arc::clone(&core);
        let runner = running(&core, move |work, _| {
            let Work::Index { index, .. } = work else { unreachable!() };
            if index == "a" {
                // Panics as it waits out what refused it: what held it up ends with it
                held.hold(Some(refused(ErrorCode::SyncBehind)));
                panic!("the stand-in's own");
            }
            Ok(())
        });
        core.queue([index("a"), index("b")]);
        // Held up while `a` is under way, and then not: asked again until the hold is long over, it is idle
        let began = Instant::now();
        let waited = loop {
            match core.waited(true) {
                Waited::Held(_) if began.elapsed() < crate::waits::ARRIVES => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                other => break other,
            }
        };
        assert!(matches!(waited, Waited::Idle), "still held up once the build that held it panicked");
        let failures = core.lock().failures.clone();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].work, index("a"));
        assert!(failures[0].message.contains("the stand-in's own"), "{}", failures[0].message);
        // The thread went on to `b`, and still runs
        assert!(core.running() && core.lock().queue.is_empty());
        core.stop();
        runner.join().expect("the thread ended as told, not with the build's panic");
    }

    #[test]
    fn a_claim_given_up_lets_the_next_asker_claim() {
        let core = Core::new(Box::new(SystemClock));
        core.queue([index("a")]);
        assert!(core.claim_start());
        core.unclaim();
        assert!(!core.running());
        assert!(core.claim_start(), "a claim given up held the building");
    }

    #[test]
    fn a_thread_is_claimed_only_where_one_is_needed_and_by_one_asker() {
        let core = Core::new(Box::new(SystemClock));
        assert!(!core.claim_start(), "a thread claimed with nothing to do");
        assert!(!core.running());
        core.queue([index("a")]);
        assert!(core.claim_start());
        assert!(core.running());
        assert!(!core.claim_start(), "a second thread claimed beside the first");
        // Given up: none during a close, one again once it gave up, none once it closed
        core.lock().running = false;
        core.begin_close();
        assert!(!core.claim_start(), "a thread claimed during a close");
        core.end_close(false);
        assert!(!core.closed());
        assert!(core.claim_start());
        core.lock().running = false;
        core.stop();
        assert!(core.closed());
        assert!(!core.claim_start(), "a thread claimed for a building stopped for good");
        // Work put off is work a thread is needed for, to begin it when it is due
        let core = Core::new(Box::new(SystemClock));
        let order = Work::Order { entity_type: "Task".to_owned(), index: "a".to_owned() };
        core.put_off(&order);
        assert!(core.put_off_now(&order) && !core.put_off_now(&index("a")));
        assert!(core.claim_start());
    }

    #[test]
    fn a_wait_says_where_work_is_queued_with_no_thread_to_take_it() {
        let core = Core::new(Box::new(SystemClock));
        core.queue([index("a")]);
        assert!(matches!(core.waited(true), Waited::Unrun));
        // A building stopped for good has nothing coming, and nothing to say of what it holds
        core.stop();
        assert!(matches!(core.waited(true), Waited::Idle));
    }

    #[test]
    fn the_thread_knows_itself_while_it_runs_and_no_other_thread_is_it() {
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        let within = Arc::clone(&core);
        let (tell, told) = std::sync::mpsc::channel();
        let runner = running(&core, move |_, _| {
            tell.send(within.on_its_thread()).unwrap();
            within.begin_close();
            Ok(())
        });
        assert!(!core.on_its_thread());
        core.queue([index("a")]);
        assert_eq!(told.recv_timeout(crate::waits::ARRIVES), Ok(true), "the thread didn't know itself");
        runner.join().unwrap();
        assert!(!core.on_its_thread(), "no other thread is it");

        // Running directly on this thread restores `ACTIVE_BUILDER` once `run` returns
        let direct = Core::new(Box::new(SystemClock));
        direct.queue([index("b")]);
        let mut inside = false;
        direct.claimed().run(|_, _| {
            inside = direct.on_its_thread();
            direct.begin_close();
            Ok(())
        });
        assert!(inside, "the thread didn't know itself");
        assert!(!direct.on_its_thread(), "the thread kept as it ended");
    }

    #[test]
    fn work_a_stop_interrupts_goes_back_to_the_head_of_the_queue_unless_it_failed_or_was_a_change_s() {
        // Stopped as it ended: queued again ahead of what waits
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        let within = Arc::clone(&core);
        let runner = running(&core, move |_, _| {
            within.begin_close();
            Ok(())
        });
        core.queue([index("a"), index("b")]);
        runner.join().unwrap();
        assert_eq!(core.lock().queue, [index("a"), index("b")], "the work under way was lost to the stop");
        // Stopped as it failed: listed as failed, and not queued again
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        let within = Arc::clone(&core);
        let runner = running(&core, move |_, _| {
            within.begin_close();
            Err(refused(ErrorCode::Io))
        });
        core.queue([index("a")]);
        runner.join().unwrap();
        let state = core.lock();
        assert!(state.queue.is_empty() && state.failures.len() == 1, "a failed build queued again");
        drop(state);
        // Taken by a change as it ran: the change's to hand back, not the building's to keep
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        let within = Arc::clone(&core);
        let runner = running(&core, move |_, _| {
            // A change that was made, and left nothing to build: what it took is its own
            within.begin_change();
            within.end_change(Vec::new(), true);
            within.begin_close();
            Ok(())
        });
        core.queue([index("a")]);
        runner.join().unwrap();
        assert!(core.lock().queue.is_empty(), "work of the generation before queued again");
    }

    #[test]
    fn a_build_of_an_order_begun_again_is_put_off_once_and_twice_as_long_each_time() {
        let start = Instant::now();
        let core = Core::new(Box::new(Stood(start)));
        let order = |index: &str| Work::Order { entity_type: "Note".to_owned(), index: index.to_owned() };
        let (name, title) = (order("name"), order("title"));
        let later = |core: &Core| core.lock().later.clone();
        // The first build of an order may begin, and once it has, the next may not: of that order alone
        assert!(core.may_begin(&name));
        core.began(&name);
        assert!(!core.may_begin(&name) && core.may_begin(&title));
        // Put off, it is due after the first wait, and is there once however often it is put off
        core.put_off(&name);
        core.put_off(&name);
        core.began(&title);
        core.put_off(&title);
        assert_eq!(later(&core), [(start + ORDER_PUT_OFF, name.clone()), (start + ORDER_PUT_OFF, title.clone())]);
        // Begun again, the one after waits twice as long, and so on up to the longest wait
        core.lock().later.clear();
        core.began(&name);
        core.put_off(&name);
        assert_eq!(later(&core), [(start + 2 * ORDER_PUT_OFF, name.clone())]);
        for _ in 0..20 {
            core.began(&name);
        }
        core.lock().later.clear();
        core.put_off(&name);
        assert_eq!(later(&core), [(start + ORDER_PUT_OFF_MOST, name)]);
    }
}
