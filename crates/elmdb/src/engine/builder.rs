//! Building indexes in the background: a thread per open database takes the unbuilt indexes one at a time, in chunks of
//! `CHUNK` entities, each chunk a unit of its own, so the app's writes go on between them. Writes keep every index
//! whether built or not, so a build only fills in the entities that predate it. Until an index is built, a plan walks
//! the type's rows instead.
//!
//! A rank index is built and kept here too: its nodes are added a chunk at a time, then pushed a chunk of `CHUNK`
//! pushes at a time until none waits, which is also how the pushes a commit leaves over its budget are drained. A
//! build's chunks and a drain's commit like any other write, telling the watches and the tracked reads whose reads
//! they wrote into.

use std::collections::{HashMap, VecDeque};
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
}

/// How a wait for the builds ended (`Core::waited`)
pub enum Waited {
    /// Nothing is queued or under way and no change or close is being made, or the builder is closed
    Idle,
    /// The builder waits out what refused its work with nothing made
    Held(crate::Error),
    /// A close is under way, which the caller said it must not wait for
    Closing,
}

/// What the thread decides, apart from what doing a piece of work means: its queue, the pause a rank index's drain
/// waits for, stopping, and the waiters. The builder's thread runs it over the store, and the model
/// (`builder/model.rs`) over a stand-in, on a clock it moves
struct Core {
    state: Mutex<State>,
    changed: Condvar,
    clock: Box<dyn Clock>,
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
        self.changed.notify_all();
    }

    /// Stops the builder for good: its thread ends once the piece of work it is on has, and no one waits for it
    fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.stopping = true;
        state.closed = true;
        drop(state);
        self.changed.notify_all();
    }

    /// That a close is under way: the thread is told to stop, as by `stop`, and whoever waits for the builds waits
    /// for `end_close` to say whether the database closed. Work queued from here stays queued
    fn begin_close(&self) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.stopping = true;
        state.closing += 1;
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
        drop(state);
        self.changed.notify_all();
    }

    #[cfg(test)]
    fn stopping(&self) -> bool {
        self.lock().stopping
    }

    /// Whether a thread may not be started: a close is under way, or the builder is stopped for good
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
            Waited::Idle | Waited::Closing => None,
        }
    }

    /// `wait`, saying how it ended. A close under way is waited through, to its closing the database or giving up
    /// and leaving it open with what is left queued again, unless `through_a_close` is false: a caller the close
    /// may itself be waiting for, a listener, is told of it instead
    fn waited(&self, through_a_close: bool) -> Waited {
        let mut state = self.lock();
        while (!state.queue.is_empty() || state.busy || state.changing > 0 || state.closing > 0)
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
            _ => Waited::Idle,
        }
    }

    /// What holds the builds up, or that nothing does
    fn hold(&self, held: Option<crate::Error>) {
        self.lock().held = held;
        self.changed.notify_all();
    }

    /// Takes the queue's work one piece at a time until stopped, `build` doing each, given whether to stop
    fn run(&self, mut build: impl FnMut(&Work, &dyn Fn() -> bool) -> crate::Result<()>) {
        loop {
            let (work, generation) = {
                let mut state = self.lock();
                loop {
                    if state.stopping {
                        // Idle as it ends: a thread started after a close that gave up finds no work under way
                        state.busy = false;
                        state.active = None;
                        self.changed.notify_all();
                        return;
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
            let built = build(&work, &|| self.over(generation));
            let mut state = self.lock();
            // Work over rows another process has replaced isn't owed any more: adopting that queues what is. Nor is
            // work for what the builder built to when it was taken: what it failed on may be what replaced that
            let current = state.generation == generation;
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
            state.active = None;
            state.busy = !state.stopping && !state.queue.is_empty();
            self.changed.notify_all();
            if state.stopping {
                return;
            }
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

/// How a build's chunk and a drain's commit: the database's own commit, which delivers to the watches it wrote into
pub type Commits =
    Arc<dyn Fn(&mut dyn FnMut(&mut Txn<'_, '_>) -> crate::Result<bool>) -> crate::Result<bool> + Send + Sync>;

/// What the builder builds to: the schema, and how its chunks commit under it
type Target = (Arc<EngineSchema>, Commits);

/// A database's index building, from its open until it is dropped: the queue, the waiters and what is built to. Its
/// thread (`Builder`) is started with the first work queued, stopped by a close, and started again where the close
/// gave up and left the database open
#[derive(Clone)]
pub struct Building {
    core: Arc<Core>,
    /// What the thread takes each piece of work with: set as a thread is started, replaced by a schema change
    /// (`end_change`), and let go of by a close, since how a chunk commits holds the store
    target: Arc<Mutex<Option<Target>>>,
}

impl Default for Building {
    fn default() -> Self {
        Self { core: Arc::new(Core::new(Box::new(SystemClock))), target: Arc::new(Mutex::new(None)) }
    }
}

impl Building {
    fn target(&self) -> crate::sync::MutexGuard<'_, Option<Target>> {
        self.target.lock().expect("the builder's target is never poisoned")
    }

    /// Starts the thread, which builds to `schema` and commits through `commits`. One thread runs at a time: the
    /// caller holds the slot the one before was taken from, and starts none while a close is under way
    pub fn start(&self, store: Arc<Store>, schema: Arc<EngineSchema>, commits: Commits) -> Builder {
        *self.target() = Some((schema, commits));
        let core = Arc::clone(&self.core);
        #[cfg(test)]
        let forged = super::order::forged();
        let thread = {
            let core = Arc::clone(&core);
            let store = Arc::clone(&store);
            let target = Arc::clone(&self.target);
            std::thread::Builder::new()
                .name("elmdb-index".to_owned())
                .spawn(move || {
                    // A stamp forged by the thread that opened the database is this thread's too
                    #[cfg(test)]
                    super::order::forge(forged);
                    core.run(|work, stopping| {
                        // What the builder builds to as the work begins. A schema change replaces it only once it
                        // has told the builder, so work begun to one schema and gone on under another is work the
                        // change has told to stop, and whose commits the store refuses
                        let built_to = target.lock().expect("the builder's target is never poisoned").clone();
                        // Let go of by a close, which has told this thread to stop
                        let Some((schema, commits)) = built_to else { return Ok(()) };
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
                .expect("the index thread starts")
        };
        Builder { core, store, thread: Some(thread) }
    }

    pub fn queue(&self, indexes: impl IntoIterator<Item = Work>) {
        self.core.queue(indexes);
    }

    /// Whether anything is queued or put off, for a thread to take
    pub fn has_work(&self) -> bool {
        self.core.has_work()
    }

    /// Whether a thread may not be started: a close is under way, or the database closed
    pub fn closing(&self) -> bool {
        self.core.closing()
    }

    /// That what is built to is about to change, the schema or the rows: the work queued is for what it was, and
    /// goes (`Core::begin_change`). The change ends when what this answers is ended or dropped
    pub fn begin_change(&self) -> Change {
        Change { left: self.core.begin_change(), core: Some(Arc::clone(&self.core)) }
    }

    /// That the schema change `change` is made, or wasn't: what is built to from here is `schema`, committed
    /// through `commits`, and `work` is what that leaves to build. Where no thread was ever started nothing is
    /// built to yet, and the one started for `work` is given both
    pub fn end_change(&self, change: Change, schema: Arc<EngineSchema>, commits: Commits, work: Vec<Work>) {
        if let Some(target) = self.target().as_mut() {
            *target = (schema, commits);
        }
        change.end(work);
    }

    /// That a close is under way (`Core::begin_close`), until what this answers says the database closed or is
    /// dropped, which says it didn't. What is built to is let go of, with its hold on the store
    pub fn begin_close(&self) -> Closing {
        self.core.begin_close();
        *self.target() = None;
        Closing(Some(self.clone()))
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
pub struct Closing(Option<Building>);

impl Closing {
    /// That the close closed the database, or left it open in a process that is exiting: nothing more is built
    pub fn closed(mut self) {
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

/// The thread that builds, for as long as the database is open and has had work: stopped and waited for by a close
pub struct Builder {
    core: Arc<Core>,
    /// Held to wake the thread when it waits for a unit to end and is stopped
    store: Arc<Store>,
    thread: Option<JoinHandle<()>>,
}

impl Builder {
    /// Whether the caller is on this thread: a listener one of its commits delivered to, calling the database from
    /// there
    pub fn on_its_thread(&self) -> bool {
        self.thread.as_ref().is_some_and(|thread| thread.thread().id() == std::thread::current().id())
    }

    /// Waits for the thread, which a close has told to stop (`Building::begin_close`), to end: an index part built
    /// is left for the next open to finish. Its caller holds nothing the thread may wait for: a chunk's commit
    /// delivers to the watches it wrote into, and a watch's read, and whatever its listener calls of the database,
    /// takes the schema's lock. Called on the thread itself it waits for nothing, the thread ending once what
    /// called this returns
    pub fn join(&mut self) {
        let waits = !self.on_its_thread();
        self.store.wake_unit_waiters();
        if let Some(thread) = self.thread.take()
            && waits
        {
            thread.join().expect("the index thread doesn't panic");
        }
    }

    /// Lets go of the thread without waiting for it, in a process that is exiting, whose other threads may be gone
    /// already
    pub fn abandon(&mut self) {
        self.store.wake_unit_waiters();
        // Dropping the handle detaches the thread
        self.thread.take();
    }
}

impl Builder {
    /// Stops the thread and waits for it, leaving the building as a close that gave up leaves it, with what is
    /// queued kept and another thread free to be started: how a test has work wait for no thread
    #[cfg(test)]
    pub fn pause(mut self) {
        self.core.begin_close();
        self.join();
        self.core.end_close(false);
    }
}

impl Drop for Builder {
    /// A thread no close waited for, its database dropped where a close was refused: the building is stopped for
    /// good, and the thread waited for unless this is it, in a listener, when it ends once the listener returns and
    /// lets go of the store with it
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.core.stop();
            self.join();
        }
    }
}

/// A change to what is built to, begun and not ended (`Building::begin_change`). Dropped without being ended, a
/// failure or a panic part way, it ends as a change that wasn't made, with the work it took queued again, so no wait
/// for the builds is left waiting for it
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
        core.lock().busy = true;
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
