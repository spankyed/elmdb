//! How a Node program collects elmdb's diagnostics (`elmdb::diagnostics`): an experiment behind this crate's
//! `diagnostics` feature, off by default, proving the engine's spans reach JavaScript before any public configuration
//! is chosen (`docs/evidence/diagnostics.md`).
//!
//! `collectDiagnostics()` installs, once per process, a collector keeping each of elmdb's spans as it closes;
//! `takeDiagnostics()` hands JavaScript what it kept since the last call, each span one JSON object. JavaScript asks
//! rather than being called, since spans close on the threads elmdb runs on, which are not JavaScript's. The collector
//! keeps at most `KEPT` spans, and a take says how many it dropped past that.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use napi_derive::napi;
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// The most spans kept between two takes
const KEPT: usize = 10_000;

/// A span not yet closed: its name, its parent's, its fields and how many handles hold it
struct Open {
    name: &'static str,
    parent: Option<&'static str>,
    fields: Map<String, Value>,
    handles: usize,
}

#[derive(Default)]
struct Collector {
    open: Mutex<HashMap<u64, Open>>,
    next: AtomicU64,
    closed: Mutex<VecDeque<String>>,
    dropped: AtomicU64,
}

thread_local! {
    /// The spans this thread has entered, innermost last
    static ENTERED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

struct Fields<'a>(&'a mut Map<String, Value>);

impl Visit for Fields<'_> {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), value.into());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}").into());
    }
}

impl Subscriber for Collector {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == elmdb::diagnostics::TARGET
    }
    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        let mut fields = Map::new();
        attributes.record(&mut Fields(&mut fields));
        let mut open = self.open.lock().expect("the collector's lock is never poisoned");
        let parent = ENTERED.with(|entered| entered.borrow().last().copied());
        let parent = parent.and_then(|at| open.get(&at)).map(|p| p.name);
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        open.insert(id, Open { name: attributes.metadata().name(), parent, fields, handles: 1 });
        Id::from_u64(id)
    }
    fn record(&self, span: &Id, values: &Record<'_>) {
        if let Some(open) = self.open.lock().expect("the collector's lock is never poisoned").get_mut(&span.into_u64())
        {
            values.record(&mut Fields(&mut open.fields));
        }
    }
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, span: &Id) {
        ENTERED.with(|entered| entered.borrow_mut().push(span.into_u64()));
    }
    fn exit(&self, _: &Id) {
        ENTERED.with(|entered| entered.borrow_mut().pop());
    }
    fn clone_span(&self, span: &Id) -> Id {
        if let Some(open) = self.open.lock().expect("the collector's lock is never poisoned").get_mut(&span.into_u64())
        {
            open.handles += 1;
        }
        span.clone()
    }
    fn try_close(&self, span: Id) -> bool {
        let mut open = self.open.lock().expect("the collector's lock is never poisoned");
        let Some(held) = open.get_mut(&span.into_u64()) else { return false };
        held.handles -= 1;
        if held.handles > 0 {
            return false;
        }
        let Open { name, parent, mut fields, .. } = open.remove(&span.into_u64()).expect("it was just found");
        drop(open);
        fields.insert("span".into(), name.into());
        if let Some(parent) = parent {
            fields.insert("parent".into(), parent.into());
        }
        let mut closed = self.closed.lock().expect("the collector's lock is never poisoned");
        if closed.len() == KEPT {
            closed.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        closed.push_back(Value::Object(fields).to_string());
        true
    }
}

static COLLECTOR: OnceLock<Arc<Collector>> = OnceLock::new();

/// Starts keeping elmdb's spans for `takeDiagnostics`, for the rest of the process. Refused when the process already
/// has a collector of another kind
#[napi(js_name = "collectDiagnostics")]
pub fn collect_diagnostics() -> napi::Result<()> {
    if COLLECTOR.get().is_some() {
        return Ok(());
    }
    let collector = Arc::new(Collector::default());
    tracing::subscriber::set_global_default(Arc::clone(&collector))
        .map_err(|e| napi::Error::from_reason(format!("This process already collects diagnostics: {e}")))?;
    let _ = COLLECTOR.set(collector);
    Ok(())
}

#[napi(object)]
pub struct Diagnostics {
    /// Each span closed since the last take, oldest first, as JSON: `span`, `parent` and its fields
    pub spans: Vec<String>,
    /// Spans closed past the most kept, and so not in `spans`
    pub dropped: f64,
}

/// What `collectDiagnostics` kept since the last take
#[napi(js_name = "takeDiagnostics")]
pub fn take_diagnostics() -> Diagnostics {
    let Some(collector) = COLLECTOR.get() else { return Diagnostics { spans: Vec::new(), dropped: 0.0 } };
    let spans = std::mem::take(&mut *collector.closed.lock().expect("the collector's lock is never poisoned"));
    Diagnostics { spans: spans.into(), dropped: collector.dropped.swap(0, Ordering::Relaxed) as f64 }
}
