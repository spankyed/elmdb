//! Building indexes in the background: a thread per open database takes the unbuilt indexes one at a time, in chunks of
//! `CHUNK` entities, each chunk a unit of its own, so the app's writes go on between them. Writes keep every index
//! whether built or not, so a build only fills in the entities that predate it. Until an index is built, a plan walks
//! the type's rows instead.
//!
//! A rank index is built and kept here too: its nodes are added a chunk at a time, then pushed a chunk of `CHUNK`
//! pushes at a time until none waits, which is also how the pushes a commit leaves over its budget are drained. A
//! drain's chunks commit like any other write, telling the watches whose reads they wrote into.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::database::EngineSchema;
use crate::error::ErrorCode;
use crate::store::Store;

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

/// How a drain's chunk commits: the database's own commit, which delivers to the watches it wrote into
pub type Commits =
    Arc<dyn Fn(&mut dyn FnMut(&mut Txn<'_, '_>) -> crate::Result<bool>) -> crate::Result<bool> + Send + Sync>;

pub struct Builder {
    shared: Arc<(Mutex<State>, Condvar)>,
    /// Held to wake the thread when it waits for a unit to end and is stopped
    store: Arc<Store>,
    thread: Option<JoinHandle<()>>,
}

impl Builder {
    pub fn start(store: Arc<Store>, schema: Arc<EngineSchema>, commits: Commits) -> Self {
        let shared: Arc<(Mutex<State>, Condvar)> = Arc::default();
        let thread = {
            let shared = Arc::clone(&shared);
            let store = Arc::clone(&store);
            std::thread::Builder::new()
                .name("elmdb-index".to_owned())
                .spawn(move || run(&store, &schema, &commits, &shared))
                .expect("the index thread starts")
        };
        Self { shared, store, thread: Some(thread) }
    }

    pub fn queue(&self, indexes: impl IntoIterator<Item = Work>) {
        let (lock, changed) = &*self.shared;
        let mut state = lock.lock().expect("the builder's lock is never poisoned");
        for index in indexes {
            if let Work::Rank { name } = &index {
                state.queued_at.insert(name.clone(), Instant::now());
            }
            if !state.queue.contains(&index) {
                state.queue.push_back(index);
            }
        }
        changed.notify_all();
    }

    /// What `wait` needs, held apart from the builder so a caller can wait without holding the slot the builder is
    /// kept in, which a schema change and a close take to stop it
    pub fn waiter(&self) -> Waiter {
        Waiter(Arc::clone(&self.shared))
    }

    /// Work left after stop joins the worker, including an interrupted chunk's index.
    pub fn pending(&self) -> Vec<Work> {
        let state = self.shared.0.lock().expect("the builder's lock is never poisoned");
        state.active.iter().chain(state.queue.iter()).cloned().collect()
    }

    pub fn failures(&self) -> Vec<IndexFailure> {
        self.shared.0.lock().expect("the builder's lock is never poisoned").failures.clone()
    }

    /// Stops the thread, leaving an index part built for the next open to finish
    pub fn stop(&mut self) {
        {
            let (lock, changed) = &*self.shared;
            lock.lock().expect("the builder's lock is never poisoned").stopping = true;
            changed.notify_all();
        }
        self.store.wake_unit_waiters();
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the index thread doesn't panic");
        }
    }
}

/// A handle on a builder's progress that outlives the builder: a builder stopped meanwhile ends the wait
pub struct Waiter(Arc<(Mutex<State>, Condvar)>);

impl Waiter {
    /// Blocks until every queued index is built or has failed, or the builder stops
    pub fn wait(&self) {
        let (lock, changed) = &*self.0;
        let mut state = lock.lock().expect("the builder's lock is never poisoned");
        while (!state.queue.is_empty() || state.busy) && !state.stopping {
            state = changed.wait(state).expect("the builder's lock is never poisoned");
        }
    }
}

impl Builder {
    /// Stops the thread without waiting for it, in a process that is exiting, whose other threads may be gone already
    pub fn abandon(&mut self) {
        {
            let (lock, changed) = &*self.shared;
            lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner).stopping = true;
            changed.notify_all();
        }
        self.store.wake_unit_waiters();
        // Dropping the handle detaches the thread
        self.thread.take();
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(store: &Store, schema: &EngineSchema, commits: &Commits, shared: &(Mutex<State>, Condvar)) {
    let (lock, changed) = shared;
    loop {
        let work = {
            let mut state = lock.lock().expect("the builder's lock is never poisoned");
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
                changed.notify_all();
                state = changed.wait(state).expect("the builder's lock is never poisoned");
            }
        };
        let stopping = || lock.lock().expect("the builder's lock is never poisoned").stopping;
        if let Work::Rank { name } = &work {
            await_quiet(lock, changed, name);
        }
        let outcome = match &work {
            Work::Index { entity_type, index } => build_index(store, schema, entity_type, index, &stopping),
            Work::Rank { name } => build_rank(store, schema, commits, name, &stopping),
        };
        if let Err(e) = outcome {
            lock.lock()
                .expect("the builder's lock is never poisoned")
                .failures
                .push(IndexFailure { work: work.clone(), message: e.message });
        }
        if stopping() {
            return;
        }
        let mut state = lock.lock().expect("the builder's lock is never poisoned");
        state.active = None;
        state.busy = !state.queue.is_empty();
        changed.notify_all();
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

/// Waits until no commit has left a rank index pushes for `QUIET`, at most `PATIENCE`, or the builder stops
fn await_quiet(lock: &Mutex<State>, changed: &Condvar, name: &str) {
    let started = Instant::now();
    let mut state = lock.lock().expect("the builder's lock is never poisoned");
    loop {
        if state.stopping || started.elapsed() >= PATIENCE {
            return;
        }
        let since = state.queued_at.get(name).map_or(QUIET, Instant::elapsed);
        if since >= QUIET {
            return;
        }
        state = changed.wait_timeout(state, QUIET - since).expect("the builder's lock is never poisoned").0;
    }
}
