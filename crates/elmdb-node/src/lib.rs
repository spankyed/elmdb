//! Clean-room elmdb v2 Node.js N-API binding.
//!
//! - Every engine failure surfaces as a JS `Error` with `error.code` drawn from the single `elmdb::ErrorCode` taxonomy.
//! - Every export is panic-guarded (`panics.rs`): a Rust panic never unwinds into Node C++, instead throwing `PANICKED`
//!   and poisoning that handle until closed.
//! - Kernel-enforced single-binary-per-process claim (`process.rs`) and clean env/process exit teardown (`exit.rs`).
//! - Zero MsgPack: binary write batches speak `elmdb-wire` directly via `executeWireBatch`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::ThreadId;

use elmdb::protocol::{QuerySpec, Schema};
use elmdb::{
    BackupInto, BeforeCommitHook, BuildPolicy, DurabilityMode, EdgeDir, ExecBudgets, TypeMask, WatchId, WatchMode,
    WatchPolicy,
};
use elmdb_wire::{ElmdbError, ErrorCode, decode_batch};
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

/// An elmdb error code exposed as `error.code` in JavaScript.
pub struct Code(&'static str);

impl AsRef<str> for Code {
    fn as_ref(&self) -> &str {
        self.0
    }
}

fn to_js(error: elmdb::Error) -> napi::Error<Code> {
    napi::Error::new(Code(error.code.as_str()), error.message)
}

fn bad_arg(msg: impl Into<String>) -> napi::Error<Code> {
    napi::Error::new(Code(ErrorCode::InvalidArgument.as_str()), msg.into())
}

const MAX_SAFE_JS_INT: f64 = 9_007_199_254_740_991.0;

fn whole(name: &str, value: Option<f64>, least: f64) -> Result<Option<u64>, Code> {
    whole_bounded(name, value, least, MAX_SAFE_JS_INT)
}

fn whole_bounded(name: &str, value: Option<f64>, least: f64, most: f64) -> Result<Option<u64>, Code> {
    match value {
        None => Ok(None),
        Some(n) if n.is_finite() && n.fract() == 0.0 && n >= least && n <= most => Ok(Some(n as u64)),
        Some(n) => Err(bad_arg(format!("{name} must be a whole number in [{least}, {most}], got {n}"))),
    }
}

fn parse_durability(s: Option<&str>) -> Result<DurabilityMode, Code> {
    match s {
        None | Some("strict") => Ok(DurabilityMode::Strict),
        Some("grouped") => Ok(DurabilityMode::Grouped),
        Some("unsafe") => Ok(DurabilityMode::Unsafe),
        Some(other) => Err(bad_arg(format!("unknown durability mode '{other}' (expected strict|grouped|unsafe)"))),
    }
}

fn parse_build_policy(s: Option<&str>) -> Result<BuildPolicy, Code> {
    match s {
        None | Some("deferred") => Ok(BuildPolicy::DeferredWatermark),
        Some("eager") => Ok(BuildPolicy::EagerInSchemaChange),
        Some(other) => Err(bad_arg(format!("unknown buildPolicy '{other}' (expected deferred|eager)"))),
    }
}

fn parse_edge_dir(s: Option<&str>) -> Result<EdgeDir, Code> {
    match s {
        None | Some("out") => Ok(EdgeDir::Out),
        Some("in") => Ok(EdgeDir::In),
        Some("both") => Ok(EdgeDir::Both),
        Some(other) => Err(bad_arg(format!("unknown edge dir '{other}' (expected out|in|both)"))),
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn to_js_json(env: &Env, v: &impl serde::Serialize) -> Result<Unknown<'static>, Code> {
    let val = serde_json::to_value(v)
        .map_err(|e| napi::Error::new(Code(ErrorCode::Internal.as_str()), format!("serialize failed: {e}")))?;
    env.to_js_value(&val).map_err(|e| napi::Error::new(Code("GenericFailure"), e.reason.clone()))
}

// ---------------------------------------------------------------------------
// Thread-local registry for synchronous JS `beforeCommit` hooks (zero unsafe)
// ---------------------------------------------------------------------------

type JsHookEntry = (napi::sys::napi_env, FunctionRef<serde_json::Value, Unknown<'static>>);

static NEXT_JS_HOOK_KEY: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static JS_HOOKS: RefCell<HashMap<u64, JsHookEntry>> = RefCell::new(HashMap::new());
    /// Tracks which `napi_env` pointers already have an env cleanup hook registered to drain `JS_HOOKS`
    /// BEFORE Node destroys `napi_env` (preventing `FunctionRef::drop` UAF at worker/process teardown).
    static CLEANUP_REGISTERED: RefCell<std::collections::HashSet<usize>> = RefCell::new(std::collections::HashSet::new());
}

fn ensure_js_hook_env_cleanup(env: &Env) -> Result<(), Code> {
    let raw_key = env.raw() as usize;
    let already = CLEANUP_REGISTERED.with(|s| s.borrow().contains(&raw_key));
    if already {
        return Ok(());
    }
    env.add_env_cleanup_hook(raw_key, |env_addr| {
        let _ = panics::caught("draining JS beforeCommit hooks at env teardown", || {
            let _ = JS_HOOKS.try_with(|m| {
                let mut map = m.borrow_mut();
                map.retain(|_, (hook_env, _)| (*hook_env as usize) != env_addr);
            });
            let _ = CLEANUP_REGISTERED.try_with(|s| {
                s.borrow_mut().remove(&env_addr);
            });
        });
    })
    .map_err(|e| napi::Error::new(Code("GenericFailure"), e.reason.clone()))?;
    CLEANUP_REGISTERED.with(|s| s.borrow_mut().insert(raw_key));
    Ok(())
}

/// Options for `Database.open` / `Database.openAsync`.
#[napi(object)]
pub struct OpenOptions {
    pub read_only: Option<bool>,
    #[napi(ts_type = "Schema")]
    pub schema: Option<serde_json::Value>,
    /// `"strict"` (default), `"grouped"`, or `"unsafe"` (tests).
    pub durability: Option<String>,
    /// `"deferred"` (default, watermarked background builds) or `"eager"` (build inline on setSchema).
    pub build_policy: Option<String>,
    pub map_size_bytes: Option<f64>,
    pub max_dbs: Option<f64>,
    pub max_readers: Option<f64>,
    pub plan_cache_capacity: Option<f64>,
    pub watch_turn_slice_ms: Option<f64>,
}

/// Watch throttling & delivery mode policy for `registerWatch`.
#[napi(object)]
pub struct WatchPolicyOptions {
    /// `"declarative"` (default, 128-bit result dedup) or `"imperative"` (always deliver on invalidation).
    pub mode: Option<String>,
    pub min_gap_ms: Option<f64>,
    pub adaptive_multiplier: Option<f64>,
}

/// Return value of `registerWatch`.
#[napi(object)]
pub struct WatchRegistration {
    pub watch_id: f64,
    #[napi(ts_type = "QueryAnswer")]
    pub answer: serde_json::Value,
}

/// Return value of `rerunWatch`.
#[napi(object)]
pub struct WatchRerun {
    #[napi(ts_type = "QueryAnswer")]
    pub answer: serde_json::Value,
    pub changed: bool,
}

/// Return value of `nextDueWatch`.
#[napi(object)]
pub struct NextDueWatch {
    pub watch_id: Option<f64>,
    pub next_wake_delay_ms: Option<f64>,
    pub settled: bool,
}

fn parse_open_options(opts: Option<OpenOptions>) -> Result<(elmdb::OpenOptions, Option<Schema>), Code> {
    let mut out = elmdb::OpenOptions::default();
    let mut schema = None;
    if let Some(o) = opts {
        if let Some(ro) = o.read_only {
            out.read_only = ro;
        }
        out.durability = parse_durability(o.durability.as_deref())?;
        out.build_policy = parse_build_policy(o.build_policy.as_deref())?;
        if let Some(ms) = whole_bounded("mapSizeBytes", o.map_size_bytes, 4096.0, usize::MAX as f64)? {
            out.map_size = Some(ms as usize);
        }
        if let Some(md) = whole_bounded("maxDbs", o.max_dbs, 16.0, u32::MAX as f64)? {
            out.max_dbs = Some(md as u32);
        }
        if let Some(mr) = whole_bounded("maxReaders", o.max_readers, 16.0, u32::MAX as f64)? {
            out.max_readers = Some(mr as u32);
        }
        if let Some(pc) = whole_bounded("planCacheCapacity", o.plan_cache_capacity, 1.0, usize::MAX as f64)? {
            out.plan_cache_capacity = pc as usize;
        }
        if let Some(ts) = whole("watchTurnSliceMs", o.watch_turn_slice_ms, 1.0)? {
            out.watch_turn_slice_ms = ts;
        }
        if let Some(s) = o.schema {
            if out.read_only {
                return Err(to_js(ElmdbError::new(ErrorCode::ReadOnly, "cannot apply schema when opening read-only")));
            }
            let parsed: Schema = serde_json::from_value(s).map_err(|e| bad_arg(format!("invalid schema JSON: {e}")))?;
            schema = Some(parsed);
        }
    }
    Ok((out, schema))
}

fn parse_watch_policy(opts: Option<WatchPolicyOptions>) -> Result<WatchPolicy, Code> {
    let mut p = WatchPolicy::default();
    if let Some(o) = opts {
        match o.mode.as_deref() {
            None | Some("declarative") => p.mode = WatchMode::Declarative,
            Some("imperative") => p.mode = WatchMode::Imperative,
            Some(other) => {
                return Err(bad_arg(format!("unknown watch mode '{other}' (expected declarative|imperative)")));
            }
        }
        if let Some(v) = whole("minGapMs", o.min_gap_ms, 0.0)? {
            p.min_gap_ms = v;
        }
        if let Some(v) = whole("adaptiveMultiplier", o.adaptive_multiplier, 0.0)? {
            p.adaptive_multiplier = v;
        }
    }
    Ok(p)
}

/// Handle to an open elmdb v2 database directory.
#[napi]
pub struct Database {
    inner: Held,
    health: Health,
    /// Tracks `(engine_hook_id, js_hook_key)` registered on this JS thread so `close()`/`Drop` cleans them up.
    js_hooks: RefCell<Vec<(u64, u64)>>,
}

impl Drop for Database {
    fn drop(&mut self) {
        let hooks = std::mem::take(&mut *self.js_hooks.borrow_mut());
        if !hooks.is_empty() {
            for (engine_id, js_key) in hooks {
                let _ = self.inner.unregister_before_commit(engine_id);
                let _ = JS_HOOKS.try_with(|m| m.borrow_mut().remove(&js_key));
            }
        }
    }
}

/// Async open task executed on Node's libuv worker pool.
pub struct OpenTask {
    dir: PathBuf,
    opts: elmdb::OpenOptions,
    schema: Option<Schema>,
    failed: Option<elmdb::Error>,
}

impl Task for OpenTask {
    type Output = Arc<elmdb::Database>;
    type JsValue = Database;

    fn compute(&mut self) -> napi::Result<Self::Output> {
        let ran = panics::caught("Database.openAsync", || {
            let db = elmdb::Database::open(&self.dir, self.opts.clone())?;
            if let Some(ref s) = self.schema {
                if self.opts.read_only {
                    return Err(ElmdbError::new(ErrorCode::ReadOnly, "cannot apply schema when opening read-only"));
                }
                db.set_schema(s)?;
            }
            Ok(Arc::new(db))
        });
        ran.and_then(|res| res).map_err(|err| {
            let reason = err.message.clone();
            self.failed = Some(err);
            napi::Error::from_reason(reason)
        })
    }

    fn resolve(&mut self, env: Env, db: Self::Output) -> napi::Result<Self::JsValue> {
        exit::close_at_exit(&env, &db)?;
        Ok(Database { inner: Held::new(db), health: Health::default(), js_hooks: RefCell::new(Vec::new()) })
    }

    fn reject(&mut self, env: Env, err: napi::Error) -> napi::Result<Self::JsValue> {
        match self.failed.take() {
            Some(failed) => Err(napi::Error::from(JsError::from(to_js(failed)).into_unknown(env))),
            None => Err(err),
        }
    }
}

#[napi]
impl Database {
    /// Opens an existing elmdb v2 database or creates a new one in `dir`.
    #[napi(factory)]
    pub fn open(env: Env, dir: String, options: Option<OpenOptions>) -> Result<Database, Code> {
        guarded("Database.open", || {
            process::claim()?;
            let (opts, schema) = parse_open_options(options)?;
            let db = Arc::new(elmdb::Database::open(&dir, opts.clone()).map_err(to_js)?);
            if let Some(s) = schema {
                if opts.read_only {
                    return Err(to_js(ElmdbError::new(
                        ErrorCode::ReadOnly,
                        "cannot apply schema when opening read-only",
                    )));
                }
                db.set_schema(&s).map_err(to_js)?;
            }
            exit::close_at_exit(&env, &db).map_err(|e| napi::Error::new(Code("GenericFailure"), e.reason.clone()))?;
            Ok(Database { inner: Held::new(db), health: Health::default(), js_hooks: RefCell::new(Vec::new()) })
        })
    }

    /// Opens an elmdb v2 database asynchronously on Node's worker pool.
    #[napi(ts_return_type = "Promise<Database>")]
    pub fn open_async(dir: String, options: Option<OpenOptions>) -> Result<AsyncTask<OpenTask>, Code> {
        guarded("Database.openAsync", || {
            process::claim()?;
            let (opts, schema) = parse_open_options(options)?;
            Ok(AsyncTask::new(OpenTask { dir: PathBuf::from(dir), opts, schema, failed: None }))
        })
    }

    /// Loads a portable v2 JSON dump file into a brand-new database directory.
    #[napi(ts_return_type = "DumpReport")]
    pub fn load(env: Env, dump_file: String, target_dir: String) -> Result<Unknown<'static>, Code> {
        guarded("Database.load", || {
            process::claim()?;
            let report = elmdb::dump::load(&dump_file, &target_dir).map_err(to_js)?;
            to_js_json(&env, &report)
        })
    }

    #[napi(getter)]
    pub fn path(&self) -> Result<String, Code> {
        self.health.unrefused("Database.path", || Ok(self.inner.dir().to_string_lossy().into_owned()))
    }

    #[napi(getter)]
    pub fn read_only(&self) -> Result<bool, Code> {
        self.health.unrefused("Database.readOnly", || Ok(self.inner.is_read_only()))
    }

    #[napi(ts_return_type = "OpenReport")]
    pub fn open_report(&self, env: Env) -> Result<Unknown<'static>, Code> {
        self.health.unrefused("Database.openReport", || to_js_json(&env, self.inner.open_report()))
    }

    #[napi]
    pub fn acknowledge_restore(&self) -> Result<(), Code> {
        self.health.guarded("Database.acknowledgeRestore", || self.inner.acknowledge_restore().map_err(to_js))
    }

    #[napi(ts_return_type = "SchemaChange")]
    pub fn set_schema(
        &self,
        env: Env,
        #[napi(ts_arg_type = "Schema")] schema: serde_json::Value,
    ) -> Result<Unknown<'static>, Code> {
        self.health.guarded("Database.setSchema", || {
            let parsed: Schema =
                serde_json::from_value(schema).map_err(|e| bad_arg(format!("invalid schema JSON: {e}")))?;
            let change = self.inner.set_schema(&parsed).map_err(to_js)?;
            to_js_json(&env, &change)
        })
    }

    #[napi(ts_return_type = "Schema")]
    pub fn get_schema(&self, env: Env) -> Result<Unknown<'static>, Code> {
        self.health.guarded("Database.getSchema", || {
            let schema = self.inner.schema().map_err(to_js)?;
            let proto = elmdb::protocol::compiled_to_protocol_schema(&schema, false);
            to_js_json(&env, &proto)
        })
    }

    #[napi]
    pub fn rename_type(&self, old_type: String, new_type: String, rewrite_ids: Option<bool>) -> Result<f64, Code> {
        self.health.guarded("Database.renameType", || {
            let epoch = self.inner.rename_type(&old_type, &new_type, rewrite_ids.unwrap_or(false)).map_err(to_js)?;
            Ok(epoch as f64)
        })
    }

    #[napi]
    pub fn rename_field(&self, type_name: String, old_field: String, new_field: String) -> Result<f64, Code> {
        self.health.guarded("Database.renameField", || {
            let epoch = self.inner.rename_field(&type_name, &old_field, &new_field).map_err(to_js)?;
            Ok(epoch as f64)
        })
    }

    #[napi]
    pub fn rename_relation(&self, old_rel: String, new_rel: String) -> Result<f64, Code> {
        self.health.guarded("Database.renameRelation", || {
            let epoch = self.inner.rename_relation(&old_rel, &new_rel).map_err(to_js)?;
            Ok(epoch as f64)
        })
    }

    /// Executes an atomic `elmdb-wire` binary batch directly (single FFI crossing, zero MsgPack/JSON).
    #[napi(ts_return_type = "BatchResult")]
    pub fn execute_wire_batch(&self, env: Env, payload: Uint8Array) -> Result<Unknown<'static>, Code> {
        self.health.guarded("Database.executeWireBatch", || {
            let res = self.inner.execute_wire_batch(payload.as_ref()).map_err(to_js)?;
            to_js_json(&env, &res)
        })
    }

    /// Sets primary vector embeddings for `extId` on `indexName`.
    #[napi]
    pub fn set_embeddings(&self, index_name: String, ext_id: String, vectors: Vec<Vec<f64>>) -> Result<(), Code> {
        self.health.guarded("Database.setEmbeddings", || {
            let mut f32_vecs = Vec::with_capacity(vectors.len());
            for v in vectors {
                let mut out = Vec::with_capacity(v.len());
                for x in v {
                    if !x.is_finite() {
                        return Err(bad_arg("embedding components must be finite numbers"));
                    }
                    out.push(x as f32);
                }
                f32_vecs.push(out);
            }
            self.inner.set_embeddings(&index_name, &ext_id, &f32_vecs).map_err(to_js)
        })
    }

    /// Permanently destroys all entities belonging to an archived type.
    #[napi]
    pub fn purge_archived(&self, type_name: String) -> Result<f64, Code> {
        self.health.guarded("Database.purgeArchived", || {
            let count = self.inner.purge_archived(&type_name).map_err(to_js)?;
            Ok(count as f64)
        })
    }

    /// Registers a synchronous same-thread JS `beforeCommit` hook.
    ///
    /// The callback receives `{ entities, edges }` inside a native LMDB nested savepoint:
    /// - Throwing an error aborts the commit cleanly with `HOOK_ABORTED`.
    /// - Returning a `Uint8Array`/`Buffer` containing an `elmdb-wire` batch applies those derived writes
    ///   in the same savepoint cascade (up to `MAX_HOOK_ROUNDS`).
    #[napi]
    pub fn register_before_commit(
        &self,
        env: Env,
        type_names: Option<Vec<String>>,
        callback: Function<serde_json::Value, Unknown<'static>>,
    ) -> Result<f64, Code> {
        self.health.guarded("Database.registerBeforeCommit", || {
            ensure_js_hook_env_cleanup(&env)?;
            let schema = self.inner.schema().map_err(to_js)?;
            let wants = match type_names {
                None => TypeMask::all(),
                Some(names) => {
                    let mut m = TypeMask::empty();
                    for name in names {
                        let td = schema.type_by_name(&name).ok_or_else(|| {
                            to_js(ElmdbError::new(ErrorCode::UnknownType, format!("unknown type '{name}'")))
                        })?;
                        m.insert(td.id);
                    }
                    m
                }
            };

            let func_ref =
                callback.create_ref().map_err(|e| napi::Error::new(Code("GenericFailure"), e.reason.clone()))?;
            let js_key = NEXT_JS_HOOK_KEY.fetch_add(1, Ordering::Relaxed);
            JS_HOOKS.with(|m| m.borrow_mut().insert(js_key, (env.raw(), func_ref)));
            let expected_thread: ThreadId = std::thread::current().id();

            let register_res = self.inner.register_before_commit(BeforeCommitHook {
                wants,
                callback: Arc::new(move |ctx| {
                    if std::thread::current().id() != expected_thread {
                        return Err(ElmdbError::new(
                            ErrorCode::HookAborted,
                            "JS beforeCommit hook cannot run from an off-thread write",
                        ));
                    }
                    let sc = ctx.schema().clone();
                    let entities: Vec<serde_json::Value> = ctx
                        .entity_deltas()
                        .into_iter()
                        .map(|d| {
                            let type_name = sc.type_by_id(d.type_id).map(|t| t.name.as_str()).unwrap_or("");
                            let ext_id =
                                d.after.as_ref().or(d.before.as_ref()).map(|r| r.header.ext_id.as_str()).unwrap_or("");
                            let op = match (&d.before, &d.after) {
                                (None, Some(_)) => "create",
                                (Some(_), None) => "destroy",
                                (Some(b), Some(a)) if b.header.is_trashed() != a.header.is_trashed() => {
                                    if a.header.is_trashed() { "trash" } else { "restore" }
                                }
                                _ => "update",
                            };
                            serde_json::json!({
                                "id": ext_id,
                                "typeName": type_name,
                                "op": op,
                            })
                        })
                        .collect();
                    let raw_edges: Vec<_> = ctx
                        .edge_deltas()
                        .into_iter()
                        .map(|e| {
                            let rel_kind = sc.relation_by_id(e.kind).map(|r| r.name.clone()).unwrap_or_default();
                            let op = match (&e.before, &e.after) {
                                (None, Some(_)) => "link",
                                (Some(_), None) => "unlink",
                                _ => "update",
                            };
                            let endpoints = e.after.as_ref().or(e.before.as_ref()).map(|pe| (pe.src, pe.dst));
                            (rel_kind, op, endpoints)
                        })
                        .collect();
                    let mut edges = Vec::with_capacity(raw_edges.len());
                    for (rel_kind, op, endpoints) in raw_edges {
                        let (source_id, target_id) = match endpoints {
                            Some((src_key, dst_key)) => (
                                ctx.resolve_ext_id(src_key)?.unwrap_or_else(|| format!("__deleted:{}", src_key.0)),
                                ctx.resolve_ext_id(dst_key)?.unwrap_or_else(|| format!("__deleted:{}", dst_key.0)),
                            ),
                            None => (String::new(), String::new()),
                        };
                        edges.push(serde_json::json!({
                            "relKind": rel_kind,
                            "op": op,
                            "sourceId": source_id,
                            "targetId": target_id,
                        }));
                    }
                    let payload = serde_json::json!({ "entities": entities, "edges": edges });

                    // Extract `raw_env`, then borrow the local `Function` and DROP the `RefCell` borrow BEFORE
                    // calling into JS so re-entrant `registerBeforeCommit` / `unregisterBeforeCommit` / `close` never panics!
                    let maybe_raw_env: Option<napi::sys::napi_env> =
                        JS_HOOKS.with(|m| m.borrow().get(&js_key).map(|(e, _)| *e));
                    let maybe_frame: elmdb::Result<Option<Vec<u8>>> = match maybe_raw_env {
                        None => Ok(None),
                        Some(raw_env) => {
                            let js_env = Env::from_raw(raw_env);
                            let maybe_func: elmdb::Result<Option<Function<'_, serde_json::Value, Unknown<'static>>>> =
                                JS_HOOKS.with(|m| {
                                    let map = m.borrow();
                                    let Some((_, fref)) = map.get(&js_key) else {
                                        return Ok(None);
                                    };
                                    let func = fref
                                        .borrow_back(&js_env)
                                        .map_err(|e| ElmdbError::new(ErrorCode::HookAborted, e.reason.clone()))?;
                                    Ok(Some(func))
                                });
                            match maybe_func? {
                                None => Ok(None),
                                Some(func) => {
                                    let ret = func
                                        .call(payload)
                                        .map_err(|e| ElmdbError::new(ErrorCode::HookAborted, e.reason.clone()))?;
                                    let vtype = ret
                                        .get_type()
                                        .map_err(|e| ElmdbError::new(ErrorCode::HookAborted, e.reason.clone()))?;
                                    if matches!(vtype, ValueType::Undefined | ValueType::Null) {
                                        Ok(None)
                                    } else {
                                        let bytes = Uint8Array::from_unknown(ret).map_err(|e| {
                                            ElmdbError::new(
                                                ErrorCode::HookAborted,
                                                format!(
                                                    "beforeCommit hook must return void/null or Uint8Array wire batch: {e}"
                                                ),
                                            )
                                        })?;
                                        Ok(Some(bytes.as_ref().to_vec()))
                                    }
                                }
                            }
                        }
                    };

                    if let Some(frame) = maybe_frame? {
                        let decoded = decode_batch(&frame)?;
                        ctx.exec_batch(&decoded.ops)?;
                    }
                    Ok(())
                }),
            });

            let engine_hook_id = match register_res {
                Ok(id) => id,
                Err(err) => {
                    let _ = JS_HOOKS.try_with(|m| m.borrow_mut().remove(&js_key));
                    return Err(to_js(err));
                }
            };

            self.js_hooks.borrow_mut().push((engine_hook_id, js_key));
            Ok(engine_hook_id as f64)
        })
    }

    #[napi]
    pub fn unregister_before_commit(&self, hook_id: f64) -> Result<bool, Code> {
        self.health.guarded("Database.unregisterBeforeCommit", || {
            let id = whole("hookId", Some(hook_id), 1.0)?.expect("present");
            let removed = self.inner.unregister_before_commit(id);
            let mut hooks = self.js_hooks.borrow_mut();
            if let Some(pos) = hooks.iter().position(|(eid, _)| *eid == id) {
                let (_, js_key) = hooks.remove(pos);
                let _ = JS_HOOKS.try_with(|m| m.borrow_mut().remove(&js_key));
            }
            Ok(removed)
        })
    }

    /// Reads one entity row by external ID with all `EntityRef`s resolved in a single pinned MVCC snapshot.
    #[napi(ts_return_type = "Row | null")]
    pub fn get(&self, env: Env, ext_id: String) -> Result<Unknown<'static>, Code> {
        self.health.guarded("Database.get", || {
            let row = self.inner.get_row_resolved(&ext_id).map_err(to_js)?;
            to_js_json(&env, &row)
        })
    }

    /// Lists outgoing, incoming, or both-direction relations for `extId`.
    #[napi(ts_return_type = "Array<RelationRow>")]
    pub fn get_edges(
        &self,
        env: Env,
        ext_id: String,
        rel_kind: Option<String>,
        dir: Option<String>,
    ) -> Result<Unknown<'static>, Code> {
        self.health.guarded("Database.getEdges", || {
            let d = parse_edge_dir(dir.as_deref())?;
            let out = self.inner.get_edges_resolved(&ext_id, rel_kind.as_deref(), d).map_err(to_js)?;
            to_js_json(&env, &out)
        })
    }

    /// Executes a `QuerySpec` synchronously on a pinned lock-free MVCC read snapshot.
    #[napi(ts_return_type = "QueryAnswer")]
    pub fn query(
        &self,
        env: Env,
        #[napi(ts_arg_type = "QuerySpec")] spec: serde_json::Value,
    ) -> Result<Unknown<'static>, Code> {
        self.health.guarded("Database.query", || {
            let parsed: QuerySpec =
                serde_json::from_value(spec).map_err(|e| bad_arg(format!("invalid query spec: {e}")))?;
            let answer = self.inner.execute_query_spec(&parsed, ExecBudgets::default()).map_err(to_js)?;
            to_js_json(&env, &answer)
        })
    }

    /// Executes a `QuerySpec` off-thread on Node's libuv worker pool.
    #[napi(ts_return_type = "Promise<QueryAnswer>")]
    pub fn query_async(
        &self,
        #[napi(ts_arg_type = "QuerySpec")] spec: serde_json::Value,
    ) -> Result<AsyncTask<Job<serde_json::Value, Unknown<'static>>>, Code> {
        self.health.guarded("Database.queryAsync", || {
            let parsed: QuerySpec =
                serde_json::from_value(spec).map_err(|e| bad_arg(format!("invalid query spec: {e}")))?;
            Ok(AsyncTask::new(Job::new(
                self.inner.again(),
                self.health.clone(),
                "Database.queryAsync",
                move |db| {
                    let ans = db.execute_query_spec(&parsed, ExecBudgets::default())?;
                    serde_json::to_value(&ans)
                        .map_err(|e| ElmdbError::new(ErrorCode::Internal, format!("serialize answer: {e}")))
                },
                jobs::value,
            )))
        })
    }

    // -----------------------------------------------------------------------
    // Sans-I/O Live Query Watch Hub
    // -----------------------------------------------------------------------

    #[napi]
    pub fn register_watch(
        &self,
        #[napi(ts_arg_type = "QuerySpec")] spec: serde_json::Value,
        policy: Option<WatchPolicyOptions>,
        now_ms: Option<f64>,
    ) -> Result<WatchRegistration, Code> {
        self.health.guarded("Database.registerWatch", || {
            let parsed: QuerySpec =
                serde_json::from_value(spec).map_err(|e| bad_arg(format!("invalid query spec: {e}")))?;
            let pol = parse_watch_policy(policy)?;
            let now = whole("nowMs", now_ms, 0.0)?.unwrap_or_else(now_unix_ms);
            let (wid, ans) = self.inner.register_query_watch(&parsed, pol, now).map_err(to_js)?;
            let ans_val = serde_json::to_value(&ans)
                .map_err(|e| napi::Error::new(Code(ErrorCode::Internal.as_str()), e.to_string()))?;
            Ok(WatchRegistration { watch_id: wid.0 as f64, answer: ans_val })
        })
    }

    #[napi]
    pub fn rerun_watch(
        &self,
        watch_id: f64,
        #[napi(ts_arg_type = "QuerySpec")] spec: serde_json::Value,
        now_ms: Option<f64>,
        cost_ms: Option<f64>,
    ) -> Result<WatchRerun, Code> {
        self.health.guarded("Database.rerunWatch", || {
            let id = WatchId(whole("watchId", Some(watch_id), 1.0)?.expect("present"));
            let parsed: QuerySpec =
                serde_json::from_value(spec).map_err(|e| bad_arg(format!("invalid query spec: {e}")))?;
            let now = whole("nowMs", now_ms, 0.0)?.unwrap_or_else(now_unix_ms);
            let cost = whole("costMs", cost_ms, 0.0)?.unwrap_or(1);
            let (ans, changed) = self.inner.rerun_query_watch(id, &parsed, now, cost).map_err(to_js)?;
            let ans_val = serde_json::to_value(&ans)
                .map_err(|e| napi::Error::new(Code(ErrorCode::Internal.as_str()), e.to_string()))?;
            Ok(WatchRerun { answer: ans_val, changed })
        })
    }

    #[napi]
    pub fn unregister_watch(&self, watch_id: f64) -> Result<bool, Code> {
        self.health.guarded("Database.unregisterWatch", || {
            let id = WatchId(whole("watchId", Some(watch_id), 1.0)?.expect("present"));
            Ok(self.inner.unregister_watch(id))
        })
    }

    #[napi]
    pub fn catch_up_cross_process(&self, now_ms: Option<f64>) -> Result<Vec<f64>, Code> {
        self.health.guarded("Database.catchUpCrossProcess", || {
            let now = whole("nowMs", now_ms, 0.0)?.unwrap_or_else(now_unix_ms);
            let dirty = self.inner.catch_up_cross_process(now).map_err(to_js)?;
            Ok(dirty.into_iter().map(|id| id.0 as f64).collect())
        })
    }

    #[napi(ts_return_type = "WatchTick")]
    pub fn tick_watches(&self, env: Env, now_ms: Option<f64>) -> Result<Unknown<'static>, Code> {
        self.health.guarded("Database.tickWatches", || {
            let now = whole("nowMs", now_ms, 0.0)?.unwrap_or_else(now_unix_ms);
            let t = self.inner.tick_watches(now);
            let wire = elmdb::protocol::WatchTick {
                due_ids: t.due_ids.into_iter().map(|id| id.0).collect(),
                next_wake_delay_ms: t.next_wake_delay_ms,
                settled: t.settled,
            };
            to_js_json(&env, &wire)
        })
    }

    #[napi]
    pub fn next_due_watch(&self, now_ms: Option<f64>, elapsed_turn_ms: Option<f64>) -> Result<NextDueWatch, Code> {
        self.health.guarded("Database.nextDueWatch", || {
            let now = whole("nowMs", now_ms, 0.0)?.unwrap_or_else(now_unix_ms);
            let elapsed = whole("elapsedTurnMs", elapsed_turn_ms, 0.0)?.unwrap_or(0);
            let nd = self.inner.next_due_watch(now, elapsed);
            Ok(NextDueWatch {
                watch_id: nd.watch_id.map(|id| id.0 as f64),
                next_wake_delay_ms: nd.next_wake_delay_ms.map(|d| d as f64),
                settled: nd.settled,
            })
        })
    }

    #[napi]
    pub fn caught_up(&self, now_ms: Option<f64>, soon: Option<bool>) -> Result<bool, Code> {
        self.health.guarded("Database.caughtUp", || {
            let now = whole("nowMs", now_ms, 0.0)?.unwrap_or_else(now_unix_ms);
            Ok(self.inner.caught_up(now, soon.unwrap_or(false)))
        })
    }

    #[napi]
    pub fn watches_settled(&self) -> Result<bool, Code> {
        self.health.guarded("Database.watchesSettled", || Ok(self.inner.watches_settled()))
    }

    #[napi]
    pub fn stop_watches(&self) -> Result<(), Code> {
        self.health.unrefused("Database.stopWatches", || {
            self.inner.stop_watches();
            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    // Maintenance, Integrity, Backup, Dump, Close
    // -----------------------------------------------------------------------

    #[napi]
    pub fn drive_background_work(&self, max_entities: Option<f64>) -> Result<bool, Code> {
        self.health.guarded("Database.driveBackgroundWork", || {
            let max = whole("maxEntities", max_entities, 1.0)?.unwrap_or(1024) as usize;
            self.inner.drive_background_work(max).map_err(to_js)
        })
    }

    #[napi]
    pub fn flush(&self) -> Result<(), Code> {
        self.health.guarded("Database.flush", || self.inner.flush().map_err(to_js))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn flush_async(&self) -> Result<AsyncTask<Job<(), ()>>, Code> {
        self.health.guarded("Database.flushAsync", || {
            Ok(AsyncTask::new(Job::new(
                self.inner.again(),
                self.health.clone(),
                "Database.flushAsync",
                |db| db.flush(),
                jobs::nothing,
            )))
        })
    }

    #[napi(ts_return_type = "CheckReport")]
    pub fn check(&self, env: Env, index_name: Option<String>) -> Result<Unknown<'static>, Code> {
        self.health.guarded("Database.check", || {
            let rep = self.inner.check(index_name.as_deref()).map_err(to_js)?;
            to_js_json(&env, &rep)
        })
    }

    #[napi(ts_return_type = "Promise<CheckReport>")]
    pub fn check_async(
        &self,
        index_name: Option<String>,
    ) -> Result<AsyncTask<Job<serde_json::Value, Unknown<'static>>>, Code> {
        self.health.guarded("Database.checkAsync", || {
            Ok(AsyncTask::new(Job::new(
                self.inner.again(),
                self.health.clone(),
                "Database.checkAsync",
                move |db| {
                    let rep = db.check(index_name.as_deref())?;
                    serde_json::to_value(&rep)
                        .map_err(|e| ElmdbError::new(ErrorCode::Internal, format!("serialize check report: {e}")))
                },
                jobs::value,
            )))
        })
    }

    #[napi]
    pub fn rebuild(&self, index_name: Option<String>) -> Result<(), Code> {
        self.health.guarded("Database.rebuild", || self.inner.rebuild(index_name.as_deref()).map_err(to_js))
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn rebuild_async(&self, index_name: Option<String>) -> Result<AsyncTask<Job<(), ()>>, Code> {
        self.health.guarded("Database.rebuildAsync", || {
            Ok(AsyncTask::new(Job::new(
                self.inner.again(),
                self.health.clone(),
                "Database.rebuildAsync",
                move |db| db.rebuild(index_name.as_deref()),
                jobs::nothing,
            )))
        })
    }

    #[napi(ts_return_type = "DatabaseStats")]
    pub fn stats(&self, env: Env) -> Result<Unknown<'static>, Code> {
        self.health.guarded("Database.stats", || {
            let st = self.inner.stats().map_err(to_js)?;
            to_js_json(&env, &st)
        })
    }

    #[napi(ts_return_type = "Array<IndexStatus>")]
    pub fn index_status(&self, env: Env) -> Result<Unknown<'static>, Code> {
        self.health.guarded("Database.indexStatus", || {
            let st = self.inner.index_status().map_err(to_js)?;
            to_js_json(&env, &st)
        })
    }

    #[napi]
    pub fn backup(&self, target_dir: String, overwrite_backup: Option<bool>) -> Result<(), Code> {
        self.health.guarded("Database.backup", || {
            let mode = if overwrite_backup.unwrap_or(false) { BackupInto::Backups } else { BackupInto::New };
            self.inner.backup_into(&target_dir, mode).map_err(to_js)
        })
    }

    #[napi(ts_return_type = "Promise<void>")]
    pub fn backup_async(
        &self,
        target_dir: String,
        overwrite_backup: Option<bool>,
    ) -> Result<AsyncTask<Job<(), ()>>, Code> {
        self.health.guarded("Database.backupAsync", || {
            let mode = if overwrite_backup.unwrap_or(false) { BackupInto::Backups } else { BackupInto::New };
            Ok(AsyncTask::new(Job::new(
                self.inner.again(),
                self.health.clone(),
                "Database.backupAsync",
                move |db| db.backup_into(&target_dir, mode),
                jobs::nothing,
            )))
        })
    }

    #[napi(ts_return_type = "DumpReport")]
    pub fn dump(&self, env: Env, target_file: String) -> Result<Unknown<'static>, Code> {
        self.health.guarded("Database.dump", || {
            let rep = elmdb::dump::dump(&self.inner, &target_file).map_err(to_js)?;
            to_js_json(&env, &rep)
        })
    }

    #[napi]
    pub fn close(&self) -> Result<(), Code> {
        self.health.unrefused("Database.close", || {
            let hooks = std::mem::take(&mut *self.js_hooks.borrow_mut());
            for (engine_id, js_key) in hooks {
                let _ = self.inner.unregister_before_commit(engine_id);
                let _ = JS_HOOKS.try_with(|m| m.borrow_mut().remove(&js_key));
            }
            self.inner.close().map_err(to_js)
        })
    }
}
