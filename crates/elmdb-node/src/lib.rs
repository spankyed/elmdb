//! The Node binding: elmdb's API for JavaScript, over N-API. Every failure throws an `Error` whose `code` is an
//! `ErrorCode`.

use napi::bindgen_prelude::*;
use napi_derive::napi;

mod process;

/// An elmdb code as the status of a JavaScript error, which N-API hands JavaScript as `error.code`
pub struct Code(&'static str);

impl AsRef<str> for Code {
    fn as_ref(&self) -> &str {
        self.0
    }
}

/// An elmdb failure as JavaScript sees it: `error.code` is the elmdb code, `error.message` its message
fn to_js(error: elmdb::Error) -> napi::Error<Code> {
    napi::Error::new(Code(error.code.as_str()), error.message)
}

#[napi(object)]
pub struct OpenOptions {
    /// Open existing files without writing: every write is refused, and the stored schema is used
    pub read_only: Option<bool>,
    /// The schema, as the canonical JSON `docs/schema.md` describes; a schema that differs from the stored one
    /// changes it
    #[napi(ts_type = "Schema")]
    pub schema: Option<serde_json::Value>,
    /// Group commit's syncs, on by default; off, nothing is synced, for a test's temporary directory
    pub syncs: Option<bool>,
    pub sync_idle_ms: Option<u32>,
    pub sync_interval_ms: Option<u32>,
    pub map_size_bytes: Option<f64>,
}

/// An open database: one directory holding one LMDB environment
#[napi]
pub struct Database {
    inner: std::sync::Arc<elmdb::Database>,
}

#[napi]
impl Database {
    /// Opens the database at `path`, creating it unless `readOnly`
    #[napi(factory)]
    pub fn open(env: Env, path: String, options: Option<OpenOptions>) -> Result<Self, Code> {
        process::claim(&env)?;
        let options = options.unwrap_or(OpenOptions {
            read_only: None,
            schema: None,
            syncs: None,
            sync_idle_ms: None,
            sync_interval_ms: None,
            map_size_bytes: None,
        });
        let schema = match &options.schema {
            Some(json) => Some(elmdb::schema::Schema::from_json(json).map_err(to_js)?),
            None => None,
        };
        let defaults = elmdb::OpenOptions::default();
        let options = elmdb::OpenOptions {
            read_only: options.read_only.unwrap_or(false),
            schema,
            syncs: options.syncs.unwrap_or(true),
            sync_idle_ms: options.sync_idle_ms.map_or(defaults.sync_idle_ms, u64::from),
            sync_interval_ms: options.sync_interval_ms.map_or(defaults.sync_interval_ms, u64::from),
            map_size_bytes: options.map_size_bytes.map(|n| n as usize),
        };
        elmdb::Database::open(path, options).map(|inner| Self { inner: std::sync::Arc::new(inner) }).map_err(to_js)
    }

    #[napi(getter)]
    pub fn path(&self) -> String {
        self.inner.path().display().to_string()
    }

    #[napi(getter)]
    pub fn read_only(&self) -> bool {
        self.inner.is_read_only()
    }

    #[napi(getter)]
    pub fn is_open(&self) -> bool {
        self.inner.is_open()
    }

    /// The schema the database holds, in its canonical form, or `null` when none was ever given
    #[napi(getter, ts_return_type = "Schema | null")]
    pub fn schema(&self) -> Option<serde_json::Value> {
        self.inner.schema().map(|s| serde_json::from_str(&s.canonical()).expect("a canonical schema is JSON"))
    }

    /// What opening did: `{ schemaChanges: [{ path, change, effect }] }`
    #[napi(getter, ts_return_type = "{ schemaChanges: SchemaChange[] }")]
    pub fn open_report(&self) -> serde_json::Value {
        serde_json::json!({ "schemaChanges": self.inner.open_report().schema_changes })
    }

    /// Group commit's syncs so far
    #[napi(getter)]
    pub fn syncs(&self) -> Result<u32, Code> {
        self.inner.syncs().map(|n| n as u32).map_err(to_js)
    }

    /// Applies a batch (msgpack bytes of a `Batch`) as one unit, answering msgpack bytes of
    /// `{ results: OpResult[], changes: ChangeSet }`. `durable` commits it with a sync
    #[napi]
    pub fn write(&self, batch: Buffer, durable: Option<bool>) -> Result<Buffer, Code> {
        self.inner.write_bytes(&batch, durable.unwrap_or(false)).map(Buffer::from).map_err(to_js)
    }

    /// Answers reads (msgpack bytes of `{ reads: ReadOp[] }`) from one snapshot, on this thread, as msgpack bytes of an
    /// answer per read
    #[napi]
    pub fn read_sync(&self, reads: Buffer) -> Result<Buffer, Code> {
        self.inner.read_bytes(&reads).map(Buffer::from).map_err(to_js)
    }

    /// `readSync` on a thread of the pool, in a snapshot of its own, so a large read doesn't hold up the event loop
    #[napi(ts_return_type = "Promise<Buffer>")]
    pub fn read(&self, reads: Buffer) -> AsyncTask<ReadTask> {
        AsyncTask::new(ReadTask { db: std::sync::Arc::clone(&self.inner), reads: reads.to_vec(), failed: None })
    }

    /// Opens a unit that stays open across calls until it is committed or aborted; while it is, `write` refuses
    /// (`UNIT_OPEN`), and writes go through the unit
    #[napi]
    pub fn begin(&self) -> Result<Unit, Code> {
        self.inner.begin().map(|inner| Unit { inner: Some(inner) }).map_err(to_js)
    }

    /// Resolves once every unit committed before the call is on disk, and rejects when the sync it waits on fails
    #[napi(ts_return_type = "Promise<void>")]
    pub fn durable(&self) -> Result<AsyncTask<DurableTask>, Code> {
        let durable = self.inner.durable().map_err(to_js)?;
        Ok(AsyncTask::new(DurableTask { durable: Some(durable), failed: None }))
    }

    /// Syncs what is owed and releases the environment; closing a closed database does nothing
    #[napi]
    pub fn close(&self) -> Result<(), Code> {
        self.inner.close().map_err(to_js)
    }
}

/// `read()`'s promise, answered on a thread of the pool; a failure becomes an error with elmdb's code on the JS thread
pub struct ReadTask {
    db: std::sync::Arc<elmdb::Database>,
    reads: Vec<u8>,
    failed: Option<elmdb::Error>,
}

impl Task for ReadTask {
    type Output = Vec<u8>;
    type JsValue = Buffer;

    fn compute(&mut self) -> napi::Result<Vec<u8>> {
        self.db.read_bytes(&self.reads).map_err(|error| {
            let reason = error.message.clone();
            self.failed = Some(error);
            napi::Error::from_reason(reason)
        })
    }

    fn resolve(&mut self, _env: Env, answer: Vec<u8>) -> napi::Result<Buffer> {
        Ok(answer.into())
    }

    fn reject(&mut self, env: Env, error: napi::Error) -> napi::Result<Buffer> {
        match self.failed.take() {
            Some(failed) => Err(napi::Error::from(JsError::from(to_js(failed)).into_unknown(env))),
            None => Err(error),
        }
    }
}

/// `durable()`'s promise. A task's errors carry napi's own status, so a failure is kept here and turned into an error
/// with elmdb's code on the JS thread, in `reject`
pub struct DurableTask {
    durable: Option<elmdb::Durable>,
    failed: Option<elmdb::Error>,
}

impl Task for DurableTask {
    type Output = ();
    type JsValue = ();

    fn compute(&mut self) -> napi::Result<()> {
        let Some(durable) = self.durable.take() else { return Ok(()) };
        durable.wait().map_err(|error| {
            let reason = error.message.clone();
            self.failed = Some(error);
            napi::Error::from_reason(reason)
        })
    }

    fn resolve(&mut self, _env: Env, (): ()) -> napi::Result<()> {
        Ok(())
    }

    fn reject(&mut self, env: Env, error: napi::Error) -> napi::Result<()> {
        match self.failed.take() {
            Some(failed) => Err(napi::Error::from(JsError::from(to_js(failed)).into_unknown(env))),
            None => Err(error),
        }
    }
}

/// A unit open across calls, on the thread that began it
#[napi]
pub struct Unit {
    inner: Option<elmdb::Unit>,
}

impl Unit {
    fn open(&mut self) -> Result<&mut elmdb::Unit, Code> {
        self.inner.as_mut().ok_or_else(|| napi::Error::new(Code("CLOSED"), "The unit has ended".to_owned()))
    }
}

#[napi]
impl Unit {
    /// Applies a batch (msgpack bytes) in the unit, keeping none of it when any operation is refused; answers msgpack
    /// bytes of `{ results: OpResult[] }`
    #[napi]
    pub fn write(&mut self, batch: Buffer) -> Result<Buffer, Code> {
        self.open()?.write_bytes(&batch).map(Buffer::from).map_err(to_js)
    }

    /// Answers reads from the unit's transaction, which sees the unit's own writes
    #[napi]
    pub fn read(&mut self, reads: Buffer) -> Result<Buffer, Code> {
        self.open()?.read_bytes(&reads).map(Buffer::from).map_err(to_js)
    }

    /// Opens a savepoint, answering its depth
    #[napi]
    pub fn savepoint(&mut self) -> Result<u32, Code> {
        self.open()?.savepoint().map(|d| d as u32).map_err(to_js)
    }

    /// Undoes everything since the savepoint at `depth`, closing it
    #[napi]
    pub fn rollback_to(&mut self, depth: u32) -> Result<(), Code> {
        self.open()?.rollback_to(depth as usize).map_err(to_js)
    }

    /// Closes the savepoint at `depth`, keeping its writes
    #[napi]
    pub fn release(&mut self, depth: u32) -> Result<(), Code> {
        self.open()?.release(depth as usize).map_err(to_js)
    }

    /// Commits the unit after its deferred checks, answering msgpack bytes of its `ChangeSet`; a failed check aborts it
    #[napi]
    pub fn commit(&mut self, durable: Option<bool>) -> Result<Buffer, Code> {
        let unit =
            self.inner.take().ok_or_else(|| napi::Error::new(Code("CLOSED"), "The unit has ended".to_owned()))?;
        let changes = unit.commit(durable.unwrap_or(false)).map_err(to_js)?;
        Ok(rmp_serde::to_vec_named(&changes).expect("a change set serializes").into())
    }

    /// Ends the unit, keeping nothing of it
    #[napi]
    pub fn abort(&mut self) {
        if let Some(mut unit) = self.inner.take() {
            unit.abort();
        }
    }

    #[napi(getter)]
    pub fn is_open(&self) -> bool {
        self.inner.as_ref().is_some_and(elmdb::Unit::is_open)
    }
}
