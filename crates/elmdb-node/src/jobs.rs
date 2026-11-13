//! Work done off the thread that asked, answered on the event loop: a write, a check, a rebuild or a backup, which can
//! take as long as the database is large. A check and a backup read, and run on a thread of the pool (`Job`). A write
//! and a rebuild write, one at a time, and run on a thread of the database's own, in the order they were asked
//! (`Writes`). Each holds its own hold on the database. A close waits for one that is running; one still queued when
//! the close takes the database runs against no store and rejects with `CLOSED`, having written nothing. A panic in
//! the work is the handle's, as a read's is (`panics.rs`). A failure is elmdb's, and reaches JavaScript with its code.

use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, PoisonError};

use napi::bindgen_prelude::*;
use napi::{Env, Task};

use crate::panics::{Health, Held};
use crate::to_js;

/// A write as the database's writing thread runs it: the work, and the settling of its promise
type Queued = Box<dyn FnOnce() + Send>;

/// A database's writes off the thread that asks for them, applied in the order they were asked for by a thread of the
/// database's own.
///
/// A write waits for the one before it, and for the write lock while another process holds it. On the pool each
/// write in flight holds a thread while it waits, and the pool starts them in no order: by default four writes
/// waiting are every thread the pool has, and the file reads, lookups and hashes Node asks of the pool wait behind
/// them. Here they wait in a queue. The thread starts with the first write and ends when the handle has gone and
/// its queue is empty
#[derive(Default)]
pub struct Writes {
    queue: Mutex<Option<Sender<Queued>>>,
}

impl Writes {
    /// Queues `work` on the database, answering the promise of what it answers as `convert` makes it of JavaScript's
    /// values: settled on the event loop once the work has run in its turn, or rejected with elmdb's error
    pub fn queue<'env, T, V>(
        &self,
        env: &'env Env,
        db: Held,
        health: Health,
        export: &'static str,
        work: impl FnOnce(&elmdb::Database) -> elmdb::Result<T> + Send + 'static,
        convert: fn(&Env, T) -> napi::Result<V>,
    ) -> napi::Result<Object<'env>>
    where
        T: Send + 'static,
        V: ToNapiValue + 'static,
    {
        let (deferred, promise) = env.create_deferred()?;
        self.send(Box::new(move || {
            let ran = health.caught(export, || work(&db)).and_then(|done| done);
            // The hold is let go on this thread, before the promise is settled on JavaScript's: the last one closes
            // the database
            drop(db);
            deferred.resolve(move |env| match ran {
                Ok(answer) => convert(&env, answer),
                Err(failed) => Err(napi::Error::from(JsError::from(to_js(failed)).into_unknown(env))),
            });
        }))?;
        Ok(promise)
    }

    /// Hands the thread a write, starting it with the first
    fn send(&self, queued: Queued) -> napi::Result<()> {
        // A panic while the lock is held is one of the two calls below, which leave the queue as it was
        let mut queue = self.queue.lock().unwrap_or_else(PoisonError::into_inner);
        if queue.is_none() {
            let (sender, writes) = mpsc::channel::<Queued>();
            std::thread::Builder::new()
                .name("elmdb writes".to_owned())
                .spawn(move || {
                    // Each write catches its own panics, so the thread ends only when the queue's sender has gone
                    for write in writes {
                        write();
                    }
                })
                .map_err(|error| napi::Error::from_reason(format!("The thread for this database's writes: {error}")))?;
            *queue = Some(sender);
        }
        let sender = queue.as_ref().expect("the queue was made above");
        sender
            .send(queued)
            .map_err(|_| napi::Error::from_reason("The thread for this database's writes has ended".to_owned()))
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
