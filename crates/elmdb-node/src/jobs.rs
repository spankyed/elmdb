//! Work done on a thread of the pool, answered on the event loop: a write, a check, a rebuild or a backup, which can take
//! as long as the database is large. A job holds its own hold on the database, so the close that runs after it has
//! started waits for it, as one of `read()`'s does, and a panic in its work is the handle's, as a read's is
//! (`panics.rs`). A failure is elmdb's, and reaches JavaScript with its code, as `reject` makes it.

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
