//! Work done on a thread of the pool, answered on the event loop: a write, a check, a rebuild or a backup, which can take
//! as long as the database is large. A check and a backup read, and each is a job of its own (`Job`). A write and a
//! rebuild write, one at a time: they wait in the database's queue, and one job of the pool applies them in the order
//! they were asked for (`Writes`). Each holds its own hold on the database. A close waits for one that is running; one
//! still queued when the close takes the database runs against no store and rejects with `CLOSED`, having written
//! nothing. A panic in the work is the handle's, as a read's is (`panics.rs`). A failure is elmdb's, and reaches
//! JavaScript with its code.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use napi::bindgen_prelude::*;
use napi::{Env, Task};

use crate::panics::{Health, Held};
use crate::to_js;

/// A write as the pool applies it: the work, and the settling of its promise, or for a write that waits for its sync,
/// the wait and the settling, left for after the writes behind it
type Queued = (bool, Box<dyn FnOnce() -> Option<Synced> + Send>);

/// The wait for a durable write's sync, and the settling of its promise
type Synced = Box<dyn FnOnce() + Send>;

/// How many durable writes are applied before their sync is waited for, while more writes keep coming
const SYNCED_TOGETHER: usize = 64;

/// The writes asked of a database and not yet applied, and whether a job of the pool is applying them
#[derive(Default)]
struct Waiting {
    writes: VecDeque<Queued>,
    draining: bool,
}

/// A database's writes off the thread that asks for them, applied in the order they were asked for.
///
/// A write waits for the one before it, and for the write lock while another process holds it. As jobs of the pool
/// each write in flight holds a thread while it waits, and which of them takes the write lock next is no order: by
/// default four writes waiting are every thread the pool has, and the file reads, lookups and hashes Node asks of the
/// pool wait behind them. Here they wait in a queue, and one job of the pool at a time applies them in order until
/// none waits, so the writes of a database handle hold one thread of the pool however many are in flight, for as
/// long as they keep coming. A job of the pool is what a process's exit waits for: `process.exit()` applies what was
/// asked for before it ends, as it does every job.
///
/// A durable write is applied with no sync of its own. The job goes on to the durable writes straight behind it, and
/// waits for a sync once the next in the queue is no durable write, or none is, or `SYNCED_TOGETHER` are owed one:
/// one sync covers every commit before it, so a run of durable writes waits for one sync between them, where each
/// applied in its turn would wait for its own, and none waits for a rebuild or for writes that asked for no sync
#[derive(Default)]
pub struct Writes {
    waiting: Arc<Mutex<Waiting>>,
}

/// The queue's lock: a panic while it is held is none of a write's, each of which catches its own, so what it holds
/// is as it was left
fn locked(waiting: &Mutex<Waiting>) -> MutexGuard<'_, Waiting> {
    waiting.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whose write it is: the hold on its database, the handle's health, and the export it is named by when it panics
pub struct Of {
    pub db: Held,
    pub health: Health,
    pub export: &'static str,
}

impl Writes {
    /// Queues `work` on the database, answering the promise of what it answers as `convert` makes it of JavaScript's
    /// values: settled on the event loop once the work has run in its turn, or rejected with elmdb's error
    pub fn queue<'env, T, V>(
        &self,
        env: &'env Env,
        Of { db, health, export }: Of,
        durable: bool,
        work: impl FnOnce(&elmdb::Database) -> elmdb::Result<(T, Option<elmdb::Unsynced>)> + Send + 'static,
        convert: fn(&Env, T) -> napi::Result<V>,
    ) -> napi::Result<Object<'env>>
    where
        T: Send + 'static,
        V: ToNapiValue + 'static,
    {
        // Held from before the job is asked for until the write is in the queue: the job, which takes it to look
        // for a write, finds this one. Nothing called under it calls back into this handle but a hook on asynchronous
        // work that writes to it, which would wait on itself
        let mut waiting = locked(&self.waiting);
        if !waiting.draining {
            // Its promise answers nothing anyone waits for: each write settles its own
            env.spawn(Drain(Arc::clone(&self.waiting)))?;
            waiting.draining = true;
        }
        // Made once there is a job to settle it: a promise no write settles would hold the event loop open
        let (deferred, promise) = env.create_deferred()?;
        let write = Box::new(move || {
            let ran = health.caught(export, || work(&db)).and_then(|done| done);
            // The hold is let go on the pool's thread, before the promise is settled on JavaScript's: the last one
            // closes the database
            drop(db);
            let settle = move |ran: elmdb::Result<T>| {
                deferred.resolve(move |env| match ran {
                    Ok(answer) => convert(&env, answer),
                    Err(failed) => Err(napi::Error::from(JsError::from(to_js(failed)).into_unknown(env))),
                });
            };
            match ran {
                // Committed, and durable once its sync has run: answered then, after the writes behind it
                Ok((answer, Some(unsynced))) => Some(Box::new(move || {
                    // Waited for whatever the handle has been through since: the write is made, and a panic of one
                    // behind it is not its answer
                    let synced = health.always(export, || unsynced.wait()).and_then(|synced| synced);
                    settle(synced.map(|()| answer));
                }) as Synced),
                Ok((answer, None)) => {
                    settle(Ok(answer));
                    None
                }
                Err(failed) => {
                    settle(Err(failed));
                    None
                }
            }
        });
        waiting.writes.push_back((durable, write));
        Ok(promise)
    }
}

/// The job of the pool that applies a database's waiting writes, in the order they were asked for, until none waits
struct Drain(Arc<Mutex<Waiting>>);

impl Task for Drain {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> napi::Result<()> {
        // The durable writes applied and not yet answered, in the order applied
        let mut unsynced: Vec<Synced> = Vec::new();
        loop {
            // The lock is let go before the write runs: writes are asked for while one is applied
            let next = {
                let mut waiting = locked(&self.0);
                // A write owed a sync is answered before anything but a durable write is applied behind it
                let together = unsynced.is_empty()
                    || (unsynced.len() < SYNCED_TOGETHER
                        && waiting.writes.front().is_some_and(|(durable, _)| *durable));
                let next = match together {
                    true => waiting.writes.pop_front(),
                    false => None,
                };
                // Said under the lock a write is queued under, so one queued after this asks for the next job. With
                // syncs to wait for the job goes on, and looks again once they are answered
                waiting.draining = next.is_some() || !unsynced.is_empty();
                next
            };
            match next {
                Some((_, write)) => unsynced.extend(write()),
                None if unsynced.is_empty() => return Ok(()),
                // The first asks for a sync and waits for it, which covers the rest
                None => unsynced.drain(..).for_each(|synced| synced()),
            }
        }
    }

    fn resolve(&mut self, _: Env, (): ()) -> napi::Result<()> {
        Ok(())
    }
}

/// The work a job does on the database
type Work<T> = Box<dyn FnOnce(&elmdb::Database) -> elmdb::Result<T> + Send>;

/// Work on the database, on a thread of the pool, answering `T` as `convert` makes it of JavaScript's values `V`
pub struct Job<T, V> {
    db: Held,
    health: Health,
    export: &'static str,
    work: Option<Work<T>>,
    convert: fn(&Env, T) -> napi::Result<V>,
    failed: Option<elmdb::Error>,
}

impl<T, V> Job<T, V> {
    pub fn new(
        db: Held,
        health: Health,
        export: &'static str,
        work: impl FnOnce(&elmdb::Database) -> elmdb::Result<T> + Send + 'static,
        convert: fn(&Env, T) -> napi::Result<V>,
    ) -> Self {
        Job { db, health, export, work: Some(Box::new(work)), convert, failed: None }
    }
}

impl<T: Send + 'static, V: ToNapiValue + TypeName> Task for Job<T, V> {
    type Output = T;
    type JsValue = V;

    fn compute(&mut self) -> napi::Result<T> {
        let Some(work) = self.work.take() else {
            return Err(napi::Error::from_reason("This job has already run".to_owned()));
        };
        let db = &self.db;
        let ran = self.health.caught(self.export, || work(db));
        ran.and_then(|done| done).map_err(|error| {
            let reason = error.message.clone();
            self.failed = Some(error);
            napi::Error::from_reason(reason)
        })
    }

    fn resolve(&mut self, env: Env, output: T) -> napi::Result<V> {
        (self.convert)(&env, output)
    }

    fn reject(&mut self, env: Env, error: napi::Error) -> napi::Result<V> {
        match self.failed.take() {
            Some(failed) => Err(napi::Error::from(JsError::from(to_js(failed)).into_unknown(env))),
            None => Err(error),
        }
    }
}

/// A job's answer in bytes, copied into a Buffer that V8 allocates (`crate::copied`)
pub fn bytes(env: &Env, answer: Vec<u8>) -> napi::Result<BufferSlice<'static>> {
    crate::copied(env, &answer)
}

/// A job's answer of JSON, made JavaScript's value by serde
pub fn value(env: &Env, answer: serde_json::Value) -> napi::Result<Unknown<'static>> {
    env.to_js_value(&answer)
}

/// A job that answers nothing
pub fn nothing(_: &Env, _: ()) -> napi::Result<()> {
    Ok(())
}
