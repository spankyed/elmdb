//! Work done on a thread of the pool, answered on the event loop: a check, a rebuild or a backup, which can take as long
//! as the database is large. Each is a job of its own (`Job`), holding its own hold on the database. A close waits for
//! one that is running; one not yet started when the close takes the database runs against no store and rejects with
//! `CLOSED`, having done nothing. A panic in the work is the handle's, as a read's is (`panics.rs`). A failure is
//! elmdb's, and reaches JavaScript with its code.
//!
//! A rebuild writes, in one unit of its own, and takes the database's write lock as any write does: a write the
//! calling thread makes while it runs waits for it, and one made before it has the lock commits first. Either order
//! leaves derived data matching primary data, since a rebuild derives from what is committed when it begins. A rebuild
//! is refused while a unit is open (`UNIT_OPEN`), as a write is, so it never waits on a unit the calling thread holds.

use napi::bindgen_prelude::*;
use napi::{Env, Task};

use crate::panics::{Health, Held};
use crate::to_js;

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

/// A job's answer of JSON, made JavaScript's value via the unified JSON-to-JS converter
pub fn value(env: &Env, answer: serde_json::Value) -> napi::Result<Unknown<'static>> {
    crate::json_value_to_js(env, answer)
}

/// A job that answers nothing
pub fn nothing(_: &Env, _: ()) -> napi::Result<()> {
    Ok(())
}
