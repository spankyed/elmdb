//! Building indexes in the background: a thread per open database takes the unbuilt indexes one at a time, in chunks of
//! `CHUNK` entities, each chunk a unit of its own, so the app's writes go on between them. Writes keep every index
//! whether built or not, so a build only fills in the entities that predate it. Until an index is built, a plan walks
//! the type's rows instead.
//!
//! A rank index is built and kept here too: its nodes are added a chunk at a time, then pushed a chunk of `CHUNK`
//! pushes at a time until none waits, which is also how the pushes a commit leaves over its budget are drained. A
//! drain's chunks commit like any other write, telling the watches whose reads they wrote into.

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
use super::unit::{Txn, UnitState};


/// How many entities one unit of a build covers
const CHUNK: usize = 1000;

/// How long a rank index's drain waits after the last commit that left it pushes, so that a burst of commits shares
/// one drain: each drain reads the whole graph and writes back every node it moved, whatever the commits before it
/// changed. Reads go on meanwhile, reporting the bound on their scores as it stands
const QUIET: Duration = Duration::from_millis(5);

/// The longest a drain waits for commits to pause, so a steady stream of them is still drained
const PATIENCE: Duration = Duration::from_millis(100);

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
            if let Err(e) = build(&work, &|| self.stopping()) {
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

/// How a drain's chunk commits: the database's own commit, which delivers to the watches it wrote into
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
                            build_index(&store, &schema, entity_type, index, stopping)
                        }
                        Work::Rank { name } => build_rank(&store, &schema, &commits, name, stopping),
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

    /// Stops the thread, leaving an index part built for the next open to finish
    pub fn stop(&mut self) {
        self.core.stop();
        self.store.wake_unit_waiters();
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the index thread doesn't panic");
        }
    }

    /// Stops the thread without waiting for it, in a process that is exiting, whose other threads may be gone already
    pub fn abandon(&mut self) {
        self.core.stop();
        self.store.wake_unit_waiters();
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

fn build_index(
    store: &Store,
    schema: &EngineSchema,
    entity_type: &str,
    index: &str,
    stopping: &dyn Fn() -> bool,
) -> crate::Result<()> {
    let mut after = 0;
    chunks(store, stopping, || {
        let next = store.write(
            |txn, db| {
                schema.validate(txn, db)?;
                let mut state = UnitState::default();
                let mut t = Txn { txn, db, state: &mut state, schema, now: 0.0 };
                build_chunk(&mut t, entity_type, index, after, CHUNK)
            },
            false,
        )?;
        match next {
            Some(last) => {
                after = last;
                Ok(false)
            }
            None => Ok(true),
        }
    })
}

/// Adds a rank index's nodes a chunk at a time until every one is in it, then pushes a chunk at a time until none
/// waits, and settles its totals
fn build_rank(
    store: &Store,
    schema: &EngineSchema,
    commits: &Commits,
    name: &str,
    stopping: &dyn Fn() -> bool,
) -> crate::Result<()> {
    let joined = store.read(|txn, db| {
        schema.validate(txn, db)?;
        let s = super::unit::Snapshot { txn, db, schema };
        Ok(super::ranks::totals(&s, name)?.stage != super::ranks::Stage::Joining)
    })?;
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
    chunks(store, stopping, || {
        commits(&mut |t| {
            if super::ranks::drain(t, name, DRAIN_PUSHES)? {
                return Ok(false);
            }
            super::ranks::settle(t, name)?;
            Ok(true)
        })
    })
}
