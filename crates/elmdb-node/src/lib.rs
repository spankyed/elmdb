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
    inner: elmdb::Database,
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
        elmdb::Database::open(path, options).map(|inner| Self { inner }).map_err(to_js)
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
