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
        for (number, batch) in chunks.chunks(1000).enumerate() {
            let ops = batch
                .iter()
                .enumerate()
                .map(|(n, c)| {
                    // A direction of its own for each document, so the graph has neighbours to tell apart
                    let i = number * 1000 + n;
                    let vector: Vec<f32> = (0..dims).map(|d| ((i * dims + d) as f32 * 0.618).sin()).collect();
                    serde_json::from_value(json!({"op":"embed", "id":c["id"], "index":"meaning", "chunk":c["chunk"],
                        "hash":c["hash"], "model":"m", "vector":vector}))
                    .unwrap()
                })
                .collect();
            db.write(&Batch { ops, now: None }, false).unwrap();
        }
        // An embedding of text a chunk no longer holds would be kept as stale rather than refused, so nothing is left
        // pending only once every vector was filed
        let answers: serde_json::Value = rmp_serde::from_slice(&db.read(&pending).unwrap()).unwrap();
        assert_eq!(answers[0]["chunks"].as_array().unwrap().len(), 0, "every chunk is embedded");
        Self { db, directory, rows }
    }

    /// Documents each linked to `degree` others by `refs`, chosen by arithmetic so every run builds the same graph: the
    /// graph algorithms' workload (`docs/roadmap/graph-algorithms.md`). Seeding is outside all measurement windows
    // The benchmarks' and the heap test's, where the profiling example shares this module
    #[allow(dead_code)]
    pub fn graph(rows: usize, degree: usize) -> Self {
        let schema = Schema::from_json(&json!({
            "$schema": SCHEMA_LANGUAGE,
            "types":{"Doc":{"fields":{"body":{"type":"string"}}}},
            "relations":{"refs":{"cardinality":"many-to-many"}}
        }))
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let db =
            Database::open(directory.path(), OpenOptions { schema: Some(schema), syncs: false, ..Default::default() })
                .unwrap();
        let mut loads: Vec<Load> = (0..rows)
            .map(|i| Load::Entity {
                id: format!("Doc-{i:08}"),
                fields: BTreeMap::from([("body".to_owned(), Value::from("d"))]),
                trashed: false,
            })
            .collect();
        for i in 0..rows {
            let mut targets: Vec<usize> = (1..=degree).map(|j| (i * 31 + j * 977) % rows).filter(|&t| t != i).collect();
            targets.sort_unstable();
            targets.dedup();
            for t in targets {
                loads.push(Load::Relation {
                    id: format!("relation-{i:08}-{t:08}"),
                    kind: "refs".into(),
                    source: format!("Doc-{i:08}"),
                    target: format!("Doc-{t:08}"),
                    info: None,
                });
            }
        }
        db.bulk_load(&loads).unwrap();
        Self { db, directory, rows }
    }

    /// A graph read over every document: `rank` (PageRank, or personalized from the first document), the ten
    /// highest as hits, every document's rank counted (`rankCounted`), or the `weakComponents` or `communities` answer
    #[allow(dead_code)]
    pub fn graph_read(&self, read: &str) -> Reads {
        let (steps, terminal) = match read {
            "rank" => (json!([{"step":"rank","kinds":["refs"]},{"step":"limit","n":10}]), json!({"answer":"hits"})),
            "rankCounted" => (json!([{"step":"rank","kinds":["refs"]}]), json!({"answer":"count"})),
            "personalized" => (
                json!([{"step":"rank","kinds":["refs"],"direction":"both","seeds":["Doc-00000000"]},{"step":"limit","n":10}]),
                json!({"answer":"hits"}),
            ),
            "communities" => (json!([]), json!({"answer":"communities","kinds":["refs"]})),
            _ => (json!([]), json!({"answer":"weakComponents","kinds":["refs"],"counts":true})),
        };
        serde_json::from_value(json!({"reads":[{"read":"query","plan":{
            "start":{"from":"types","types":["Doc"]}, "steps":steps, "terminal":terminal
        }}]}))
        .unwrap()
    }

    /// The graph read's answer, checked: ten hits, settled, for a rank, and every document in the weak components or the
    /// communities
    #[allow(dead_code)]
    pub fn assert_graph_read(&self, read: &str) {
        let bytes = self.db.read(&self.graph_read(read)).unwrap();
        let answers: serde_json::Value = rmp_serde::from_slice(&bytes).unwrap();
        let answer = &answers[0];
        if read == "rankCounted" {
            assert_eq!(answer["count"].as_u64().unwrap() as usize, self.rows);
        } else if read == "weakComponents" {
            let total: u64 = answer["weakComponents"].as_array().unwrap().iter().map(|n| n.as_u64().unwrap()).sum();
            assert_eq!(total as usize, self.rows);
        } else if read == "communities" {
            let total: usize =
                answer["communities"].as_array().unwrap().iter().map(|c| c.as_array().unwrap().len()).sum();
            assert_eq!(total, self.rows);
        } else {
            assert_eq!(answer["hits"].as_array().unwrap().len(), 10.min(self.rows));
            assert_eq!(answer["rank"]["converged"], json!(true));
        }
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
