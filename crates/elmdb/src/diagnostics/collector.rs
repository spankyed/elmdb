//! A collector of the engine's spans, for a program embedding elmdb to install: the engine never installs one. It keeps
//! each span of the target `elmdb` as it closes, one JSON object with its `span`, its `parent` and its fields, and hands
//! over what it kept with `take`. It keeps at most a given number, and a take says how many it dropped past that.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use super::TARGET;

/// A span not yet closed: its name, its parent's, its fields and how many handles hold it
struct Open {
    name: &'static str,
    parent: Option<&'static str>,
    fields: Map<String, Value>,
    handles: usize,
}

pub struct Collector {
    kept: usize,
    open: Mutex<HashMap<u64, Open>>,
    next: AtomicU64,
    closed: Mutex<VecDeque<String>>,
    dropped: AtomicU64,
}

/// What a collector kept since the last take
#[derive(Debug, Default, PartialEq)]
pub struct Taken {
    /// Each span closed, oldest first, as JSON
    pub spans: Vec<String>,
    /// Spans closed past the most kept, and so not in `spans`
    pub dropped: u64,
}

thread_local! {
    /// The spans this thread has entered, innermost last
    static ENTERED: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

impl Collector {
    /// A collector keeping at most `kept` spans between two takes
    pub fn new(kept: usize) -> Self {
        Self {
            kept,
            open: Mutex::default(),
            next: AtomicU64::new(0),
            closed: Mutex::default(),
            dropped: AtomicU64::new(0),
        }
    }

    /// What was kept since the last take
    pub fn take(&self) -> Taken {
        let spans = std::mem::take(&mut *self.closed.lock().expect("the collector's lock is never poisoned"));
        Taken { spans: spans.into(), dropped: self.dropped.swap(0, Ordering::Relaxed) }
    }
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
        metadata.target() == TARGET
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
        if closed.len() == self.kept {
            closed.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        closed.push_back(Value::Object(fields).to_string());
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{Value, json};

    use super::Collector;

    fn spans(collector: &Collector) -> Vec<Value> {
        collector.take().spans.iter().map(|s| serde_json::from_str(s).unwrap()).collect()
    }

    #[test]
    fn a_span_is_kept_once_its_last_handle_closes_with_its_parent_and_fields() {
        let collector = Arc::new(Collector::new(10));
        tracing::subscriber::with_default(Arc::clone(&collector), || {
            let outer = tracing::info_span!(target: "elmdb", "outer");
            outer.in_scope(|| {
                let inner = tracing::info_span!(target: "elmdb", "inner", durable = false, rows = 3u64);
                let held = inner.clone();
                drop(inner);
                assert!(spans(&collector).is_empty(), "a handle still holds it");
                drop(held);
            });
            assert_eq!(spans(&collector), [json!({"span":"inner","parent":"outer","durable":false,"rows":3})]);
            // Left, the outer span is no longer the parent of what starts next, though it is still open
            drop(tracing::info_span!(target: "elmdb", "after"));
            assert_eq!(spans(&collector), [json!({"span":"after"})]);
            drop(outer);
            tracing::info_span!(target: "another", "ignored").in_scope(|| {});
        });
        assert_eq!(spans(&collector), [json!({"span":"outer"})], "and nothing outside elmdb's target");
    }

    #[test]
    fn past_the_most_kept_the_oldest_are_dropped_and_counted() {
        let collector = Arc::new(Collector::new(10));
        tracing::subscriber::with_default(Arc::clone(&collector), || {
            for n in 0..15u64 {
                drop(tracing::info_span!(target: "elmdb", "n", rows = n));
            }
        });
        let taken = collector.take();
        assert_eq!((taken.spans.len(), taken.dropped), (10, 5));
        assert_eq!(taken.spans[0], r#"{"rows":5,"span":"n"}"#);
        assert_eq!(collector.take(), Default::default(), "a take empties what was kept, and the count");
    }
}
