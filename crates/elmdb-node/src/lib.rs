//! The Node binding: elmdb's API for JavaScript, over N-API. Every failure elmdb reports throws an `Error` whose `code`
//! is an `ErrorCode`; an argument of the wrong JavaScript type is refused by N-API before elmdb sees it, with N-API's
//! own code (`InvalidArg`, `NumberExpected`, …).

use napi::bindgen_prelude::*;
use napi_derive::napi;

mod exit;
mod process;

/// An elmdb code as the status of a JavaScript error, which N-API hands JavaScript as `error.code`
pub struct Code(&'static str);

impl AsRef<str> for Code {
    fn as_ref(&self) -> &str {
        self.0
    }
}

/// An option that must be a whole number of at least `least`, refused otherwise (`VALUE_INVALID`) rather than cast
fn whole(name: &str, value: Option<f64>, least: f64) -> Result<Option<u64>, Code> {
    match value {
        None => Ok(None),
        Some(n) if n.is_finite() && n.fract() == 0.0 && n >= least && n <= 2f64.powi(53) => Ok(Some(n as u64)),
        Some(n) => Err(napi::Error::new(
            Code("VALUE_INVALID"),
            format!("{name} must be a whole number of at least {least}, and is {n}"),
        )),
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
    /// The schema, as the canonical JSON `docs/architecture/schema.md` describes; a schema that differs from the stored one
    /// changes it
    #[napi(ts_type = "Schema")]
    pub schema: Option<serde_json::Value>,
    /// Group commit's syncs, on by default; off, nothing is synced, for a test's temporary directory
    pub syncs: Option<bool>,
    pub sync_idle_ms: Option<f64>,
    pub sync_interval_ms: Option<f64>,
    pub map_size_bytes: Option<f64>,
}

/// What `purge` deletes
#[napi(object)]
pub struct PurgeTarget {
    #[napi(js_name = "type")]
    pub entity_type: Option<String>,
    pub field: Option<String>,
    pub relation_kind: Option<String>,
    pub role: Option<String>,
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
        process::claim()?;
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
            sync_idle_ms: whole("syncIdleMs", options.sync_idle_ms, 0.0)?.unwrap_or(defaults.sync_idle_ms),
            sync_interval_ms: whole("syncIntervalMs", options.sync_interval_ms, 0.0)?
                .unwrap_or(defaults.sync_interval_ms),
            map_size_bytes: whole("mapSizeBytes", options.map_size_bytes, 1.0)?.map(|n| n as usize),
        };
        let inner = std::sync::Arc::new(elmdb::Database::open(path, options).map_err(to_js)?);
        exit::close_at_exit(&env, &inner).map_err(|e| napi::Error::new(Code("LMDB"), e.reason.clone()))?;
        Ok(Self { inner })
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

    /// Changes the open database's schema as opening with it would, answering each change and what was done about it.
    /// Refused while a unit is open, in a read-only database, and for a change opening refuses
    #[napi(ts_args_type = "schema: Schema", ts_return_type = "SchemaChange[]")]
    pub fn set_schema(&self, schema: serde_json::Value) -> Result<serde_json::Value, Code> {
        let schema = elmdb::schema::Schema::from_json(&schema).map_err(to_js)?;
        let changes = self.inner.set_schema(schema).map_err(to_js)?;
        Ok(serde_json::to_value(changes).expect("schema changes serialize"))
    }

    /// What opening did: `{ schemaChanges: [{ path, change, effect }] }`
    #[napi(getter, ts_return_type = "{ schemaChanges: SchemaChange[] }")]
    pub fn open_report(&self) -> serde_json::Value {
        serde_json::json!({ "schemaChanges": self.inner.open_report().schema_changes })
    }

    /// Group commit's syncs so far
    #[napi(getter)]
    pub fn syncs(&self) -> Result<f64, Code> {
        self.inner.syncs().map(|n| n as f64).map_err(to_js)
    }

    /// Applies a batch (msgpack bytes of a `Batch`) as one unit, answering msgpack bytes of
    /// `{ results: OpResult[], changes: ChangeSet, sequence: number }`. `durable` commits it with a sync
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

    /// Drops what the reads tracked under `token` touched (`{ reads, track: token }`): no commit names it after
    #[napi]
    pub fn untrack(&self, token: f64) -> Result<(), Code> {
        // A token is a u32, which a JS number holds exactly; any other number names no token
        if !(token.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&token)) {
            return Err(napi::Error::new(
                Code("BATCH_INVALID"),
                format!("A tracking token is a whole number from 0 to {}, which {token} isn't", u32::MAX),
            ));
        }
        self.inner.untrack(token as u32);
        Ok(())
    }

    /// How many tokens have tracked reads filed, and how many keys and ranges they hold
    #[napi(ts_return_type = "{ owners: number; keys: number; ranges: number }")]
    pub fn track_stats(&self) -> serde_json::Value {
        let (owners, keys, ranges) = self.inner.track_stats();
        serde_json::json!({ "owners": owners, "keys": keys, "ranges": ranges })
    }

    /// The last commit's sequence, or 0 before the first: a number that only increases for as long as the database
    /// exists
    #[napi(getter)]
    pub fn sequence(&self) -> Result<f64, Code> {
        self.inner.sequence().map(|n| n as f64).map_err(to_js)
    }

    /// Opens a unit that stays open across calls until it is committed or aborted; while it is, `write` refuses
    /// (`UNIT_OPEN`), and writes go through the unit
    #[napi]
    pub fn begin(&self) -> Result<Unit, Code> {
        self.inner.begin().map(|inner| Unit { inner: Some(inner) }).map_err(to_js)
    }

    /// Compares derived data with primary data and every row with the schema's rules
    #[napi(ts_return_type = "CheckReport")]
    pub fn check(&self) -> Result<serde_json::Value, Code> {
        let report = self.inner.check().map_err(to_js)?;
        Ok(serde_json::to_value(report).expect("a report serializes"))
    }

    /// Deletes derived data and writes what primary data derives: every index's, or with `type` and `index`, one's
    #[napi]
    pub fn rebuild(&self, entity_type: Option<String>, index: Option<String>) -> Result<(), Code> {
        let one = match (entity_type.as_deref(), index.as_deref()) {
            (Some(entity_type), Some(index)) => Some((entity_type, index)),
            (None, None) => None,
            _ => {
                return Err(napi::Error::new(
                    Code("BATCH_INVALID"),
                    "rebuild takes both a type and an index, or neither for every index".to_owned(),
                ));
            }
        };
        self.inner.rebuild(one).map_err(to_js)
    }

    /// Every value index of every declared type: whether it is built, and why it couldn't be when a build failed
    #[napi(ts_return_type = "IndexStatus[]")]
    pub fn index_status(&self) -> Result<serde_json::Value, Code> {
        let status = self.inner.index_status().map_err(to_js)?;
        Ok(serde_json::to_value(status).expect("a status serializes"))
    }

    /// Blocks until every index waiting to be built is built or has failed
    #[napi]
    pub fn wait_for_index_builds(&self) -> Result<(), Code> {
        self.inner.wait_for_index_builds().map_err(to_js)
    }

    /// Deletes what a removed declaration left: `{ type }`, `{ type, field }`, `{ relationKind }` or `{ role }`
    #[napi]
    pub fn purge(&self, what: PurgeTarget) -> Result<(), Code> {
        use elmdb::engine::admin::Purge;
        let target = match (what.entity_type, what.field, what.relation_kind, what.role) {
            (Some(entity_type), Some(field), None, None) => Purge::Field { entity_type, field },
            (Some(entity_type), None, None, None) => Purge::Type(entity_type),
            (None, None, Some(kind), None) => Purge::RelationKind(kind),
            (None, None, None, Some(role)) => Purge::Role(role),
            _ => {
                return Err(napi::Error::new(
                    Code("BATCH_INVALID"),
                    "purge takes { type }, { type, field }, { relationKind } or { role }".to_owned(),
                ));
            }
        };
        self.inner.purge(&target).map_err(to_js)
    }

    #[napi]
    pub fn rename_type(&self, from: String, to: String) -> Result<(), Code> {
        self.inner.rename_type(&from, &to).map_err(to_js)
    }

    #[napi]
    pub fn rename_field(&self, entity_type: String, from: String, to: String) -> Result<(), Code> {
        self.inner.rename_field(&entity_type, &from, &to).map_err(to_js)
    }

    #[napi]
    pub fn rename_relation(&self, from: String, to: String) -> Result<(), Code> {
        self.inner.rename_relation(&from, &to).map_err(to_js)
    }

    /// A compacted copy of one moment of the database into `target`, which opens as a database of its own
    #[napi]
    pub fn backup(&self, target: String) -> Result<(), Code> {
        self.inner.backup(target).map_err(to_js)
    }

    /// Every entry replaced by another database's primary data, derived data rebuilt under this schema
    #[napi]
    pub fn replace_with(&self, source: String) -> Result<(), Code> {
        self.inner.replace_with(source).map_err(to_js)
    }

    /// Every entry removed but the database's format and schema
    #[napi]
    pub fn reset(&self) -> Result<(), Code> {
        self.inner.reset().map_err(to_js)
    }

    /// What the database holds: entities per type and field, relations per kind, entries per index, its sizes
    #[napi(ts_return_type = "Stats")]
    pub fn stats(&self) -> Result<serde_json::Value, Code> {
        let stats = self.inner.stats().map_err(to_js)?;
        Ok(serde_json::to_value(stats).expect("stats serialize"))
    }

    /// Loads (msgpack bytes of `Load[]`) applied in one durable unit without a write's checks, as an import does
    #[napi]
    pub fn bulk_load(&self, loads: Buffer) -> Result<(), Code> {
        let loads: Vec<elmdb::engine::maintenance::Load> = rmp_serde::from_slice(&loads)
            .map_err(|e| napi::Error::new(Code("BATCH_INVALID"), format!("The loads don't decode: {e}")))?;
        self.inner.bulk_load(&loads).map_err(to_js)
    }

    /// Resolves once every unit committed before the call is on disk, and rejects when the sync it waits on fails or
    /// the database is closed
    #[napi(ts_return_type = "Promise<void>")]
    pub fn durable(&self) -> AsyncTask<DurableTask> {
        let task = match self.inner.durable() {
            Ok(durable) => DurableTask { durable: Some(durable), failed: None },
            Err(error) => DurableTask { durable: None, failed: Some(error) },
        };
        AsyncTask::new(task)
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
        if let Some(failed) = &self.failed {
            return Err(napi::Error::from_reason(failed.message.clone()));
        }
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
