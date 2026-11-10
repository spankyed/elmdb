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

/// What the thread does: build a type's index, or build or drain a rank index
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    Index { entity_type: String, index: String },
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

    fn stop(&self) {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).stopping = true;
        self.changed.notify_all();
    }

    fn stopping(&self) -> bool {
        self.lock().stopping
    }

    /// Blocks until every queued index is built or has failed, or the builder stops
    fn wait(&self) {
        let mut state = self.lock();
        while (!state.queue.is_empty() || state.busy) && !state.stopping {
            state = self.changed.wait(state).expect("the builder's lock is never poisoned");
        }
    }

    /// Takes the queue's work one piece at a time until stopped, `build` doing each, given whether to stop
    fn run(&self, mut build: impl FnMut(&Work, &dyn Fn() -> bool) -> crate::Result<()>) {
        loop {
            let work = {
                let mut state = self.lock();
                loop {
                    if state.stopping {
                        return;
                    }
                    if let Some(next) = state.queue.pop_front() {
                        state.busy = true;
                        state.active = Some(next.clone());
                        break next;
                    }
                    state.busy = false;
                    self.changed.notify_all();
                    state = self.changed.wait(state).expect("the builder's lock is never poisoned");
                }
            };
            if let Work::Rank { name } = &work {
                self.await_quiet(name);
            }
            // Work over rows another process has replaced isn't owed any more: adopting that queues what is
            if let Err(e) = build(&work, &|| self.stopping())
                && e.code != ErrorCode::DataReplaced
            {
                self.lock().failures.push(IndexFailure { work: work.clone(), message: e.message });
            }
            let mut state = self.lock();
            if state.stopping {
                return;
            }
            state.active = None;
            state.busy = !state.queue.is_empty();
            self.changed.notify_all();
        }
    }

    /// Waits until no commit has left a rank index pushes for `QUIET`, at most `PATIENCE`, or the builder stops. On a
    /// clock that moves only when a test moves it, the wait is for a wake: a queue, a stop, or the test's tick
    fn await_quiet(&self, name: &str) {
        let started = self.clock.now();
        let mut state = self.lock();
        loop {
            let now = self.clock.now();
            if state.stopping || now.saturating_duration_since(started) >= PATIENCE {
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

pub struct Builder {
    core: Arc<Core>,
    /// Held to wake the thread when it waits for a unit to end and is stopped
    store: Arc<Store>,
    thread: Option<JoinHandle<()>>,
}

impl Builder {
    pub fn start(store: Arc<Store>, schema: Arc<EngineSchema>, commits: Commits) -> Self {
        let core = Arc::new(Core::new(Box::new(SystemClock)));
        let thread = {
            let core = Arc::clone(&core);
            let store = Arc::clone(&store);
            std::thread::Builder::new()
                .name("elmdb-index".to_owned())
                .spawn(move || {
                    core.run(|work, stopping| match work {
                        Work::Index { entity_type, index } => {
                            build_index(&store, &commits, entity_type, index, stopping)
                        }
                        Work::Rank { name } => {
                            build_rank(&store, &schema, &commits, name, stopping, &|| core.await_quiet(name))
                        }
                    });
                })
                .expect("the index thread starts")
        };
        Self { core, store, thread: Some(thread) }
    }

    pub fn queue(&self, indexes: impl IntoIterator<Item = Work>) {
        self.core.queue(indexes);
    }

    /// What `wait` needs, held apart from the builder so a caller can wait without holding the slot the builder is
    /// kept in, which a schema change and a close take to stop it
    pub fn waiter(&self) -> Waiter {
        Waiter(Arc::clone(&self.core))
    }

    /// Work left after stop joins the worker, including an interrupted chunk's index.
    pub fn pending(&self) -> Vec<Work> {
        let state = self.core.lock();
        state.active.iter().chain(state.queue.iter()).cloned().collect()
    }

    pub fn failures(&self) -> Vec<IndexFailure> {
        self.core.lock().failures.clone()
    }

    /// Tells the thread to stop once the piece of work it is on ends, without waiting for it as `stop` does
    pub fn tell_to_stop(&self) {
        self.core.stop();
        self.store.wake_unit_waiters();
    }

    /// Whether the caller is on the builder's thread: a listener one of its commits delivered to, calling the
    /// database from there
    pub fn on_its_thread(&self) -> bool {
        self.thread.as_ref().is_some_and(|thread| thread.thread().id() == std::thread::current().id())
    }

    /// Stops the thread, leaving an index part built for the next open to finish. Its caller holds nothing the thread
    /// may wait for: a chunk's commit delivers to the watches it wrote into, and a watch's read, and whatever its
    /// listener calls of the database, takes the schema's lock. Called on the thread itself, by such a listener, it
    /// waits for nothing: the thread can't see itself end, and told to stop it ends once the listener returns
    pub fn stop(&mut self) {
        let waits = !self.on_its_thread();
        self.tell_to_stop();
        if let Some(thread) = self.thread.take()
            && waits
        {
            thread.join().expect("the index thread doesn't panic");
        }
    }

    /// Stops the thread without waiting for it, in a process that is exiting, whose other threads may be gone already
    pub fn abandon(&mut self) {
        self.tell_to_stop();
        // Dropping the handle detaches the thread
        self.thread.take();
    }
}

/// A handle on a builder's progress that outlives the builder: a builder stopped meanwhile ends the wait
pub struct Waiter(Arc<Core>);

impl Waiter {
    /// Blocks until every queued index is built or has failed, or the builder stops
    pub fn wait(&self) {
        self.0.wait();
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Runs `chunk` as units until it says it's done, waiting out a unit the app holds open; `Ok` once done or stopping
fn chunks(
    store: &Store,
    stopping: &dyn Fn() -> bool,
    mut chunk: impl FnMut() -> crate::Result<bool>,
) -> crate::Result<()> {
    loop {
        if stopping() {
            return Ok(());
        }
        match chunk() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            // A unit held open by the app has the write transaction: try again once it ends
            Err(e) if e.code == ErrorCode::UnitOpen => store.wait_while_unit_open(stopping),
            Err(e) => return Err(e),
        }
    }
}

/// Builds a type's index over the entities that predate it, a chunk at a time, the last chunk marking it built. A read
/// that had no answer while it was built (`INDEX_BUILDING`, a text index's) read that marker to be refused, so the
/// commit that writes it is the one that tells it to read again
fn build_index(
    store: &Store,
    commits: &Commits,
    entity_type: &str,
    index: &str,
    stopping: &dyn Fn() -> bool,
) -> crate::Result<()> {
    let mut after = 0;
    chunks(store, stopping, || {
        // The cursor moves only once its chunk commits: a chunk refused leaves it where it was
        let mut next = None;
        let done = commits(&mut |t| {
            next = build_chunk(t, entity_type, index, after, CHUNK)?;
            Ok(next.is_none())
        })?;
        after = next.unwrap_or(after);
        Ok(done)
    })
}

/// Adds a rank index's nodes a chunk at a time until every one is in it, then pushes a chunk at a time until none
/// waits, and settles its totals
/// Kept a function of its own for the instruction counts to enter at (`benches/instructions.rs`)
#[inline(never)]
fn build_rank(
    store: &Store,
    schema: &EngineSchema,
    commits: &Commits,
    name: &str,
    stopping: &dyn Fn() -> bool,
    pause: &dyn Fn(),
) -> crate::Result<()> {
    failure_point!(BUILD_RANK at store.path(), crate::Error::new(crate::ErrorCode::Io, "injected rank build failure"));
    let (joined, owes) = store.read(|txn, db| {
        schema.validate(txn, db)?;
        let s = super::unit::Snapshot { txn, db, schema };
        Ok((super::ranks::totals(&s, name)?.stage != super::ranks::Stage::Joining, super::ranks::owes_work(&s, name)?))
    })?;
    // A commit that left the index alone queues it while a drain is computed; that drain has done what it owed
    if !owes {
        return Ok(());
    }
    if !joined {
        let mut after = None;
        chunks(store, stopping, || {
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
    chunks(store, stopping, || {
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
