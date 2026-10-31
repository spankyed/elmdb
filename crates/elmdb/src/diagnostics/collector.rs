//! A collector of the engine's spans, for a program embedding elmdb to install: the engine never installs one. It keeps
//! each span of the target `elmdb` as it closes, one JSON object with its `span` and `id`, its parent's `parent` and
//! `parent_id` when it has one, and its fields, and hands over what it kept with `take`. Ids are the collector's own,
//! from 1, so the spans a take hands over can be put back together as the tree they ran in. It keeps at most a given
//! number, and a take says how many it dropped past that.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use super::TARGET;

/// A span not yet closed: its name, its parent's id and name, its fields and how many handles hold it
struct Open {
    name: &'static str,
    parent: Option<(u64, &'static str)>,
    fields: Map<String, Value>,
    handles: usize,
}

pub struct Collector {
    /// This collector among every one in the process, so the spans a thread entered are looked up in their own's
    instance: u64,
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

/// The next collector's instance
static INSTANCES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The spans this thread has entered, innermost last, each with the instance of the collector whose id it is: two
    /// collectors on one thread number their spans alike
    static ENTERED: RefCell<Vec<(u64, u64)>> = const { RefCell::new(Vec::new()) };
}

impl Collector {
    /// A collector keeping at most `kept` spans between two takes
    pub fn new(kept: usize) -> Self {
        Self {
            instance: INSTANCES.fetch_add(1, Ordering::Relaxed),
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
        // A parent given explicitly, none for a root, or else the innermost span this thread entered of this collector's
        let parent = if let Some(parent) = attributes.parent() {
            Some(parent.into_u64())
        } else if attributes.is_root() {
            None
        } else {
            ENTERED.with(|entered| {
                entered.borrow().iter().rev().find(|(instance, _)| *instance == self.instance).map(|(_, id)| *id)
            })
        };
        let parent = parent.and_then(|at| open.get(&at).map(|p| (at, p.name)));
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
        ENTERED.with(|entered| entered.borrow_mut().push((self.instance, span.into_u64())));
    }
    /// Removes the span exited, the innermost entry of it, so a span exited out of order leaves the others entered
    fn exit(&self, span: &Id) {
        let exited = (self.instance, span.into_u64());
        ENTERED.with(|entered| {
            let mut entered = entered.borrow_mut();
            if let Some(at) = entered.iter().rposition(|e| *e == exited) {
                entered.remove(at);
            }
        });
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
        fields.insert("id".into(), span.into_u64().into());
        if let Some((id, parent)) = parent {
            fields.insert("parent".into(), parent.into());
            fields.insert("parent_id".into(), id.into());
        }
        let text = Value::Object(fields).to_string();
        let mut closed = self.closed.lock().expect("the collector's lock is never poisoned");
        if self.kept == 0 {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        if closed.len() >= self.kept {
            closed.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        closed.push_back(text);
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
            assert_eq!(
                spans(&collector),
                [json!({"span":"inner","id":2,"parent":"outer","parent_id":1,"durable":false,"rows":3})]
            );
            // Left, the outer span is no longer the parent of what starts next, though it is still open
            drop(tracing::info_span!(target: "elmdb", "after"));
            assert_eq!(spans(&collector), [json!({"span":"after","id":3})]);
            drop(outer);
            tracing::info_span!(target: "another", "ignored").in_scope(|| {});
        });
        assert_eq!(spans(&collector), [json!({"span":"outer","id":1})], "and nothing outside elmdb's target");
    }

    #[test]
    fn a_parent_given_explicitly_or_none_is_the_one_kept() {
        let collector = Arc::new(Collector::new(10));
        tracing::subscriber::with_default(Arc::clone(&collector), || {
            let named = tracing::info_span!(target: "elmdb", "named");
            let other = tracing::info_span!(target: "elmdb", "other");
            other.in_scope(|| {
                drop(tracing::info_span!(target: "elmdb", parent: &named, "child"));
                drop(tracing::info_span!(target: "elmdb", parent: None, "root"));
            });
        });
        let heard = spans(&collector);
        assert_eq!(heard[0], json!({"span":"child","id":3,"parent":"named","parent_id":1}));
        assert_eq!(heard[1], json!({"span":"root","id":4}));
    }

    #[test]
    fn two_collectors_on_one_thread_keep_their_own_parents() {
        let first = Arc::new(Collector::new(10));
        let second = Arc::new(Collector::new(10));
        tracing::subscriber::with_default(Arc::clone(&first), || {
            // The first's span 1 is entered while the second makes its own span 1 and its child
            tracing::info_span!(target: "elmdb", "first").in_scope(|| {
                tracing::subscriber::with_default(Arc::clone(&second), || {
                    let solo = tracing::info_span!(target: "elmdb", "solo");
                    drop(tracing::info_span!(target: "elmdb", parent: &solo, "under"));
                    drop(solo);
                    drop(tracing::info_span!(target: "elmdb", "after"));
                });
            });
        });
        let heard = spans(&second);
        assert_eq!(heard[0], json!({"span":"under","id":2,"parent":"solo","parent_id":1}));
        assert_eq!(heard[2], json!({"span":"after","id":3}), "the first's entered span is not the second's parent");
    }

    #[test]
    fn a_span_exited_out_of_order_leaves_the_other_entered() {
        let collector = Arc::new(Collector::new(10));
        tracing::subscriber::with_default(Arc::clone(&collector), || {
            let a = tracing::info_span!(target: "elmdb", "a");
            let b = tracing::info_span!(target: "elmdb", "b");
            let entered_a = a.enter();
            let entered_b = b.enter();
            drop(entered_a);
            drop(tracing::info_span!(target: "elmdb", "inside_b"));
            drop(entered_b);
        });
        assert_eq!(spans(&collector)[0], json!({"span":"inside_b","id":3,"parent":"b","parent_id":2}));
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
        assert_eq!(taken.spans[0], r#"{"rows":5,"span":"n","id":6}"#);
        assert_eq!(collector.take(), Default::default(), "a take empties what was kept, and the count");
    }

    #[test]
    fn a_collector_keeping_none_counts_every_span_dropped() {
        let collector = Arc::new(Collector::new(0));
        tracing::subscriber::with_default(Arc::clone(&collector), || {
            for _ in 0..3 {
                drop(tracing::info_span!(target: "elmdb", "n"));
            }
        });
        assert_eq!(collector.take(), super::Taken { spans: Vec::new(), dropped: 3 });
    }
}
