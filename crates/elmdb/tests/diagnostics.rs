//! The engine's diagnostics (`src/diagnostics.rs`): it never switches collection on itself, a collector the embedding
//! program installs hears each instrumented operation with its phases and outcome, and what it hears names no data.
//! Built with the `diagnostics` feature; nextest runs it in a process of its own, so nothing else set a collector.
#![cfg(feature = "diagnostics")]

// The benchmarks' fixture, of which this uses only the database
#[allow(dead_code)]
mod support;

use std::sync::Arc;

use elmdb::diagnostics::{Collector, names};
use elmdb::engine::batch::Batch;
use serde_json::{Value, json};

/// A span as elmdb's collector kept it
struct Heard(Value);

impl Heard {
    fn name(&self) -> &str {
        self.0["span"].as_str().unwrap()
    }
    fn parent(&self) -> Option<&str> {
        self.0["parent"].as_str()
    }
    /// A field as text, empty when the span doesn't have it
    fn field(&self, field: &str) -> String {
        match &self.0[field] {
            Value::Null => String::new(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }
    fn has(&self, field: &str) -> bool {
        self.0.get(field).is_some()
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

    let collector = Arc::new(Collector::new(1000));
    let collected = tracing::subscriber::with_default(Arc::clone(&collector), || operations(&fixture));
    assert_eq!(collected, uncollected, "collecting changes no answer");

    let taken = collector.take();
    assert_eq!(taken.dropped, 0);
    let heard: Vec<Heard> = taken.spans.iter().map(|span| Heard(serde_json::from_str(span).unwrap())).collect();
    let named = |name: &str| heard.iter().filter(|h| h.name() == name).collect::<Vec<_>>();

    let begun = named(names::BEGIN);
    assert_eq!(begun.len(), 1);
    assert_eq!(begun[0].field(names::OUTCOME), "begun");
    assert!(begun[0].has(names::WAITED_US));

    // The plain write's: the rebuilds' own writes are their children
    let writes: Vec<_> = named(names::WRITE).into_iter().filter(|h| h.parent().is_none()).collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].field("durable"), "false");
    assert_eq!(writes[0].field(names::OUTCOME), "committed");
    assert!(writes[0].has(names::WAITED_US) && writes[0].has(names::ELAPSED_US));

    let rebuilds = named(names::REBUILD);
    assert_eq!(rebuilds.len(), 3);
    assert_eq!((rebuilds[0].field("entity_type"), rebuilds[0].field("index")), ("Doc".into(), "tags".into()));
    assert_eq!(rebuilds[0].field(names::OUTCOME), "durable");
    assert_eq!(rebuilds[1].field(names::OUTCOME), "failed");
    assert_eq!(rebuilds[1].field(names::ERROR), "UNKNOWN_INDEX", "named as JavaScript's error.code names it");
    assert_eq!(rebuilds[2].field(names::OUTCOME), "durable");
    // A full rebuild of 200 entities' tags and text takes far longer than a microsecond
    let elapsed = rebuilds[2].field(names::ELAPSED_US).parse::<u64>().unwrap();
    assert!(elapsed > 100, "{}", rebuilds[2].0);

    let sorts = named(names::SORT);
    assert!(
        !sorts.is_empty() && sorts.iter().all(|s| s.parent() == Some(names::REBUILD)),
        "a rebuild's sorts are its own"
    );
    // And by id, each sort of the one-index rebuild under that rebuild, not another of the same name
    let one_index = rebuilds[0].field("id");
    assert!(sorts.iter().any(|s| s.field("parent_id") == one_index), "{one_index}");
    let ids: std::collections::HashSet<String> = heard.iter().map(|h| h.field("id")).collect();
    assert_eq!(ids.len(), heard.len(), "every span its own id");
    assert!(sorts.iter().all(|s| s.field(names::OUTCOME) == "sorted"));
    // One index's entries: eight tags for each of the 200 entities
    assert_eq!(sorts[0].field(names::ROWS), "1600");
    assert!(sorts.iter().all(|s| s.has(names::RUNS) && s.has(names::SPILLED_BYTES)));

    for h in &heard {
        let text = h.0.to_string();
        assert!(!text.contains("Doc-") && !text.contains("secret") && !text.contains("tag0"), "{text} names data");
    }
}
