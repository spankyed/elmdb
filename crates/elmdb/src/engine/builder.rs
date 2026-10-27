//! Building indexes in the background: a thread per open database takes the unbuilt indexes one at a time, in chunks of
//! `CHUNK` entities, each chunk a unit of its own, so the app's writes go on between them. Writes keep every index
//! whether built or not, so a build only fills in the entities that predate it. Until an index is built, a plan walks
//! the type's rows instead.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::database::EngineSchema;
use crate::error::ErrorCode;
use crate::store::Store;

use super::derive::build_chunk;
use super::unit::{Txn, UnitState};

/// How many entities one unit of a build covers
const CHUNK: usize = 1000;

/// An index being built, or that failed to build
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexFailure {
    pub entity_type: String,
    pub index: String,
    pub message: String,
}

#[derive(Default)]
struct State {
    queue: VecDeque<(String, String)>,
    busy: bool,
    stopping: bool,
    failures: Vec<IndexFailure>,
}

pub struct Builder {
    shared: Arc<(Mutex<State>, Condvar)>,
    /// Held to wake the thread when it waits for a unit to end and is stopped
    store: Arc<Store>,
    thread: Option<JoinHandle<()>>,
}

impl Builder {
    pub fn start(store: Arc<Store>, schema: Arc<EngineSchema>) -> Self {
        let shared: Arc<(Mutex<State>, Condvar)> = Arc::default();
        let thread = {
            let shared = Arc::clone(&shared);
            let store = Arc::clone(&store);
            std::thread::Builder::new()
                .name("elmdb-index".to_owned())
                .spawn(move || run(&store, &schema, &shared))
                .expect("the index thread starts")
        };
        Self { shared, store, thread: Some(thread) }
    }

    pub fn queue(&self, indexes: impl IntoIterator<Item = (String, String)>) {
        let (lock, changed) = &*self.shared;
        let mut state = lock.lock().expect("the builder's lock is never poisoned");
        for index in indexes {
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

fn run(store: &Store, schema: &EngineSchema, shared: &(Mutex<State>, Condvar)) {
    let (lock, changed) = shared;
    loop {
        let (entity_type, index) = {
            let mut state = lock.lock().expect("the builder's lock is never poisoned");
            loop {
                if state.stopping {
                    return;
                }
                if let Some(next) = state.queue.pop_front() {
                    state.busy = true;
                    break next;
                }
                state.busy = false;
                changed.notify_all();
                state = changed.wait(state).expect("the builder's lock is never poisoned");
            }
        };
        let mut after = 0;
        loop {
            if lock.lock().expect("the builder's lock is never poisoned").stopping {
                return;
            }
            let chunk = store.write(
                |txn, db| {
                    schema.validate(txn, db)?;
                    let mut state = UnitState::default();
                    let mut t = Txn { txn, db, state: &mut state, schema, now: 0.0 };
                    build_chunk(&mut t, &entity_type, &index, after, CHUNK)
                },
                false,
            );
            match chunk {
                Ok(Some(last)) => after = last,
                Ok(None) => break,
                // A unit held open by the app has the write transaction: try again once it ends
                Err(e) if e.code == ErrorCode::UnitOpen => {
                    store.wait_while_unit_open(|| lock.lock().expect("the builder's lock is never poisoned").stopping)
                }
                Err(e) => {
                    let mut state = lock.lock().expect("the builder's lock is never poisoned");
                    state.failures.push(IndexFailure {
                        entity_type: entity_type.clone(),
                        index: index.clone(),
                        message: e.message,
                    });
                    break;
                }
            }
        }
        let mut state = lock.lock().expect("the builder's lock is never poisoned");
        state.busy = !state.queue.is_empty();
        changed.notify_all();
    }
}
