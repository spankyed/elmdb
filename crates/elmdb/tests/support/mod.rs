//! Deterministic maintenance workloads shared by benchmarks and the isolated heap test.

use std::collections::BTreeMap;

use elmdb::engine::batch::Batch;
use elmdb::engine::maintenance::Load;
use elmdb::engine::plan::Reads;
use elmdb::schema::{SCHEMA_LANGUAGE, Schema};
use elmdb::{Database, OpenOptions};
use rmpv::Value;
use serde_json::json;

pub struct Fixture {
    // Close the database before removing its files, also on Windows.
    pub db: Database,
    pub directory: tempfile::TempDir,
    pub rows: usize,
}

impl Fixture {
    /// Eight 96-byte indexed values per entity, optional text with 16 distinct words per document, and optional
    /// 8 KiB relation payloads. Seeding and initial derivation are outside all measurement windows.
    pub fn new(rows: usize, text: bool, relations: bool) -> Self {
        Self::with_tags(rows, 96, text, relations)
    }

    /// The same, each of the eight values `tag_bytes` long: at 250, just under the longest string a value index holds
    /// (256 bytes), an entity's entries are about 2.4 KB, so a few thousand entities fill a sorter
    pub fn with_tags(rows: usize, tag_bytes: usize, text: bool, relations: bool) -> Self {
        let mut indexes = json!({"tags": {"kind":"value", "fields":["tags"]}});
        if text {
            indexes["text"] = json!({"kind":"text", "fields":{"body":{}}});
        }
        let schema = Schema::from_json(&json!({
            "$schema": SCHEMA_LANGUAGE,
            "types":{"Doc":{"fields":{
                "tags":{"type":"list","of":{"type":"string"}},
                "body":{"type":"string"}
            },"indexes":indexes}},
            "relations":{"refs":{"cardinality":"many-to-many"}}
        }))
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let db =
            Database::open(directory.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() })
                .unwrap();
        let mut loads = Vec::new();
        for i in 0..rows {
            // Letters avoid analyzer-specific numeric splitting and stemming; every word is deterministic.
            let word = |n: usize| {
                let mut n = n;
                let mut out = String::from("word");
                for _ in 0..6 {
                    out.push((b'a' + (n % 26) as u8) as char);
                    n /= 26;
                }
                out
            };
            let body = if text {
                (0..16).map(|j| word(i * 16 + j)).collect::<Vec<_>>().join(" ")
            } else {
                "body ".repeat(200)
            };
            let tags = (0..8).map(|j| Value::from(format!("tag{j:02}{}", "x".repeat(tag_bytes - 5)))).collect();
            let fields =
                BTreeMap::from([("tags".to_owned(), Value::Array(tags)), ("body".to_owned(), Value::from(body))]);
            loads.push(Load::Entity { id: format!("Doc-{i:08}"), fields, trashed: false });
        }
        if relations {
            for i in 0..rows {
                loads.push(Load::Relation {
                    id: format!("relation-{i:08}"),
                    kind: "refs".into(),
                    source: format!("Doc-{i:08}"),
                    target: format!("Doc-{:08}", (i + 1) % rows),
                    info: Some(Value::from("r".repeat(8192))),
                });
            }
        }
        db.bulk_load(&loads).unwrap();
        drop(loads);
        Self { db, directory, rows }
    }

    /// One document an entity, each embedded in a `dims`-dimension vector index the way a caller's worker embeds:
    /// its pending chunks read, and a vector written for each. Seeding and embedding are outside all measurement windows
    // The heap test's alone, where the benchmark and the profiling example share this module
    #[allow(dead_code)]
    pub fn with_vectors(rows: usize, dims: usize) -> Self {
        let schema = Schema::from_json(&json!({
            "$schema": SCHEMA_LANGUAGE,
            "types":{"Doc":{"fields":{"body":{"type":"string"}},"indexes":{
                "meaning":{"kind":"vector", "from":["body"], "chunk":"paragraph", "dims":dims, "model":"m"}
            }}}
        }))
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let db =
            Database::open(directory.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() })
                .unwrap();
        let loads: Vec<Load> = (0..rows)
            .map(|i| Load::Entity {
                id: format!("Doc-{i:08}"),
                fields: BTreeMap::from([("body".to_owned(), Value::from(format!("document {i}")))]),
                trashed: false,
            })
            .collect();
        db.bulk_load(&loads).unwrap();
        let pending: Reads = serde_json::from_value(json!({"reads":[
            {"read":"pendingEmbeddings", "type":"Doc", "index":"meaning", "limit":rows}
        ]}))
        .unwrap();
        let answers: serde_json::Value = rmp_serde::from_slice(&db.read(&pending).unwrap()).unwrap();
        let chunks = answers[0]["chunks"].as_array().unwrap();
        assert_eq!(chunks.len(), rows);
        for (first, batch) in chunks.chunks(1000).enumerate() {
            let ops = batch
                .iter()
                .enumerate()
                .map(|(n, c)| {
                    // A direction of its own for each document, so the graph has neighbours to tell apart
                    let i = first * 1000 + n;
                    let vector: Vec<f32> = (0..dims).map(|d| ((i * dims + d) as f32 * 0.618).sin()).collect();
                    serde_json::from_value(json!({"op":"embed", "id":c["id"], "index":"meaning", "chunk":c["chunk"],
                        "hash":c["hash"], "model":"m", "vector":vector}))
                    .unwrap()
                })
                .collect();
            db.write(&Batch { ops, now: None }, false).unwrap();
        }
        Self { db, directory, rows }
    }

    pub fn assert_limited_query(&self) {
        let bytes = self.db.read(&self.limited_query()).unwrap();
        let answers: serde_json::Value = rmp_serde::from_slice(&bytes).unwrap();
        let expected: Vec<_> = (0..self.rows.min(50)).map(|i| format!("Doc-{i:08}")).collect();
        assert_eq!(answers[0]["ids"], json!(expected));
    }

    pub fn limited_query(&self) -> Reads {
        serde_json::from_value(json!({"reads":[{"read":"query","plan":{
            "start":{"from":"types","types":["Doc"]},
            "steps":[{"step":"where","field":"tags","value":format!("tag00{}", "x".repeat(91))},
                     {"step":"limit","n":50}],
            "terminal":{"answer":"ids"}
        }}]}))
        .unwrap()
    }
}
