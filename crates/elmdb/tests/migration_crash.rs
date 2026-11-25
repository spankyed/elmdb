//! Real subprocess `SIGKILL` crash test during multi-chunk background index building (`d/xbr` progress checkpoints)
//! across value, text, trigram, and vector indexes: reopening resumes every interrupted build cleanly, passes
//! `check().is_clean()`, and matches brute-force ground truth with zero lost or duplicate entries.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use elmdb::engine::answer::{self, Answer};
use elmdb::engine::batch::Batch;
use elmdb::engine::maintenance::Load;
use elmdb::engine::plan::{ReadOp, Reads};
use elmdb::schema::{SCHEMA_LANGUAGE, Schema};
use elmdb::{Database, OpenOptions};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

fn chunk_hash(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Spans 3 chunks of `CHUNK = 1000` entities per index across 4 indexes (12 build chunks plus string order).
const ENTITY_COUNT: usize = 2_200;
/// Every 40th entity (55 total, spread across all 3 chunks) carries a pre-loaded primary embedding so the vector
/// index's background build populates the hannoy HNSW graph across multiple chunks.
const EMBEDDED_STRIDE: usize = 40;

fn plain_schema() -> Schema {
    Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "types": {
            "Doc": {
                "fields": {
                    "cat": { "type": "string", "optional": true },
                    "num": { "type": "number", "optional": true },
                    "title": { "type": "string", "optional": true },
                    "body": { "type": "string", "optional": true }
                }
            }
        }
    }))
    .unwrap()
}

fn indexed_schema() -> Schema {
    Schema::from_json(&json!({
        "$schema": SCHEMA_LANGUAGE,
        "collation": "en",
        "types": {
            "Doc": {
                "fields": {
                    "cat": { "type": "string", "optional": true },
                    "num": { "type": "number", "optional": true },
                    "title": { "type": "string", "optional": true },
                    "body": { "type": "string", "optional": true }
                },
                "indexes": {
                    "byCat": { "kind": "value", "fields": ["cat"] },
                    "text": { "kind": "text", "fields": { "title": { "weight": 2 }, "body": {} } },
                    "grams": { "kind": "trigram", "fields": ["title", "body"] },
                    "meaning": {
                        "kind": "vector",
                        "from": ["title", "body"],
                        "dims": 4,
                        "metric": "cosine",
                        "model": "m"
                    }
                }
            }
        }
    }))
    .unwrap()
}

fn doc_id(i: usize) -> String {
    format!("Doc-{i:04}")
}

fn doc_fields(i: usize) -> (String, String, String) {
    let cat = format!("c{}", i % 10);
    let keyword = if i.is_multiple_of(17) { "lighthouse" } else { "harbor" };
    let title = format!("{keyword} {i}");
    let body = format!("SIG-{:04}", i % 250);
    (cat, title, body)
}

fn embedding_for(i: usize) -> Vec<f32> {
    let theta = (i as f32 / ENTITY_COUNT as f32) * std::f32::consts::FRAC_PI_2;
    vec![theta.cos(), theta.sin(), 0.0, 0.0]
}

fn read_answer(db: &Database, op: Json) -> Answer {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    let bytes = db.read(&reads).unwrap();
    let mut answers = answer::conforms(&bytes).unwrap().0;
    assert_eq!(answers.len(), 1);
    answers.pop().unwrap()
}

fn read_json(db: &Database, op: Json) -> Json {
    let reads = Reads { reads: vec![serde_json::from_value::<ReadOp>(op).unwrap()], track: None };
    let bytes = db.read(&reads).unwrap();
    let mut answers = serde_json::to_value(rmpv::decode::read_value(&mut &bytes[..]).unwrap()).unwrap();
    answers[0].take()
}

#[test]
#[ignore = "invoked only as the child process of sigkill_mid_background_index_build_recovers_cleanly"]
fn child_builds_indexes_and_signals_mid_build() {
    let Ok(path) = std::env::var("ELMDB_SIGKILL_BUILD_DB") else {
        return;
    };
    let db = Database::open(&path, OpenOptions { syncs: false, ..Default::default() }).unwrap();

    // Keep `tracks` non-empty before `set_schema` so every builder chunk starts with key-write logging enabled
    // (otherwise a chunk that began while `tracks` was empty commits with `written = None` and notifies every token
    // conservatively). A `count` read only tracks `d/c`, which index builds never write.
    let keep_logging = Reads {
        reads: vec![
            serde_json::from_value::<ReadOp>(json!({
                "read": "query",
                "plan": { "start": { "from": "types", "types": ["Doc"] }, "terminal": { "answer": "count" } }
            }))
            .unwrap(),
        ],
        track: Some(99),
    };
    db.read(&keep_logging).unwrap();

    // `hear_builder_commits` runs synchronously on the background builder thread right after a chunk commits a key
    // watched by a tracked read. Parking inside it freezes the builder mid-build (after chunk 1 of `meaning` commits
    // `d/qp` + `d/xbr` progress, before chunks 2..3 and `grams` run), so SIGKILL always catches an interrupted build
    // with zero scheduling race.
    db.hear_builder_commits(Some(Arc::new(move |_, _| {
        let mut out = std::io::stdout().lock();
        // Leading newline separates the marker from libtest's `test <name> ... ` prefix under `--nocapture`.
        writeln!(out, "\nchunk-committed").unwrap();
        out.flush().unwrap();
        loop {
            std::thread::park();
        }
    })))
    .unwrap();

    db.set_schema(indexed_schema()).unwrap();

    // Track `pendingEmbeddings` on `meaning` (built third, after `byCat` and `text`): chunk 1 of `meaning` writes
    // `d/qp` keys plus `d/xbr` progress and immediately fires `hear_builder_commits` on the builder thread.
    let track_pending = Reads {
        reads: vec![
            serde_json::from_value::<ReadOp>(json!({ "read": "pendingEmbeddings", "type": "Doc", "limit": 1 }))
                .unwrap(),
        ],
        track: Some(1),
    };
    db.read(&track_pending).unwrap();

    // Park main thread until SIGKILLed by parent (or parent exits and closes stdin).
    let _ = std::io::stdin().read_to_end(&mut Vec::new());
}

#[test]
fn sigkill_mid_background_index_build_recovers_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("db");
    let scratch = dir.path().join("scratch");
    std::fs::create_dir(&db_path).unwrap();
    std::fs::create_dir(&scratch).unwrap();

    // Populate ENTITY_COUNT entities with no indexes yet, plus pre-loaded primary embeddings (`p/q`) spread across
    // all chunks so the vector index build inserts real vectors into the hannoy graph during background chunks.
    let mut embedded_count = 0usize;
    {
        let db =
            Database::open(&db_path, OpenOptions { schema: Some(plain_schema()), syncs: false, ..Default::default() })
                .unwrap();
        let mut embedding_loads = Vec::new();
        for chunk_start in (0..ENTITY_COUNT).step_by(500) {
            let chunk_end = (chunk_start + 500).min(ENTITY_COUNT);
            let mut ops = Vec::with_capacity(chunk_end - chunk_start);
            for i in chunk_start..chunk_end {
                let id = doc_id(i);
                let (cat, title, body) = doc_fields(i);
                if i.is_multiple_of(EMBEDDED_STRIDE) {
                    let chunk_text = format!("{title}\n\n{body}");
                    embedding_loads.push(Load::Embedding {
                        id: id.clone(),
                        index: "meaning".into(),
                        chunk: 0,
                        model: "m".into(),
                        hash: chunk_hash(&chunk_text),
                        vector: embedding_for(i),
                    });
                    embedded_count += 1;
                }
                ops.push(json!({
                    "op": "create",
                    "type": "Doc",
                    "id": id,
                    "fields": { "cat": cat, "num": i, "title": title, "body": body }
                }));
            }
            let batch = Batch {
                ops: ops.into_iter().map(|v| serde_json::from_value(v).unwrap()).collect(),
                now: Some(1_000.0),
            };
            db.write(&batch, false).unwrap();
        }
        db.bulk_load(&embedding_loads).unwrap();
        db.close().unwrap();
    }

    // Spawn child to add all 4 indexes via `set_schema` and SIGKILL it as soon as the first chunk commits.
    let stderr_path = dir.path().join("child-stderr");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "child_builds_indexes_and_signals_mid_build", "--nocapture", "--test-threads=1"])
        .env("ELMDB_SIGKILL_BUILD_DB", &db_path)
        .env("TMPDIR", &scratch)
        .env("TMP", &scratch)
        .env("TEMP", &scratch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&stderr_path).unwrap())
        .spawn()
        .unwrap();

    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.contains("chunk-committed") {
                let _ = tx.send(true);
                return;
            }
        }
        let _ = tx.send(false);
    });

    let signaled = match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(true) => true,
        Ok(false) => false,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            let err_out = std::fs::read_to_string(&stderr_path).unwrap_or_default();
            panic!("timed out waiting for child to signal mid-build:\n{err_out}");
        }
    };
    if !signaled {
        let status = child.wait().unwrap();
        let err_out = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        panic!("child exited ({status}) before signaling mid-build:\n{err_out}");
    }

    // Deliver SIGKILL immediately mid-build and reap child.
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success(), "killed child must not report normal exit");

    // Reopen read-only first to verify the database opens cleanly right after SIGKILL without modifying anything
    // and that the kill genuinely interrupted `meaning` mid-build (chunk 1 committed with `d/xbr`, remaining chunks
    // and `grams` still unbuilt).
    {
        let ro = Database::open(&db_path, OpenOptions { read_only: true, ..Default::default() }).unwrap();
        let count = read_json(
            &ro,
            json!({
                "read": "query",
                "plan": { "start": { "from": "types", "types": ["Doc"] }, "terminal": { "answer": "count" } }
            }),
        );
        assert_eq!(count["n"], json!(ENTITY_COUNT));
        let Answer::Pending { count: pending_mid, .. } =
            read_answer(&ro, json!({ "read": "pendingEmbeddings", "type": "Doc", "limit": 1 }))
        else {
            panic!("pendingEmbeddings did not answer Pending");
        };
        let total_pending = (ENTITY_COUNT - embedded_count) as u64;
        assert!(
            pending_mid > 0 && pending_mid < total_pending,
            "SIGKILL must leave `meaning` partially built with a `d/xbr` checkpoint on disk (got {pending_mid}/{total_pending} pending)"
        );
        let any_index_unbuilt = ro.index_status().unwrap().iter().any(|s| !s.built || s.ordered == Some(false));
        assert!(any_index_unbuilt, "SIGKILL must leave remaining index/order builds unfinished");
        ro.close().unwrap();
    }

    // Reopen writable: background builder resumes from `d/xbr` checkpoints and finishes all 4 indexes cleanly.
    let db = Database::open(&db_path, OpenOptions { syncs: false, ..Default::default() }).unwrap();
    db.wait_for_index_builds().unwrap();
    let report = db.check().unwrap();
    assert!(report.is_clean(), "check after SIGKILL recovery: {report:?}");

    // Verify all indexes are built.
    let statuses = db.index_status().unwrap();
    assert!(statuses.iter().all(|s| s.built && s.failure.is_none()), "all indexes must be built: {statuses:?}");

    // 1. Value index (`byCat`): exact match with zero lost or duplicate entries, using `byCat`.
    let expected_cat3: Vec<String> = (0..ENTITY_COUNT).filter(|i| i % 10 == 3).map(doc_id).collect();
    let cat3_explained = read_json(
        &db,
        json!({
            "read": "query",
            "explain": true,
            "plan": {
                "start": { "from": "types", "types": ["Doc"] },
                "steps": [{ "step": "where", "field": "cat", "value": "c3" }],
                "terminal": { "answer": "ids" }
            }
        }),
    );
    let actual_cat3: Vec<String> =
        cat3_explained["answer"]["ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_owned()).collect();
    assert_eq!(actual_cat3, expected_cat3, "value index must match brute-force ground truth");
    assert!(
        cat3_explained["stages"].as_array().unwrap().iter().any(|s| s.get("index") == Some(&json!("byCat"))),
        "value query must use byCat index"
    );

    // 2. Text index (`text`): exact match against brute-force ground truth (`i.is_multiple_of(17)`).
    let expected_text: BTreeSet<String> = (0..ENTITY_COUNT).filter(|i| i.is_multiple_of(17)).map(doc_id).collect();
    let text_ids = read_json(
        &db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Doc"] },
                "steps": [{ "step": "search", "query": "lighthouse" }],
                "terminal": { "answer": "ids" }
            }
        }),
    );
    let actual_text_vec: Vec<String> =
        text_ids["ids"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_owned()).collect();
    let actual_text_set: BTreeSet<String> = actual_text_vec.iter().cloned().collect();
    assert_eq!(actual_text_vec.len(), actual_text_set.len(), "text search must return no duplicates");
    assert_eq!(actual_text_set, expected_text, "text search must match brute-force ground truth");

    // 3. Trigram index (`grams`): exact `containing` and `matching` against brute-force ground truth.
    let expected_sub: Vec<String> = (0..ENTITY_COUNT).filter(|i| i % 250 == 42).map(doc_id).collect();
    let actual_sub: Vec<String> = read_json(
        &db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Doc"] },
                "steps": [{ "step": "containing", "text": "SIG-0042" }],
                "terminal": { "answer": "ids" }
            }
        }),
    )["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(actual_sub, expected_sub, "trigram containing must match brute-force ground truth");

    let expected_regex: Vec<String> =
        (0..ENTITY_COUNT).filter(|i| i % 250 == 42 || i % 250 == 43).map(doc_id).collect();
    let actual_regex: Vec<String> = read_json(
        &db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Doc"] },
                "steps": [{ "step": "matching", "pattern": r"SIG-00(42|43)" }],
                "terminal": { "answer": "ids" }
            }
        }),
    )["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(actual_regex, expected_regex, "trigram matching must match brute-force ground truth");

    // 4. Vector index (`meaning`): top-10 nearest neighbors match brute-force cosine ground truth, and pending
    // count equals ENTITY_COUNT - embedded_count (proving every chunk was indexed with zero lost/duplicate entries).
    let expected_top10: Vec<String> = (0..10).map(|k| doc_id(k * EMBEDDED_STRIDE)).collect();
    let Answer::Hits { hits: near_hits, .. } = read_answer(
        &db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Doc"] },
                "steps": [{ "step": "near", "vector": [1.0, 0.0, 0.0, 0.0], "k": 10 }],
                "terminal": { "answer": "hits" }
            }
        }),
    ) else {
        panic!("near did not answer hits");
    };
    let actual_top10: Vec<String> = near_hits.iter().map(|h| h.id.clone()).collect();
    assert_eq!(actual_top10, expected_top10, "vector near top-10 must match brute-force cosine ground truth");

    let Answer::Hits { hits: like_hits, .. } = read_answer(
        &db,
        json!({
            "read": "query",
            "plan": {
                "start": { "from": "types", "types": ["Doc"] },
                "steps": [{ "step": "like", "id": "Doc-0000", "k": 3 }],
                "terminal": { "answer": "hits" }
            }
        }),
    ) else {
        panic!("like did not answer hits");
    };
    assert_eq!(like_hits[0].id, doc_id(EMBEDDED_STRIDE), "like Doc-0000 must rank Doc-0040 first");

    let Answer::Pending { count: pending_count, .. } =
        read_answer(&db, json!({ "read": "pendingEmbeddings", "type": "Doc", "limit": 5 }))
    else {
        panic!("pendingEmbeddings did not answer Pending");
    };
    assert_eq!(
        pending_count,
        (ENTITY_COUNT - embedded_count) as u64,
        "every non-embedded entity must be pending with zero lost or duplicate chunks"
    );

    db.close().unwrap();
}
