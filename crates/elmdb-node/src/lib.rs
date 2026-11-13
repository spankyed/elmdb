//! The Node binding: elmdb's API for JavaScript, over N-API. Every failure elmdb reports throws an `Error` whose `code`
//! is an `ErrorCode`; an argument of the wrong JavaScript type is refused by N-API before elmdb sees it, with N-API's
//! own code (`InvalidArg`, `NumberExpected`, …). Every export runs its work where a panic is caught, so a panic in
//! elmdb is thrown as `PANICKED`, and the handle it was raised through refuses calls after it (`panics.rs`).

use napi::bindgen_prelude::*;
use napi_derive::napi;

#[cfg(feature = "diagnostics")]
pub mod diagnostics;
mod exit;
mod jobs;
mod panics;
mod process;

use jobs::Job;
use panics::{Health, Held, guarded};

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

/// `bytes` copied into a Buffer whose memory V8 allocates and frees itself, which is how every answer in bytes reaches
/// JavaScript. A `Buffer` made from a `Vec` is an external buffer: the bytes stay the `Vec`'s, and V8's collector frees
/// them on a thread of its own, where Node wakes a handle of the environment to have the JavaScript thread run the
/// finalizer. An environment that is ending closes that handle before it stops waking it, and libuv on Windows aborts
/// the process on a wake of a closing handle (`Assertion failed: !(handle->flags & UV_HANDLE_CLOSING)`), so a process
/// that ends while answers it dropped are being collected would die there. Freeing a copy calls nothing
fn copied<'env>(env: &Env, bytes: &[u8]) -> napi::Result<BufferSlice<'env>> {
    BufferSlice::copy_from(env, bytes)
}

/// `copied`, for an export whose failures are elmdb's. N-API failing to make the Buffer is none of elmdb's: where Node
/// left an exception of its own pending, that one is thrown, and otherwise an error with N-API's `GenericFailure`
fn answer<'env>(env: &Env, bytes: &[u8]) -> Result<BufferSlice<'env>, Code> {
    copied(env, bytes).map_err(|error| napi::Error::new(Code("GenericFailure"), error.reason.clone()))
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
    /// Sync every commit as it is made, so that one that returned survives the machine losing power, at the price of a
    /// sync per commit. It syncs whatever `syncs` says, and is ignored by a read-only database. Off by default, where
    /// commits are synced in groups, a moment after they return
    pub sync_each_commit: Option<bool>,
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

/// A commit the index builder made
#[napi(object)]
pub struct BuilderCommit {
    pub sequence: f64,
    pub touched: Vec<u32>,
}

/// An open database: one directory holding one LMDB environment
#[napi]
pub struct Database {
    inner: Held,
    /// Whether a call through this handle has panicked, after which it refuses calls (`panics.rs`)
    health: Health,
    /// The unit this handle began last, which a panic of the handle ends: the JavaScript object of it holds it too
    unit: std::cell::RefCell<Option<UnitSlot>>,
    /// The writes asked of this handle for the pool, applied in the order they were asked for (`jobs::Writes`)
    writes: jobs::Writes,
}

/// A unit as its JavaScript object and the handle that began it share it, on the thread that began it: empty once
/// it has ended
type UnitSlot = std::rc::Rc<std::cell::RefCell<Option<elmdb::Unit>>>;

/// The type and index a rebuild names: both, or neither for every index
fn rebuild_target(entity_type: Option<String>, index: Option<String>) -> Result<Option<(String, String)>, Code> {
    match (entity_type, index) {
        (Some(entity_type), Some(index)) => Ok(Some((entity_type, index))),
        (None, None) => Ok(None),
        _ => Err(napi::Error::new(
            Code("BATCH_INVALID"),
            "rebuild takes both a type and an index, or neither for every index".to_owned(),
        )),
    }
}

/// Ends the unit in a slot, keeping none of its writes, where a panic in ending it is caught
fn end_unit(slot: &UnitSlot) {
    // Nothing borrows the slot as this runs: an export's borrow ends with its work, panicking or not, and no
    // export calls back into JavaScript. A slot found borrowed is left, as a drop mustn't panic
    let unit = slot.try_borrow_mut().ok().and_then(|mut unit| unit.take());
    if let Some(unit) = unit {
        let _ = panics::caught("ending a unit", move || drop(unit));
    }
}

impl Database {
    /// Runs an export's work, refused once the handle has panicked. A panic in it ends the unit the handle has
    /// open: what the panic left in memory is not known, and the unit's transaction is part of it
    fn guarded<T>(&self, export: &'static str, work: impl FnOnce() -> Result<T, Code>) -> Result<T, Code> {
        let ran = self.health.guarded(export, work);
        self.end_unit_once_panicked();
        ran
    }

    /// `guarded`, for what runs whatever the handle has been through: a close, which the unit of a handle that
    /// panicked is ended for first
    fn unrefused<T>(&self, export: &'static str, work: impl FnOnce() -> Result<T, Code>) -> Result<T, Code> {
        self.end_unit_once_panicked();
        let ran = self.health.unrefused(export, work);
        self.end_unit_once_panicked();
        ran
    }

    /// A job on the database, on a thread of the pool, answering what `work` answers as `convert` makes it
    fn job<T, V>(
        &self,
        export: &'static str,
        work: impl FnOnce(&elmdb::Database) -> elmdb::Result<T> + Send + 'static,
        convert: fn(&Env, T) -> napi::Result<V>,
    ) -> AsyncTask<Job<T, V>>
    where
        T: Send + 'static,
        V: ToNapiValue + TypeName,
    {
        AsyncTask::new(Job::new(self.inner.again(), self.health.clone(), export, work, convert))
    }

    /// A write to the database on a thread of the pool, in its turn among those asked for, answering a promise of
    /// what `work` answers as `convert` makes it
    fn queued<'env, T, V>(
        &self,
        env: &'env Env,
        export: &'static str,
        work: impl FnOnce(&elmdb::Database) -> elmdb::Result<T> + Send + 'static,
        convert: fn(&Env, T) -> napi::Result<V>,
    ) -> Result<Object<'env>, Code>
    where
        T: Send + 'static,
        V: ToNapiValue + 'static,
    {
        self.writes
            .queue(env, self.inner.again(), self.health.clone(), export, work, convert)
            .map_err(|error| napi::Error::new(Code("GenericFailure"), error.reason.clone()))
    }

    fn end_unit_once_panicked(&self) {
        if self.health.panicked()
            && let Some(slot) = self.unit.try_borrow_mut().ok().and_then(|mut unit| unit.take())
        {
            end_unit(&slot);
        }
    }
}

#[napi]
impl Database {
    /// Opens the database at `path`, creating it unless `readOnly`
    #[napi(factory)]
    pub fn open(env: Env, path: String, options: Option<OpenOptions>) -> Result<Self, Code> {
        guarded("Database.open", || {
            process::claim()?;
            let options = options.unwrap_or(OpenOptions {
                read_only: None,
                schema: None,
                syncs: None,
                sync_each_commit: None,
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
                sync_each_commit: options.sync_each_commit.unwrap_or(false),
                sync_idle_ms: whole("syncIdleMs", options.sync_idle_ms, 0.0)?.unwrap_or(defaults.sync_idle_ms),
                sync_interval_ms: whole("syncIntervalMs", options.sync_interval_ms, 0.0)?
                    .unwrap_or(defaults.sync_interval_ms),
                map_size_bytes: whole("mapSizeBytes", options.map_size_bytes, 1.0)?.map(|n| n as usize),
            };
            let inner = std::sync::Arc::new(elmdb::Database::open(path, options).map_err(to_js)?);
            exit::close_at_exit(&env, &inner).map_err(|e| napi::Error::new(Code("LMDB"), e.reason.clone()))?;
            Ok(Self {
                inner: Held::new(inner),
                health: Health::default(),
                unit: std::cell::RefCell::default(),
                writes: jobs::Writes::default(),
            })
        })
    }

    #[napi(getter)]
    pub fn path(&self) -> Result<String, Code> {
        self.unrefused("Database.path", || Ok(self.inner.path().display().to_string()))
    }

    #[napi(getter)]
    pub fn read_only(&self) -> Result<bool, Code> {
        self.unrefused("Database.readOnly", || Ok(self.inner.is_read_only()))
    }

    #[napi(getter)]
    pub fn is_open(&self) -> Result<bool, Code> {
        self.unrefused("Database.isOpen", || Ok(self.inner.is_open()))
    }

    /// The schema the database holds, in its canonical form, or `null` when none was ever given
    #[napi(getter, ts_return_type = "Schema | null")]
    pub fn schema(&self) -> Result<Option<serde_json::Value>, Code> {
        self.guarded("Database.schema", || {
            Ok({
                self.inner.schema().map(|s| serde_json::from_str(&s.canonical()).expect("a canonical schema is JSON"))
            })
        })
    }

    /// Changes the open database's schema as opening with it would, answering each change and what was done about it.
    /// Refused while a unit is open, in a read-only database, and for a change opening refuses
    #[napi(ts_args_type = "schema: Schema", ts_return_type = "SchemaChange[]")]
    pub fn set_schema(&self, schema: serde_json::Value) -> Result<serde_json::Value, Code> {
        self.guarded("Database.setSchema", || {
            let schema = elmdb::schema::Schema::from_json(&schema).map_err(to_js)?;
            let changes = self.inner.set_schema(schema).map_err(to_js)?;
            Ok(serde_json::to_value(changes).expect("schema changes serialize"))
        })
    }

    /// What opening did: `{ schemaChanges: [{ path, change, effect }] }`
    #[napi(getter, ts_return_type = "{ schemaChanges: SchemaChange[] }")]
    pub fn open_report(&self) -> Result<serde_json::Value, Code> {
        self.guarded("Database.openReport", || {
            Ok(serde_json::json!({ "schemaChanges": self.inner.open_report().schema_changes }))
        })
    }

    /// Group commit's syncs so far
    #[napi(getter)]
    pub fn syncs(&self) -> Result<f64, Code> {
        self.guarded("Database.syncs", || self.inner.syncs().map(|n| n as f64).map_err(to_js))
    }

    /// Applies a batch (msgpack bytes of a `Batch`) as one unit, answering msgpack bytes of
    /// `{ results: OpResult[], changes: ChangeSet, sequence: number }`. `durable` commits it with a sync. On a thread of
    /// the pool, so the event loop isn't held while it runs. Writes in flight at once are applied one at a time in the
    /// order they were asked for, a rebuild among them in its turn, and hold one thread of the pool between them; one
    /// is refused while a unit is open
    #[napi(ts_return_type = "Promise<Buffer>")]
    pub fn write<'env>(&self, env: &'env Env, batch: Buffer, durable: Option<bool>) -> Result<Object<'env>, Code> {
        self.guarded("Database.write", || {
            let batch = batch.to_vec();
            self.queued(
                env,
                "Database.write, on a thread of the pool,",
                move |db| db.write_bytes(&batch, durable.unwrap_or(false)),
                jobs::bytes,
            )
        })
    }

    /// `write` on this thread, held until the batch is applied
    #[napi]
    pub fn write_sync<'env>(
        &self,
        env: &'env Env,
        batch: Buffer,
        durable: Option<bool>,
    ) -> Result<BufferSlice<'env>, Code> {
        self.guarded("Database.writeSync", || {
            answer(env, &self.inner.write_bytes(&batch, durable.unwrap_or(false)).map_err(to_js)?)
        })
    }

    /// Answers reads (msgpack bytes of `{ reads: ReadOp[] }`) from one snapshot, on this thread, as msgpack bytes of an
    /// answer per read
    #[napi]
    pub fn read_sync<'env>(&self, env: &'env Env, reads: Buffer) -> Result<BufferSlice<'env>, Code> {
        self.guarded("Database.readSync", || answer(env, &self.inner.read_bytes(&reads).map_err(to_js)?))
    }

    /// `readSync` on a thread of the pool, in a snapshot of its own, so a large read doesn't hold up the event loop
    #[napi(ts_return_type = "Promise<Buffer>")]
    pub fn read(&self, reads: Buffer) -> Result<AsyncTask<ReadTask>, Code> {
        self.guarded("Database.read", || {
            Ok({
                AsyncTask::new(ReadTask {
                    db: self.inner.again(),
                    health: self.health.clone(),
                    reads: reads.to_vec(),
                    failed: None,
                })
            })
        })
    }

    /// Drops what the reads tracked under `token` touched (`{ reads, track: token }`): no commit names it after
    #[napi]
    pub fn untrack(&self, token: f64) -> Result<(), Code> {
        self.guarded("Database.untrack", || {
            // A token is a u32, which a JS number holds exactly; any other number names no token
            if !(token.fract() == 0.0 && (0.0..=f64::from(u32::MAX)).contains(&token)) {
                return Err(napi::Error::new(
                    Code("BATCH_INVALID"),
                    format!("A tracking token is a whole number from 0 to {}, which {token} isn't", u32::MAX),
                ));
            }
            self.inner.untrack(token as u32);
            Ok(())
        })
    }

    /// Calls `listener` after each commit the index builder makes that wrote into a tracked read, with its sequence and
    /// those reads' tokens: a write's result names its own, and nothing else tells of a builder's. There is one listener,
    /// a second being refused, which keeps no process alive, and closing the database drops it
    #[napi(ts_args_type = "listener: (commit: { sequence: number; touched: number[] }) => void")]
    pub fn hear_builder_commits(
        &self,
        listener: napi::bindgen_prelude::Function<'_, BuilderCommit, ()>,
    ) -> Result<(), Code> {
        self.guarded("Database.hearBuilderCommits", || {
            let tell = listener
                .build_threadsafe_function()
                .callee_handled::<false>()
                .weak::<true>()
                .build()
                .map_err(|error| napi::Error::new(Code("IO"), error.to_string()))?;
            self.inner
                .hear_builder_commits(Some(std::sync::Arc::new(move |sequence, touched| {
                    tell.call(
                        BuilderCommit { sequence: sequence as f64, touched },
                        napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
                    );
                })))
                .map_err(to_js)
        })
    }

    /// Tells `listener` of commits other processes make as soon as each lands, which `committedElsewhere` then answers, a
    /// second being refused. It keeps no process alive, and closing the database drops it
    #[napi(ts_args_type = "listener: () => void")]
    pub fn hear_commits_elsewhere(&self, listener: napi::bindgen_prelude::Function<'_, (), ()>) -> Result<(), Code> {
        self.guarded("Database.hearCommitsElsewhere", || {
            let tell = listener
                .build_threadsafe_function()
                .callee_handled::<false>()
                .weak::<true>()
                .build()
                .map_err(|error| napi::Error::new(Code("IO"), error.to_string()))?;
            self.inner
                .hear_commits_elsewhere(std::sync::Arc::new(move || {
                    tell.call((), napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking);
                }))
                .map_err(to_js)
        })
    }

    /// The epoch another process's replacement of every row moved the database to, when this handle hasn't adopted it
    #[napi]
    pub fn replaced_elsewhere(&self) -> Result<Option<f64>, Code> {
        self.guarded("Database.replacedElsewhere", || {
            Ok(self.inner.replaced_elsewhere().map_err(to_js)?.map(|epoch| epoch as f64))
        })
    }

    /// Takes the replacement that moved the database to `epoch` as seen, so commits here are taken again; false, adopting
    /// nothing, when another has landed since
    #[napi]
    pub fn adopt_replacement(&self, epoch: f64) -> Result<bool, Code> {
        self.guarded("Database.adoptReplacement", || self.inner.adopt_replacement(epoch as u64).map_err(to_js))
    }

    /// How many tokens have tracked reads filed, and how many keys and ranges they hold
    #[napi(ts_return_type = "{ owners: number; keys: number; ranges: number }")]
    pub fn track_stats(&self) -> Result<serde_json::Value, Code> {
        self.guarded("Database.trackStats", || {
            Ok({
                let (owners, keys, ranges) = self.inner.track_stats();
                serde_json::json!({ "owners": owners, "keys": keys, "ranges": ranges })
            })
        })
    }

    /// The last commit's sequence, or 0 before the first: a number that only increases for as long as the database
    /// exists
    #[napi(getter)]
    pub fn sequence(&self) -> Result<f64, Code> {
        self.guarded("Database.sequence", || self.inner.sequence().map(|n| n as f64).map_err(to_js))
    }

    /// Opens a unit that stays open across calls until it is committed or aborted; while it is, `write` refuses
    /// (`UNIT_OPEN`), and writes go through the unit
    #[napi]
    pub fn begin(&self) -> Result<Unit, Code> {
        self.guarded("Database.begin", || {
            let inner: UnitSlot = std::rc::Rc::new(std::cell::RefCell::new(Some(self.inner.begin().map_err(to_js)?)));
            *self.unit.borrow_mut() = Some(std::rc::Rc::clone(&inner));
            Ok(Unit { inner, health: self.health.clone() })
        })
    }

    /// Compares derived data with primary data and every row with the schema's rules, on a thread of the pool
    #[napi(ts_return_type = "Promise<CheckReport>")]
    pub fn check(&self) -> Result<AsyncTask<Job<serde_json::Value, Unknown<'static>>>, Code> {
        self.guarded("Database.check", || {
            Ok(self.job(
                "Database.check, on a thread of the pool,",
                |db| db.check().map(|report| serde_json::to_value(report).expect("a report serializes")),
                jobs::value,
            ))
        })
    }

    /// `check` on this thread
    #[napi(ts_return_type = "CheckReport")]
    pub fn check_sync(&self) -> Result<serde_json::Value, Code> {
        self.guarded("Database.checkSync", || {
            let report = self.inner.check().map_err(to_js)?;
            Ok(serde_json::to_value(report).expect("a report serializes"))
        })
    }

    /// Deletes derived data and writes what primary data derives: every index's, or with `type` and `index`, one's. On a
    /// thread of the pool and in its turn among the writes asked for, as a write is
    #[napi(ts_return_type = "Promise<void>")]
    pub fn rebuild<'env>(
        &self,
        env: &'env Env,
        entity_type: Option<String>,
        index: Option<String>,
    ) -> Result<Object<'env>, Code> {
        self.guarded("Database.rebuild", || {
            let one = rebuild_target(entity_type, index)?;
            self.queued(
                env,
                "Database.rebuild, on a thread of the pool,",
                move |db| db.rebuild(one.as_ref().map(|(entity_type, index)| (entity_type.as_str(), index.as_str()))),
                jobs::nothing,
            )
        })
    }

    /// `rebuild` on this thread
    #[napi]
    pub fn rebuild_sync(&self, entity_type: Option<String>, index: Option<String>) -> Result<(), Code> {
        self.guarded("Database.rebuildSync", || {
            let one = rebuild_target(entity_type, index)?;
            self.inner
                .rebuild(one.as_ref().map(|(entity_type, index)| (entity_type.as_str(), index.as_str())))
                .map_err(to_js)
        })
    }

    /// Every value index of every declared type: whether it is built, and why it couldn't be when a build failed
    #[napi(ts_return_type = "IndexStatus[]")]
    pub fn index_status(&self) -> Result<serde_json::Value, Code> {
        self.guarded("Database.indexStatus", || {
            let status = self.inner.index_status().map_err(to_js)?;
            Ok(serde_json::to_value(status).expect("a status serializes"))
        })
    }

    /// Whether another handle has committed since this one last asked, or since it opened; never for this handle's own
    #[napi]
    pub fn committed_elsewhere(&self) -> Result<bool, Code> {
        self.guarded("Database.committedElsewhere", || self.inner.committed_elsewhere().map_err(to_js))
    }

    /// The checkpoint named `name`, as the latest commit left it
    #[napi]
    pub fn checkpoint<'env>(&self, env: &'env Env, name: String) -> Result<Option<BufferSlice<'env>>, Code> {
        self.guarded("Database.checkpoint", || {
            self.inner.checkpoint(&name).map_err(to_js)?.map(|value| answer(env, &value)).transpose()
        })
    }

    /// The entities of `types` that changed after `from` (msgpack of a `Position`), at most `limit`: msgpack bytes of
    /// its `Changed`
    #[napi]
    pub fn changed_since<'env>(
        &self,
        env: &'env Env,
        types: Vec<String>,
        from: Buffer,
        limit: u32,
    ) -> Result<BufferSlice<'env>, Code> {
        self.guarded("Database.changedSince", || {
            answer(env, &self.inner.changed_since_bytes(&types, &from, limit as usize).map_err(to_js)?)
        })
    }

    /// Blocks until every index waiting to be built is built or has failed
    #[napi]
    pub fn wait_for_index_builds(&self) -> Result<(), Code> {
        self.guarded("Database.waitForIndexBuilds", || self.inner.wait_for_index_builds().map_err(to_js))
    }

    /// Deletes what a removed declaration left: `{ type }`, `{ type, field }`, `{ relationKind }` or `{ role }`
    #[napi]
    pub fn purge(&self, what: PurgeTarget) -> Result<(), Code> {
        self.guarded("Database.purge", || {
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
        })
    }

    #[napi]
    pub fn rename_type(&self, from: String, to: String) -> Result<(), Code> {
        self.guarded("Database.renameType", || self.inner.rename_type(&from, &to).map_err(to_js))
    }

    #[napi]
    pub fn rename_field(&self, entity_type: String, from: String, to: String) -> Result<(), Code> {
        self.guarded("Database.renameField", || self.inner.rename_field(&entity_type, &from, &to).map_err(to_js))
    }

    #[napi]
    pub fn rename_relation(&self, from: String, to: String) -> Result<(), Code> {
        self.guarded("Database.renameRelation", || self.inner.rename_relation(&from, &to).map_err(to_js))
    }

    /// A compacted copy of one moment of the database into `target`, which opens as a database of its own, on a thread of
    /// the pool
    #[napi(ts_return_type = "Promise<void>")]
    pub fn backup(&self, target: String) -> Result<AsyncTask<Job<(), ()>>, Code> {
        self.guarded("Database.backup", || {
            Ok(self.job("Database.backup, on a thread of the pool,", move |db| db.backup(target), jobs::nothing))
        })
    }

    /// `backup` on this thread
    #[napi]
    pub fn backup_sync(&self, target: String) -> Result<(), Code> {
        self.guarded("Database.backupSync", || self.inner.backup(target).map_err(to_js))
    }

    /// Every entry replaced by another database's primary data, derived data rebuilt under this schema
    #[napi]
    pub fn replace_with(&self, source: String) -> Result<(), Code> {
        self.guarded("Database.replaceWith", || self.inner.replace_with(source).map_err(to_js))
    }

    /// Every entry removed but the database's format and schema
    #[napi]
    pub fn reset(&self) -> Result<(), Code> {
        self.guarded("Database.reset", || self.inner.reset().map_err(to_js))
    }

    /// What the database holds: entities per type and field, relations per kind, entries per index, its sizes
    #[napi(ts_return_type = "Stats")]
    pub fn stats(&self) -> Result<serde_json::Value, Code> {
        self.guarded("Database.stats", || {
            let stats = self.inner.stats().map_err(to_js)?;
            Ok(serde_json::to_value(stats).expect("stats serialize"))
        })
    }

    /// Loads (msgpack bytes of `Load[]`) applied in one durable unit without a write's checks, as an import does; a
    /// value outside the value spec is refused all the same
    #[napi]
    pub fn bulk_load(&self, loads: Buffer) -> Result<(), Code> {
        self.guarded("Database.bulkLoad", || self.inner.bulk_load_bytes(&loads).map_err(to_js))
    }

    /// Resolves once every unit committed before the call is on disk, and rejects when the sync it waits on fails or
    /// the database is closed
    #[napi(ts_return_type = "Promise<void>")]
    pub fn durable(&self) -> Result<AsyncTask<DurableTask>, Code> {
        self.guarded("Database.durable", || {
            Ok({
                let task = match self.inner.durable() {
                    Ok(durable) => DurableTask { durable: Some(durable), health: self.health.clone(), failed: None },
                    Err(error) => DurableTask { durable: None, health: self.health.clone(), failed: Some(error) },
                };
                AsyncTask::new(task)
            })
        })
    }

    /// Syncs what is owed and releases the environment; closing a closed database does nothing
    #[napi]
    pub fn close(&self) -> Result<(), Code> {
        self.unrefused("Database.close", || self.inner.close().map_err(to_js))
    }
}

/// `read()`'s promise, answered on a thread of the pool; a failure becomes an error with elmdb's code on the JS thread
pub struct ReadTask {
    db: Held,
    health: Health,
    reads: Vec<u8>,
    failed: Option<elmdb::Error>,
}

impl Task for ReadTask {
    type Output = Vec<u8>;
    type JsValue = BufferSlice<'static>;

    fn compute(&mut self) -> napi::Result<Vec<u8>> {
        let (db, reads) = (&self.db, &self.reads);
        let read = self.health.caught("Database.read, on a thread of the pool,", || db.read_bytes(reads));
        read.and_then(|read| read).map_err(|error| {
            let reason = error.message.clone();
            self.failed = Some(error);
            napi::Error::from_reason(reason)
        })
    }

    fn resolve(&mut self, env: Env, answer: Vec<u8>) -> napi::Result<BufferSlice<'static>> {
        copied(&env, &answer)
    }

    fn reject(&mut self, env: Env, error: napi::Error) -> napi::Result<BufferSlice<'static>> {
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
    health: Health,
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
        let waited = self.health.caught("Database.durable, on a thread of the pool,", || durable.wait());
        waited.and_then(|waited| waited).map_err(|error| {
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
    inner: UnitSlot,
    /// Its database handle's, which a panic in the unit is a panic of
    health: Health,
}

impl Unit {
    fn open(&self) -> Result<std::cell::RefMut<'_, elmdb::Unit>, Code> {
        std::cell::RefMut::filter_map(self.inner.borrow_mut(), Option::as_mut)
            .map_err(|_| napi::Error::new(Code("CLOSED"), "The unit has ended".to_owned()))
    }

    /// The unit taken out of its slot, to end it: by a commit, or keeping nothing
    fn take(&self) -> Option<elmdb::Unit> {
        self.inner.borrow_mut().take()
    }

    /// Runs an export's work on the unit. Once its handle has panicked, in this call or another, the unit is ended,
    /// keeping none of its writes: what a panic interrupted may be half of a batch
    fn run<T>(&mut self, export: &'static str, work: impl FnOnce(&mut Self) -> Result<T, Code>) -> Result<T, Code> {
        let health = self.health.clone();
        let ran = health.guarded(export, || work(self));
        if health.panicked() {
            end_unit(&self.inner);
        }
        ran
    }
}

impl Drop for Unit {
    /// A unit V8 collects while it is open is aborted, in a finalizer a panic would unwind out of into Node
    fn drop(&mut self) {
        end_unit(&self.inner);
    }
}

#[napi]
impl Unit {
    /// Applies a batch (msgpack bytes) in the unit, keeping none of it when any operation is refused; answers msgpack
    /// bytes of `{ results: OpResult[] }`
    #[napi]
    pub fn write<'env>(&mut self, env: &'env Env, batch: Buffer) -> Result<BufferSlice<'env>, Code> {
        self.run("Unit.write", |unit| answer(env, &unit.open()?.write_bytes(&batch).map_err(to_js)?))
    }

    /// Records `value` as the checkpoint `name`, committing with this unit or not at all
    #[napi]
    pub fn set_checkpoint(&mut self, name: String, value: Buffer) -> Result<(), Code> {
        self.run("Unit.setCheckpoint", |unit| unit.open()?.set_checkpoint(&name, &value).map_err(to_js))
    }

    /// The checkpoint named `name`, as this unit sees it
    #[napi]
    pub fn checkpoint<'env>(&mut self, env: &'env Env, name: String) -> Result<Option<BufferSlice<'env>>, Code> {
        self.run("Unit.checkpoint", |unit| {
            unit.open()?.checkpoint(&name).map_err(to_js)?.map(|value| answer(env, &value)).transpose()
        })
    }

    /// What the unit has changed so far: msgpack bytes of its `ChangeSet`
    #[napi]
    pub fn pending_changes<'env>(&mut self, env: &'env Env) -> Result<BufferSlice<'env>, Code> {
        self.run("Unit.pendingChanges", |unit| answer(env, &unit.open()?.pending_changes_bytes().map_err(to_js)?))
    }

    /// `Database.changedSince` read in the unit, before its own writes are stamped
    #[napi]
    pub fn changed_since<'env>(
        &mut self,
        env: &'env Env,
        types: Vec<String>,
        from: Buffer,
        limit: u32,
    ) -> Result<BufferSlice<'env>, Code> {
        self.run("Unit.changedSince", |unit| {
            answer(env, &unit.open()?.changed_since_bytes(&types, &from, limit as usize).map_err(to_js)?)
        })
    }

    /// The sequence this unit commits as, which what it writes is stamped with
    #[napi]
    pub fn next_sequence(&mut self) -> Result<f64, Code> {
        self.run("Unit.nextSequence", |unit| Ok(unit.open()?.next_sequence().map_err(to_js)? as f64))
    }

    /// Answers reads from the unit's transaction, which sees the unit's own writes
    #[napi]
    pub fn read<'env>(&mut self, env: &'env Env, reads: Buffer) -> Result<BufferSlice<'env>, Code> {
        self.run("Unit.read", |unit| answer(env, &unit.open()?.read_bytes(&reads).map_err(to_js)?))
    }

    /// Opens a savepoint, answering its depth
    #[napi]
    pub fn savepoint(&mut self) -> Result<u32, Code> {
        self.run("Unit.savepoint", |unit| unit.open()?.savepoint().map(|d| d as u32).map_err(to_js))
    }

    /// Undoes everything since the savepoint at `depth`, closing it
    #[napi]
    pub fn rollback_to(&mut self, depth: u32) -> Result<(), Code> {
        self.run("Unit.rollbackTo", |unit| unit.open()?.rollback_to(depth as usize).map_err(to_js))
    }

    /// Closes the savepoint at `depth`, keeping its writes
    #[napi]
    pub fn release(&mut self, depth: u32) -> Result<(), Code> {
        self.run("Unit.release", |unit| unit.open()?.release(depth as usize).map_err(to_js))
    }

    /// Commits the unit after its deferred checks, answering msgpack bytes of its `ChangeSet`; a failed check aborts it
    #[napi]
    pub fn commit<'env>(&mut self, env: &'env Env, durable: Option<bool>) -> Result<BufferSlice<'env>, Code> {
        self.run("Unit.commit", |unit| {
            let unit = unit.take().ok_or_else(|| napi::Error::new(Code("CLOSED"), "The unit has ended".to_owned()))?;
            let changes = unit.commit(durable.unwrap_or(false)).map_err(to_js)?;
            answer(env, &rmp_serde::to_vec_named(&changes).expect("a change set serializes"))
        })
    }

    /// Ends the unit, keeping nothing of it
    #[napi]
    pub fn abort(&mut self) -> Result<(), Code> {
        self.run("Unit.abort", |unit| {
            if let Some(mut unit) = unit.take() {
                unit.abort();
            }
            Ok(())
        })
    }

    #[napi(getter)]
    pub fn is_open(&self) -> Result<bool, Code> {
        // A unit of a handle that panicked has ended, whether or not a call has reached it since
        if self.health.panicked() {
            end_unit(&self.inner);
        }
        let open =
            self.health.unrefused("Unit.isOpen", || Ok(self.inner.borrow().as_ref().is_some_and(elmdb::Unit::is_open)));
        // And when this is the call that panicked
        if self.health.panicked() {
            end_unit(&self.inner);
        }
        open
    }
}
