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
    BackupInto, BuildPolicy, DurabilityMode, EdgeDir, ExecBudgets, TransactOptions as EngineTransactOptions, TypeMask,
    WatchId, WatchMode, WatchPolicy,
};
use elmdb_wire::{ElmdbError, ErrorCode};
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

/// Unified `serde_json::Value -> napi::Unknown` converter used by every N-API surface (sync, async,
/// and `beforeCommit` hooks) so finite JSON integers `> u32::MAX` (timestamps, `mapSizeBytes`, safe
/// integers like `5e9`) consistently cross as JS `number` matching `.d.ts` and `JSON.stringify`.
pub(crate) fn json_value_to_js(env: &Env, val: serde_json::Value) -> napi::Result<Unknown<'static>> {
    // SAFETY: `env.raw()` is the active N-API environment on the current JS thread.
    let raw = unsafe { <serde_json::Value as ToNapiValue>::to_napi_value(env.raw(), val)? };
    // SAFETY: `raw` was produced on `env.raw()` by `ToNapiValue::to_napi_value`.
    unsafe { Unknown::from_napi_value(env.raw(), raw) }
}

fn to_js_json(env: &Env, v: &impl serde::Serialize) -> Result<Unknown<'static>, Code> {
    let val = serde_json::to_value(v)
        .map_err(|e| napi::Error::new(Code(ErrorCode::Internal.as_str()), format!("serialize failed: {e}")))?;
    json_value_to_js(env, val).map_err(|e| napi::Error::new(Code("GenericFailure"), e.reason.clone()))
}

/// Normalizes exact whole-number JSON floats produced by `napi-rs` for JS `number` values outside `i32`
/// (such as `Date.now()` timestamps or 53-bit seeds) into JSON integers before typed `serde` parsing.
/// JSON integers still deserialize losslessly into `f64`/`f32` fields.
fn normalize_js_numbers(val: &mut serde_json::Value) {
    match val {
        serde_json::Value::Number(n) => {
            if n.as_i64().is_none()
                && n.as_u64().is_none()
                && let Some(f) = n.as_f64()
                && f.is_finite()
                && f.fract() == 0.0
            {
                if (0.0..=MAX_SAFE_JS_INT).contains(&f) {
                    *n = serde_json::Number::from(f as u64);
                } else if (-MAX_SAFE_JS_INT..0.0).contains(&f) {
                    *n = serde_json::Number::from(f as i64);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_js_numbers(item);
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                normalize_js_numbers(v);
            }
        }
        _ => {}
    }
}

fn parse_query_spec(mut spec: serde_json::Value) -> Result<QuerySpec, Code> {
    normalize_js_numbers(&mut spec);
    serde_json::from_value(spec).map_err(|e| bad_arg(format!("invalid query spec: {e}")))
}

fn parse_schema_json(mut schema: serde_json::Value) -> Result<Schema, Code> {
    normalize_js_numbers(&mut schema);
    serde_json::from_value(schema).map_err(|e| bad_arg(format!("invalid schema JSON: {e}")))
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

/// Options for `Database.transact`.
#[napi(object)]
pub struct TransactOptions {
    /// Whether registered `beforeCommit` hooks run when the outermost transaction commits (default `true`).
    pub run_before_commit: Option<bool>,
}

fn take_pending_exception(raw_env: napi::sys::napi_env) -> Option<napi::sys::napi_value> {
    let mut is_pending = false;
    // SAFETY: `raw_env` is the active N-API environment on the current JS thread.
    if unsafe { napi::sys::napi_is_exception_pending(raw_env, &mut is_pending) } == napi::sys::Status::napi_ok
        && is_pending
    {
        let mut raw_exc = std::ptr::null_mut();
        if unsafe { napi::sys::napi_get_and_clear_last_exception(raw_env, &mut raw_exc) } == napi::sys::Status::napi_ok
            && !raw_exc.is_null()
        {
            return Some(raw_exc);
        }
    }
    None
}

fn clear_pending_exception(raw_env: napi::sys::napi_env) {
    let _ = take_pending_exception(raw_env);
}

fn extract_hook_error_value(
    raw_env: napi::sys::napi_env,
    raw_exc: Option<napi::sys::napi_value>,
    err: napi::Error,
) -> ElmdbError {
    let mut js_code: Option<String> = None;
    let mut extracted: Option<String> = None;

    // SAFETY: `raw_env` is the active N-API environment on the current JS thread and `raw_exc` came from it.
    if let Some(raw_exc) = raw_exc
        && let Ok(exc) = unsafe { Unknown::from_napi_value(raw_env, raw_exc) }
    {
        if let Ok(vtype) = exc.get_type()
            && matches!(vtype, ValueType::Object | ValueType::Function)
            && let Ok(obj) = exc.coerce_to_object()
        {
            if let Ok(code_val) = obj.get_named_property_unchecked::<Unknown<'_>>("code")
                && matches!(code_val.get_type(), Ok(ValueType::String))
                && let Ok(js_str) = code_val.coerce_to_string()
                && let Ok(utf8) = js_str.into_utf8()
                && let Ok(s) = utf8.as_str()
            {
                js_code = Some(s.to_owned());
            }
            // Clear any exception thrown by a hostile `.code` getter before reading `.message`.
            clear_pending_exception(raw_env);

            if let Ok(msg_val) = obj.get_named_property_unchecked::<Unknown<'_>>("message")
                && matches!(msg_val.get_type(), Ok(ValueType::String))
                && let Ok(js_str) = msg_val.coerce_to_string()
                && let Ok(utf8) = js_str.into_utf8()
                && let Ok(s) = utf8.as_str()
                && !s.is_empty()
            {
                extracted = Some(s.to_owned());
            }
            // If reading `.message` threw a hostile getter error, clear it before falling back.
            clear_pending_exception(raw_env);
        }

        if extracted.is_none() {
            if let Ok(js_str) = exc.coerce_to_string()
                && let Ok(utf8) = js_str.into_utf8()
                && let Ok(s) = utf8.as_str()
                && !s.is_empty()
            {
                extracted = Some(s.to_owned());
            }
            clear_pending_exception(raw_env);
        }
    }

    let message = extracted
        .or_else(|| (!err.reason.is_empty()).then_some(err.reason))
        .unwrap_or_else(|| "beforeCommit hook aborted".to_owned());
    let code = match js_code.as_deref() {
        Some(c) if c == ErrorCode::AsyncCallbackForbidden.as_str() => ErrorCode::AsyncCallbackForbidden,
        Some(c) if c == ErrorCode::StorageFull.as_str() && message.starts_with("storage map is full") => {
            ErrorCode::StorageFull
        }
        _ => ErrorCode::HookAborted,
    };
    ElmdbError::new(code, message)
}

/// Clears any pending N-API exception raised by a JS `beforeCommit` hook, extracts its `.message`
/// (or `String(err)` fallback), and returns `ElmdbError(HookAborted, message)` with a clean N-API env.
fn extract_and_clear_hook_error(env: &Env, err: napi::Error) -> ElmdbError {
    let raw_env = env.raw();
    let raw_exc = take_pending_exception(raw_env);
    extract_hook_error_value(raw_env, raw_exc, err)
}

/// Calls a synchronous JS `beforeCommit` callback via raw `napi_call_function` so any thrown JS value
/// (Error objects, primitive strings/numbers, or objects with throwing getters) is inspected and cleared
/// directly without collapsing primitives to `"Create Error reference failed"`.
fn call_js_hook_sync<'env>(
    env: &'env Env,
    func: Function<'env, serde_json::Value, Unknown<'static>>,
    payload: serde_json::Value,
) -> elmdb::Result<Unknown<'static>> {
    let raw_env = env.raw();
    let arg_val = json_value_to_js(env, payload).map_err(|e| extract_and_clear_hook_error(env, e))?;
    let raw_arg = arg_val.raw();
    let mut recv = std::ptr::null_mut();
    let mut ret = std::ptr::null_mut();
    // SAFETY: `raw_env`, `func.raw()`, and `raw_arg` are valid N-API handles on this JS thread.
    let recv_status = unsafe { napi::sys::napi_get_undefined(raw_env, &mut recv) };
    if recv_status != napi::sys::Status::napi_ok {
        return Err(extract_and_clear_hook_error(env, napi::Error::from_status(napi::Status::from(recv_status))));
    }
    let status = unsafe { napi::sys::napi_call_function(raw_env, recv, func.raw(), 1, &raw_arg, &mut ret) };
    if status != napi::sys::Status::napi_ok {
        return Err(extract_and_clear_hook_error(env, napi::Error::from_status(napi::Status::from(status))));
    }
    // SAFETY: `ret` was initialized by a successful `napi_call_function`.
    unsafe { Unknown::from_napi_value(raw_env, ret) }.map_err(|e| extract_and_clear_hook_error(env, e))
}

enum ThenableCheck {
    Value(Unknown<'static>),
    AsyncForbidden,
    JsThrown(Option<napi::sys::napi_value>, napi::Error),
}

/// Checks whether `raw_val` is a Promise or thenable using raw N-API calls so any exception thrown
/// by a hostile `.then` getter (including primitives) is captured and cleared cleanly without
/// leaving a pending exception active on `raw_env`.
fn check_not_thenable_raw(raw_env: napi::sys::napi_env, raw_val: napi::sys::napi_value) -> ThenableCheck {
    let fail = |status: napi::sys::napi_status| -> ThenableCheck {
        let raw_exc = take_pending_exception(raw_env);
        ThenableCheck::JsThrown(raw_exc, napi::Error::from_status(napi::Status::from(status)))
    };

    let mut is_promise = false;
    // SAFETY: `raw_env` and `raw_val` are valid handles on the current JS thread.
    let st = unsafe { napi::sys::napi_is_promise(raw_env, raw_val, &mut is_promise) };
    if st != napi::sys::Status::napi_ok {
        return fail(st);
    }
    if is_promise {
        return ThenableCheck::AsyncForbidden;
    }

    let mut vtype = napi::sys::ValueType::napi_undefined;
    // SAFETY: `raw_env` and `raw_val` are valid handles on the current JS thread.
    let st = unsafe { napi::sys::napi_typeof(raw_env, raw_val, &mut vtype) };
    if st != napi::sys::Status::napi_ok {
        return fail(st);
    }
    if vtype == napi::sys::ValueType::napi_object || vtype == napi::sys::ValueType::napi_function {
        let mut has_then = false;
        // SAFETY: `c"then"` is NUL-terminated; `raw_val` is an object/function.
        let st = unsafe { napi::sys::napi_has_named_property(raw_env, raw_val, c"then".as_ptr(), &mut has_then) };
        if st != napi::sys::Status::napi_ok {
            return fail(st);
        }
        if has_then {
            let mut then_val = std::ptr::null_mut();
            // SAFETY: `c"then"` is NUL-terminated; `raw_val` is an object/function.
            let st = unsafe { napi::sys::napi_get_named_property(raw_env, raw_val, c"then".as_ptr(), &mut then_val) };
            if st != napi::sys::Status::napi_ok {
                return fail(st);
            }
            let mut then_type = napi::sys::ValueType::napi_undefined;
            let st = unsafe { napi::sys::napi_typeof(raw_env, then_val, &mut then_type) };
            if st != napi::sys::Status::napi_ok {
                return fail(st);
            }
            if then_type == napi::sys::ValueType::napi_function {
                return ThenableCheck::AsyncForbidden;
            }
        }
    }

    // SAFETY: `raw_val` is a valid JS value on `raw_env`.
    match unsafe { Unknown::from_napi_value(raw_env, raw_val) } {
        Ok(val) => ThenableCheck::Value(val),
        Err(e) => ThenableCheck::JsThrown(take_pending_exception(raw_env), e),
    }
}

fn check_not_thenable(env: &Env, val: Unknown<'static>) -> ThenableCheck {
    check_not_thenable_raw(env.raw(), val.raw())
}

fn counter_to_js_safe(val: u64) -> Result<f64, Code> {
    if val > MAX_SAFE_JS_INT as u64 {
        return Err(to_js(ElmdbError::new(
            ErrorCode::ResourceLimit,
            format!("counter value {val} exceeds JS safe integer range"),
        )));
    }
    Ok(val as f64)
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
            let parsed = parse_schema_json(s)?;
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
            let parsed = parse_schema_json(schema)?;
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

    /// Executes a strictly-synchronous callback inside a write transaction (or native LMDB savepoint when nested).
    ///
    /// - Reads (`get`, `getEdges`, `query`, `getCounter`) and writes (`executeWireBatch`, `nextCounter`, `setCounter`)
    ///   see all uncommitted outer and inner writes immediately (RYOW).
    /// - Throwing rolls back this transaction (or nested savepoint only, preserving outer writes).
    /// - Returning a Promise or thenable aborts immediately with `ASYNC_CALLBACK_FORBIDDEN`.
    /// - `beforeCommit` hooks run once on the outermost commit unless `{ runBeforeCommit: false }`.
    #[napi(ts_return_type = "unknown")]
    pub fn transact(
        &self,
        env: Env,
        #[napi(ts_arg_type = "() => unknown")] callback: Function<(), Unknown<'static>>,
        options: Option<TransactOptions>,
    ) -> napi::Result<Unknown<'static>> {
        self.health.guarded_raw(&env, "Database.transact", || {
            let opts =
                EngineTransactOptions { run_before_commit: options.and_then(|o| o.run_before_commit).unwrap_or(true) };
            let raw_env = env.raw();
            let raw_fn = callback.raw();
            let mut thrown_raw_exc: Option<napi::sys::napi_value> = None;
            let mut thrown_js: Option<napi::Error> = None;

            let outcome = self.inner.transact_scoped(opts, || {
                let mut recv = std::ptr::null_mut();
                let mut ret = std::ptr::null_mut();
                // SAFETY: `raw_env` and `raw_fn` are valid N-API handles on this JS thread.
                let recv_st = unsafe { napi::sys::napi_get_undefined(raw_env, &mut recv) };
                if recv_st != napi::sys::Status::napi_ok {
                    thrown_raw_exc = take_pending_exception(raw_env);
                    thrown_js = Some(napi::Error::from_status(napi::Status::from(recv_st)));
                    return Err(ElmdbError::new(ErrorCode::Cancelled, "JS transaction receiver lookup failed"));
                }
                let call_st =
                    unsafe { napi::sys::napi_call_function(raw_env, recv, raw_fn, 0, std::ptr::null(), &mut ret) };
                if call_st != napi::sys::Status::napi_ok {
                    thrown_raw_exc = take_pending_exception(raw_env);
                    thrown_js = Some(napi::Error::from_status(napi::Status::from(call_st)));
                    return Err(ElmdbError::new(ErrorCode::Cancelled, "JS transaction callback threw"));
                }
                let val = match check_not_thenable_raw(raw_env, ret) {
                    ThenableCheck::Value(v) => v,
                    ThenableCheck::AsyncForbidden => {
                        return Err(ElmdbError::new(
                            ErrorCode::AsyncCallbackForbidden,
                            "synchronous callback must not return a Promise or thenable",
                        ));
                    }
                    ThenableCheck::JsThrown(raw_exc, js_err) => {
                        thrown_raw_exc = raw_exc;
                        thrown_js = Some(js_err);
                        return Err(ElmdbError::new(
                            ErrorCode::Cancelled,
                            "JS getter threw while inspecting return value",
                        ));
                    }
                };
                // Re-check handle health AFTER both callback execution and `.then` inspection (which can
                // run arbitrary JS getters): a poisoned handle must NEVER commit partial writes!
                if let Some(refused) = self.health.refusal("Database.transact") {
                    return Err(refused);
                }
                Ok(val)
            });

            match outcome {
                Ok(val) => Ok(val),
                Err(elmdb_err) => {
                    if let Some(raw_exc) = thrown_raw_exc {
                        // SAFETY: Rethrow the exact captured JS value (Error or primitive) verbatim.
                        let _ = unsafe { napi::sys::napi_throw(raw_env, raw_exc) };
                        Err(napi::Error::from_status(napi::Status::PendingException))
                    } else if let Some(js_err) = thrown_js {
                        Err(js_err)
                    } else {
                        Err(napi::Error::from(JsError::from(to_js(elmdb_err)).into_unknown(env)))
                    }
                }
            }
        })
    }

    /// Returns the pending commit sequence while inside `transact()` or `beforeCommit`, or `null` outside a write txn.
    #[napi]
    pub fn pending_commit_seq(&self) -> Result<Option<f64>, Code> {
        self.health.guarded("Database.pendingCommitSeq", || Ok(self.inner.pending_commit_seq().map(|s| s as f64)))
    }

    /// Atomically increments and returns the named persistent counter (`1, 2, 3, ...`).
    /// Participates in the active `transact()` / `beforeCommit` savepoint and rolls back on abort.
    #[napi]
    pub fn next_counter(&self, name: String) -> Result<f64, Code> {
        self.health.guarded("Database.nextCounter", || {
            let val = self.inner.next_counter(&name).map_err(to_js)?;
            counter_to_js_safe(val)
        })
    }

    /// Reads the current value of the named persistent counter (`0` if unset), with RYOW inside `transact()`.
    #[napi]
    pub fn get_counter(&self, name: String) -> Result<f64, Code> {
        self.health.guarded("Database.getCounter", || {
            let val = self.inner.get_counter(&name).map_err(to_js)?;
            counter_to_js_safe(val)
        })
    }

    /// Sets the named persistent counter (e.g. preserving imported high-water marks).
    /// Participates in the active `transact()` / `beforeCommit` savepoint and rolls back on abort.
    #[napi]
    pub fn set_counter(&self, name: String, value: f64) -> Result<(), Code> {
        self.health.guarded("Database.setCounter", || {
            let val = whole("value", Some(value), 0.0)?.expect("present");
            self.inner.set_counter(&name, val).map_err(to_js)
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
    /// The callback receives `{ commitSeq, entities, edges }` inside a native LMDB nested savepoint:
    /// - Full read-your-own-writes (`get`, `getEdges`, `query`, `getCounter`, `nextCounter`, `setCounter`,
    ///   `pendingCommitSeq`, `executeWireBatch`, and nested `transact`) are available directly inside the hook.
    /// - Throwing an error aborts the commit cleanly with `HOOK_ABORTED`.
    /// - Returning a `Uint8Array`/`Buffer` containing an `elmdb-wire` batch applies those derived writes
    ///   in the same savepoint cascade (up to `MAX_HOOK_ROUNDS`).
    #[napi]
    pub fn register_before_commit(
        &self,
        env: Env,
        type_names: Option<Vec<String>>,
        #[napi(
            ts_arg_type = "(delta: { commitSeq: number; entities: Array<{ id: string; typeName: string; op: 'create' | 'update' | 'trash' | 'restore' | 'destroy' }>; edges: Array<{ relKind: string; op: 'link' | 'unlink' | 'update'; sourceId: string; targetId: string }> }) => unknown"
        )]
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
            let hook_health = self.health.clone();

            let register_res = self.inner.register_host_before_commit(
                wants,
                Arc::new(move |delta| {
                    if std::thread::current().id() != expected_thread {
                        return Err(ElmdbError::new(
                            ErrorCode::HookAborted,
                            "JS beforeCommit hook cannot run from an off-thread write",
                        ));
                    }
                    let entities: Vec<serde_json::Value> = delta
                        .entities
                        .iter()
                        .map(|d| {
                            serde_json::json!({
                                "id": d.id,
                                "typeName": d.type_name,
                                "op": d.op,
                            })
                        })
                        .collect();
                    let edges: Vec<serde_json::Value> = delta
                        .edges
                        .iter()
                        .map(|e| {
                            serde_json::json!({
                                "relKind": e.rel_kind,
                                "op": e.op,
                                "sourceId": e.source_id,
                                "targetId": e.target_id,
                            })
                        })
                        .collect();
                    let payload = serde_json::json!({
                        "commitSeq": delta.commit_seq,
                        "entities": entities,
                        "edges": edges,
                    });

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
                                    let call_res = call_js_hook_sync(&js_env, func, payload);
                                    if let Some(refused) = hook_health.refusal("Database.beforeCommit") {
                                        return Err(refused);
                                    }
                                    let ret = call_res?;
                                    let ret = match check_not_thenable(&js_env, ret) {
                                        ThenableCheck::Value(v) => v,
                                        ThenableCheck::AsyncForbidden => {
                                            return Err(ElmdbError::new(
                                                ErrorCode::AsyncCallbackForbidden,
                                                "beforeCommit hooks must be synchronous (Promises and async callbacks are forbidden)",
                                            ));
                                        }
                                        ThenableCheck::JsThrown(raw_exc, e) => {
                                            if let Some(refused) = hook_health.refusal("Database.beforeCommit") {
                                                return Err(refused);
                                            }
                                            return Err(extract_hook_error_value(js_env.raw(), raw_exc, e));
                                        }
                                    };
                                    // Re-check handle health AFTER `.then` inspection too: a hostile `.then` getter
                                    // can trigger and swallow a nested panic before returning a wire batch!
                                    if let Some(refused) = hook_health.refusal("Database.beforeCommit") {
                                        return Err(refused);
                                    }
                                    let vtype = ret
                                        .get_type()
                                        .map_err(|e| extract_and_clear_hook_error(&js_env, e))?;
                                    if matches!(vtype, ValueType::Undefined | ValueType::Null) {
                                        Ok(None)
                                    } else {
                                        let bytes = Uint8Array::from_unknown(ret).map_err(|e| {
                                            clear_pending_exception(js_env.raw());
                                            ElmdbError::new(
                                                ErrorCode::InvalidArgument,
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

                    maybe_frame
                }),
            );

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
            let parsed = parse_query_spec(spec)?;
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
            let parsed = parse_query_spec(spec)?;
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
            let parsed = parse_query_spec(spec)?;
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
            let parsed = parse_query_spec(spec)?;
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
    pub fn begin_watch_run(&self, watch_id: f64) -> Result<bool, Code> {
        self.health.guarded("Database.beginWatchRun", || {
            let id = WatchId(whole("watchId", Some(watch_id), 1.0)?.expect("present"));
            Ok(self.inner.begin_watch_run(id))
        })
    }

    #[napi]
    pub fn abort_watch_run(&self, watch_id: f64, requeue_dirty: bool, now_ms: Option<f64>) -> Result<bool, Code> {
        self.health.guarded("Database.abortWatchRun", || {
            let id = WatchId(whole("watchId", Some(watch_id), 1.0)?.expect("present"));
            let now = whole("nowMs", now_ms, 0.0)?.unwrap_or_else(now_unix_ms);
            Ok(self.inner.abort_watch_run(id, requeue_dirty, now))
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
            // Close the underlying engine FIRST so rejecting `close()` inside `transact()`
            // never wipes registered JS `beforeCommit` hooks on a live database!
            self.inner.close().map_err(to_js)?;
            let hooks = std::mem::take(&mut *self.js_hooks.borrow_mut());
            for (engine_id, js_key) in hooks {
                let _ = self.inner.unregister_before_commit(engine_id);
                let _ = JS_HOOKS.try_with(|m| m.borrow_mut().remove(&js_key));
            }
            Ok(())
        })
    }
}
