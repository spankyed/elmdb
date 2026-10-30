//! A deterministic message store and the four reads the read benchmark, the read profile and the allocation count
//! share (`docs/roadmap/read-allocation-codec.md`): one field of one entity, a 50-message page, the same page
//! projected to two fields, and every message's row.

use std::collections::BTreeMap;

use elmdb::engine::maintenance::Load;
use elmdb::engine::plan::Reads;
use elmdb::schema::{SCHEMA_LANGUAGE, Schema};
use elmdb::{Database, OpenOptions};
use rmpv::Value;
use serde_json::json;

/// How many messages a channel holds: a page is the first 50 of one
pub const PER_CHANNEL: usize = 100;
/// Version 2 stores integral scores as integers, matching the JavaScript fixture's Number encoding.
pub const FIXTURE_VERSION: u32 = 2;

pub struct Messages {
    // Close the database before removing its files, also on Windows.
    pub db: Database,
    pub directory: tempfile::TempDir,
    pub rows: usize,
}

/// One read the benchmark times: its name, the request, and what its answer must hold
pub struct Workload {
    pub name: &'static str,
    pub reads: Reads,
    check: fn(&serde_json::Value, usize),
}

impl Workload {
    /// The answer, decoded and checked, so a read that answers wrong fails before it is timed
    pub fn assert_answer(&self, db: &Database, rows: usize) {
        let bytes = db.read(&self.reads).unwrap();
        // Through rmpv, which reads a binary value as the list of its bytes where JSON has no bytes
        let answer = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
        (self.check)(&answer[0], rows);
    }
}

impl Messages {
    /// `rows` messages over `rows / 100` channels, each with short strings, a long body that is stored compressed, a
    /// nested map, a list, bytes, a number, a boolean, a field that is null on every third and missing on every
    /// fifth. Seeding and initial derivation happen here, outside any measurement
    pub fn new(rows: usize) -> Self {
        assert!(
            rows >= 8 * PER_CHANNEL && rows.is_multiple_of(PER_CHANNEL),
            "fixture v{FIXTURE_VERSION} requires at least eight whole channels of {PER_CHANNEL}"
        );
        let schema = Schema::from_json(&json!({
            "$schema": SCHEMA_LANGUAGE,
            "types":{"Message":{"fields":{
                "channel":{"type":"string"},
                "title":{"type":"string"},
                "sender":{"type":"string"},
                "body":{"type":"string"},
                "meta":{"type":"object"},
                "tags":{"type":"list","of":{"type":"string"}},
                "attachment":{"type":"bytes"},
                "score":{"type":"number"},
                "read":{"type":"boolean"},
                "note":{"type":"string","optional":true,"nullable":true}
            },"indexes":{"channel":{"kind":"value","fields":["channel"]}}}}
        }))
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let db =
            Database::open(directory.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() })
                .unwrap();
        let loads: Vec<Load> =
            (0..rows).map(|i| Load::Entity { id: id(i), fields: message(i), trashed: false }).collect();
        db.bulk_load(&loads).unwrap();
        Self { db, directory, rows }
    }

    /// The four reads, each with its answer's check
    pub fn workloads(&self) -> [Workload; 4] {
        let page = |terminal: serde_json::Value| -> Reads {
            serde_json::from_value(json!({"reads":[{"read":"query","plan":{
                "start":{"from":"types","types":["Message"]},
                "steps":[{"step":"where","field":"channel","value":"channel-0007"},{"step":"limit","n":50}],
                "terminal":terminal
            }}]}))
            .unwrap()
        };
        [
            Workload {
                name: "field",
                reads: serde_json::from_value(json!({"reads":[{"read":"field","id":id(742),"field":"title"}]}))
                    .unwrap(),
                check: |answer, _| assert_eq!(answer, &json!({"kind":"field","present":true,"value":"message 742"})),
            },
            Workload {
                name: "page",
                reads: page(json!({"answer":"rows"})),
                check: |answer, _| {
                    assert_eq!(answer["kind"], json!("rows"));
                    let rows = answer["rows"].as_array().unwrap();
                    assert_eq!(rows.len(), 50);
                    for (offset, row) in rows.iter().enumerate() {
                        assert_eq!(*row, expected_row(7 * PER_CHANNEL + offset));
                    }
                },
            },
            Workload {
                name: "page_projected",
                reads: page(json!({"answer":"rows","fields":["title","sender"]})),
                check: |answer, _| {
                    assert_eq!(answer["kind"], json!("rows"));
                    let rows = answer["rows"].as_array().unwrap();
                    assert_eq!(rows.len(), 50);
                    for (offset, row) in rows.iter().enumerate() {
                        let i = 7 * PER_CHANNEL + offset;
                        assert_eq!(
                            *row,
                            json!({"id":id(i),"title":format!("message {i}"),"sender":format!("sender-{}", i % 13)})
                        );
                    }
                },
            },
            Workload {
                name: "all_rows",
                reads: serde_json::from_value(json!({"reads":[{"read":"query","plan":{
                    "start":{"from":"types","types":["Message"]},"terminal":{"answer":"rows"}
                }}]}))
                .unwrap(),
                check: |answer, rows| {
                    assert_eq!(answer["kind"], json!("rows"));
                    let found = answer["rows"].as_array().unwrap();
                    assert_eq!(found.len(), rows);
                    for (i, row) in found.iter().enumerate() {
                        assert_eq!(*row, expected_row(i));
                    }
                },
            },
        ]
    }
}

fn id(i: usize) -> String {
    format!("Message-{i:08}")
}

/// About a kilobyte of words from a small vocabulary, so it compresses
fn body(i: usize) -> String {
    const WORDS: [&str; 8] = ["status", "update", "deploy", "review", "merge", "branch", "build", "release"];
    (0..150).map(|j| WORDS[(i + j * 7) % WORDS.len()]).collect::<Vec<_>>().join(" ")
}

fn message(i: usize) -> BTreeMap<String, Value> {
    let mut fields = BTreeMap::from([
        ("channel".to_owned(), Value::from(format!("channel-{:04}", i / PER_CHANNEL))),
        ("title".to_owned(), Value::from(format!("message {i}"))),
        ("sender".to_owned(), Value::from(format!("sender-{}", i % 13))),
        ("body".to_owned(), Value::from(body(i))),
        (
            "meta".to_owned(),
            Value::Map(vec![
                (Value::from("thread"), Value::from(format!("thread-{}", i / 10))),
                (Value::from("depth"), Value::from(i % 4)),
                (Value::from("labels"), Value::Array(vec![Value::from("inbox"), Value::from(format!("l{}", i % 5))])),
            ]),
        ),
        ("tags".to_owned(), Value::Array((0..3).map(|j| Value::from(format!("tag-{}", (i + j) % 17))).collect())),
        ("attachment".to_owned(), Value::Binary((0..64).map(|j| ((i * 31 + j) % 251) as u8).collect())),
        ("score".to_owned(), if i.is_multiple_of(7) { Value::from(i / 7) } else { Value::from(i as f64 / 7.0) }),
        ("read".to_owned(), Value::from(i.is_multiple_of(2))),
    ]);
    if !i.is_multiple_of(5) {
        fields
            .insert("note".to_owned(), if i.is_multiple_of(3) { Value::Nil } else { Value::from(format!("note {i}")) });
    }
    fields
}

/// All values, including bytes, nested maps, missing and null fields, must survive a measured read.
fn expected_row(i: usize) -> serde_json::Value {
    let mut fields = message(i);
    fields.insert("id".to_owned(), Value::from(id(i)));
    fields.insert("entityType".to_owned(), Value::from("Message"));
    serde_json::to_value(Value::Map(fields.into_iter().map(|(k, v)| (Value::from(k), v)).collect())).unwrap()
}
