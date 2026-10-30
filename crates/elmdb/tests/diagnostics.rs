//! The engine's diagnostics (`src/diagnostics.rs`): it never switches collection on itself, a collector the embedding
//! program installs hears each instrumented operation with its phases and outcome, and what it hears names no data.
//! Built with the `diagnostics` feature; nextest runs it in a process of its own, so nothing else set a collector.
#![cfg(feature = "diagnostics")]

// The benchmarks' fixture, of which this uses only the database
#[allow(dead_code)]
mod support;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use elmdb::diagnostics::{TARGET, names};
use elmdb::engine::batch::Batch;
use serde_json::json;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// A span as a collector heard it: its name, its parent's name and every field recorded on it
#[derive(Debug, Clone)]
struct Heard {
    name: &'static str,
    parent: Option<&'static str>,
    fields: BTreeMap<&'static str, String>,
}

/// A collector keeping every span of elmdb's it hears
#[derive(Default, Clone)]
struct Collector {
    spans: Arc<Mutex<Vec<Heard>>>,
    next: Arc<AtomicU64>,
    stack: Arc<Mutex<Vec<u64>>>,
}

struct Fields<'a>(&'a mut BTreeMap<&'static str, String>);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name(), value.to_owned());
    }
}

impl Subscriber for Collector {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == TARGET
    }
    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        let mut fields = BTreeMap::new();
        attributes.record(&mut Fields(&mut fields));
        let mut spans = self.spans.lock().unwrap();
        let parent = self.stack.lock().unwrap().last().map(|&at| spans[at as usize - 1].name);
        spans.push(Heard { name: attributes.metadata().name(), parent, fields });
        Id::from_u64(self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }
    fn record(&self, span: &Id, values: &Record<'_>) {
        let mut spans = self.spans.lock().unwrap();
        values.record(&mut Fields(&mut spans[span.into_u64() as usize - 1].fields));
    }
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, span: &Id) {
        self.stack.lock().unwrap().push(span.into_u64());
    }
    fn exit(&self, _: &Id) {
        self.stack.lock().unwrap().pop();
    }
}

impl Collector {
    fn heard(&self) -> Vec<Heard> {
        self.spans.lock().unwrap().clone()
    }
}

/// Every instrumented operation once, answering what each returned: a write, a rebuild of one index, one of an index
/// the schema doesn't declare, every index, and a unit begun and committed
fn operations(fixture: &support::Fixture) -> Vec<String> {
    let set = Batch {
        ops: vec![
            serde_json::from_value(json!({"op":"set","id":"Doc-00000000","fields":{"body":"secret words"}})).unwrap(),
        ],
        now: None,
    };
    let db = &fixture.db;
    let mut unit = db.begin().unwrap();
    unit.write(&set).unwrap();
    unit.commit(false).unwrap();
    vec![
        format!("{:?}", db.write(&set, false).map(|(result, _)| result)),
        format!("{:?}", db.rebuild(Some(("Doc", "tags")))),
        format!("{:?}", db.rebuild(Some(("Doc", "missing"))).map_err(|e| e.code)),
        format!("{:?}", db.rebuild(None)),
    ]
}

#[test]
fn the_engine_never_installs_a_collector_and_one_installed_hears_every_operation() {
    let fixture = support::Fixture::new(200, true, false);

    let uncollected = operations(&fixture);
    assert!(!tracing::dispatcher::has_been_set(), "no operation may install a collector of its own");

    let collector = Collector::default();
    let collected = tracing::subscriber::with_default(collector.clone(), || operations(&fixture));
    assert_eq!(collected, uncollected, "collecting changes no operation's answer");

    let heard = collector.heard();
    let named = |name: &str| heard.iter().filter(|h| h.name == name).cloned().collect::<Vec<_>>();
    let field = |h: &Heard, f: &str| h.fields.get(f).cloned().unwrap_or_default();

    let begun = named(names::BEGIN);
    assert_eq!(begun.len(), 1);
    assert_eq!(field(&begun[0], names::OUTCOME), "begun");
    assert!(begun[0].fields.contains_key(names::WAITED_US));

    // The plain write's: the rebuilds' own writes are their children
    let writes: Vec<_> = named(names::WRITE).into_iter().filter(|h| h.parent.is_none()).collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(field(&writes[0], "durable"), "false");
    assert_eq!(field(&writes[0], names::OUTCOME), "committed");
    assert!(writes[0].fields.contains_key(names::WAITED_US) && writes[0].fields.contains_key(names::ELAPSED_US));

    let rebuilds = named(names::REBUILD);
    assert_eq!(rebuilds.len(), 3);
    assert_eq!((field(&rebuilds[0], "entity_type"), field(&rebuilds[0], "index")), ("Doc".into(), "tags".into()));
    assert_eq!(field(&rebuilds[0], names::OUTCOME), "durable");
    assert_eq!(field(&rebuilds[1], names::OUTCOME), "failed");
    assert_eq!(field(&rebuilds[1], names::ERROR), "UnknownIndex");
    assert_eq!(field(&rebuilds[2], names::OUTCOME), "durable");

    let sorts = named(names::SORT);
    assert!(
        !sorts.is_empty() && sorts.iter().all(|s| s.parent == Some(names::REBUILD)),
        "a rebuild's sorts are its own"
    );
    assert!(sorts.iter().all(|s| field(s, names::OUTCOME) == "sorted"));
    // One index's entries: eight tags for each of the 200 entities
    assert_eq!(field(&sorts[0], names::ROWS), "1600");
    assert!(sorts.iter().all(|s| s.fields.contains_key(names::RUNS) && s.fields.contains_key(names::SPILLED_BYTES)));

    for h in &heard {
        for value in h.fields.values() {
            assert!(
                !value.contains("Doc-") && !value.contains("secret") && !value.contains("tag0"),
                "{h:?} names data"
            );
        }
    }
}
